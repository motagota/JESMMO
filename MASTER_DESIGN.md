# JESMMO — Master Design

The single reconciled view of what this game **is**, what it **has**, and what
it **does next**. Where a design document and the code disagree, this file
records which one is right and why.

Supersedes the standing-alone status of `MMO.md`, `ECONOMY_DESIGN.md`,
`MARKET_DESIGN.md`, `MINING_POTTERY_DESIGN.md` and `BUSINESS_DESIGN.md`. Those
remain as source material; this is the reconciliation.

Last reconciled: 2026-08-08, against `main` at 399 tests.

---

## 1. What actually exists

Not aspirations. Shipped, tested, and running.

### 1.1 Architecture

```
proxy.rs   (gateway)  ── tokio websocket server, SOLE DURABLE WRITER
   │                     owns SQLite, all game state, all validation
   ├── clients          ws://:8766
   ├── zone registry    ws://:8764
   └── admin            ws://:8767

zone_server.rs (xN)  ── one process per zone, ZERO database access
                        owns simulation and timing only
                        surface zones own a world Region and auto-split
                        interior zones own an authored volume set

client_godot         ── Godot 4.6, a view over server state
```

**There is no actor system.** No `MarketActor`, no `SiteActor`, no registry, no
message-passing concurrency framework. The pattern every feature since #136 has
used is:

> The zone validates range for things it **simulates**; the gateway does the
> durable transaction. Anything with a **panel** rather than a swing is
> range-gated gateway-side from the position cache.

This matters enormously for §3.

### 1.2 Shipped systems

| System | Issue | State |
|---|---|---|
| Terrain: 25.6km real-Brisbane DEM bake, water, deltas, paint tool | #56, #72 | shipped |
| Districts: 5, tiling the **entire** world | — | shipped |
| Plots: leased, rented, reclaimed | #14 | shipped |
| Structures on plots (crafting, storage, market) | #14 | shipped |
| Skills, XP curve, XP falloff | #123, #166 | shipped |
| Instanced tools with durability + repairable husks | #128 | shipped |
| Abilities, hotbar, cooldowns | #120-#130 | shipped |
| Roads as build orders, splines, per-cell progress | #93-#135 | shipped |
| Build orders: contribute items, earn wages | #145 | shipped |
| Market: order book, warehouse, listings, candles, fees | #136-#150 | shipped |
| Second market, NPC provisioner, gold ledger | #151-#156 | shipped |
| Wild dogs, weapon slot, pelt bounty | #157-#163 | shipped |
| Interior zones + portals | #165 | shipped |
| Deposits, contested seams, Mining | #166 | shipped |
| Stations, fuel, timed jobs, escrow, Smelting | #167 | shipped |
| Pottery, shaping failure, catalyst mechanism | #168 | shipped |
| Conditional handouts + tutorial track | #169 | shipped |
| Mine balance, bulk recipes, faucet ceiling | #170 | shipped |

### 1.3 The economy as built

- **Gold only.** No copper, no silver. A character starts with 500.
- **Faucets:** build wages (~25 g/min), dog bounty (~100 g/min), NPC provisioner
  floor (mints on demand — the only unbounded one).
- **Sinks:** market listing fees, sale tax, warehouse storage fees, station
  usage fees. All burned through `gold_ledger`.
- **The identity:** `SUM(gold_ledger.amount) == purses + escrow`. Asserted by
  `gold_supply_gap()`, checked in tests across whole production chains.
- **Config:** `market.toml`, `zones.toml`, `crafting.toml`, `tutorial.toml`,
  `terrain.toml`. Strict loader — missing file falls back to shipped defaults,
  malformed refuses the boot naming the key.

---

## 2. Reconciling BUSINESS_DESIGN.md

Point by point, against the code.

### 2.1 Already true — no work needed

**§5 "There is no global exchange."** This is the document's self-declared most
important decision, and **it is already how the game works.** Orders are keyed
by `market_id`; #151 shipped a second market with its own book, its own
district-scoped config, and its own fee rates. Nothing to build. The argument in
§5.1 is a correct defence of an existing choice and should be moved into
`market.toml` as a comment so nobody "optimises" it later.

**§6 Offline production with fuel.** Shipped in #167. `station_fuel` holds a
shared buffer, `requires_presence = false` jobs run while the owner is offline,
and `ready_at` is absolute so a restart resumes rather than pauses. The doc's
tick pseudocode describes what already runs.

**§P2 "No teleporting goods."** Already satisfied, twice over: there is no
teleport of any kind, and `MAX_CARRY` is a hard 50. The 8.6km Milton Road haul
(#100) exists precisely because geography is expensive.

**§8 anti-automation, in part.** The doc proposes diminishing per-character
regional yield. #170 solved the same problem differently and better: contested
shared nodes cap total output by **seam count and respawn**, not player count —
about five miners saturate the mine and the sixth just makes everyone slower.
That ceiling is tested. Diminishing yield would be a second, weaker mechanism
layered on a working one. **Recommend dropping it.**

### 2.2 Already exists under a different name

**§3 "Sites" are `Plot`s.** This is the largest finding in the document.

| Doc's Site | Existing `Plot` |
|---|---|
| leased, not owned | ✔ `owner_character_id`, lease semantics |
| region-scoped | ✔ `district` |
| rent per period | ✔ `rent_due_at`, `rent_paid_through`, `auto_pay` |
| lease state machine | ✔ `active → lapsed → reclaimed` |
| slots holding stations | ✔ `structure` rows with `kind` |
| storage | ✔ warehouse (`market_warehouse_item`, per market per character, with locked/available, storage fees and arrears) |

What Plots genuinely lack: **access policy, a roster, and fees charged to other
players.** That is the actual new work in §3 — perhaps a fifth of what the
document implies.

The doc's four-state machine (`leased → arrears → derelict → vacant`) is a
strictly better version of the three-state one that exists, chiefly because it
never deletes player property — a recovery vault instead. **Adopt the extra
state and the vault; keep the existing table.**

### 2.3 Wrong, and why

**§10 The actor architecture does not exist and should not be built.**
`SiteActor`, `LeaseActor`, `MarketRegistry`, "no cross-actor locks", "single
direction of message flow, no distributed transaction" — this describes a system
this project does not have. The gateway is a single process holding one SQLite
connection with WAL; a durable multi-step operation is *a transaction*, which is
simpler and stronger than the two-actor handshake §10.2 proposes.

The §10.2 haul settlement is a hand-rolled two-phase commit with a "pending
sub-compartment" to survive lost messages. In the real architecture that entire
paragraph collapses to: `BEGIN; ... COMMIT;`.

**Adopt the intent, discard the mechanism.** Every "actor" in §10 becomes a
module in `proxy.rs` plus a table. The rent ticker (#14), the order-expiry sweep
(#140), the candle rollup (#143), the provisioner refresh (#154) and the station
job sweep (#167) are all already "the LeaseActor pattern" — a `tokio::spawn`ed
loop on a slow timer.

**§5.2 / §9.4 The geography is wrong.** There is no Ipswich, Redcliffe,
Moreton Bay or North Pine. The 25.6km bake **is** Brisbane, and five districts —
`market`, `suburbs`, `civic`, `craftworks`, `old_quarter` — tile it exactly, with
a test proving no gaps or overlaps. There is no room for neighbouring towns
inside the world as baked.

So `regions.toml` should not be created. **Districts already are the regions**,
and they already carry per-district market config. If the world ever needs
Ipswich, that is a new bake, not a config file.

**§3.1 `wharf` and `zone_restriction = "coastal"`.** Water exists (sea level,
water mask, drowning) but there is no coastal POI concept, no boats, no ferries,
and no fishing. A wharf class is three unbuilt systems wearing one hat. Defer.

**§9 The TOML is over-specified.** Five new files, where the project's own
pattern is few files with wide scope — `market.toml` already outgrew its name
and holds fees, the provisioner and the bounty. Site classes and policy defaults
belong in **one** new file; service orders belong in `market.toml` beside the
fees they charge.

### 2.4 Genuinely new, and worth building

1. **Access policy and fees charged to other players.** Nothing in the game lets
   one player charge another for anything. This is the actual capital layer.
2. **`fee_in_kind`.** The best idea in the document. Keeps gold out of the loop,
   pays the owner in goods they must then sell, and is legible without a
   spreadsheet. Note §13.5's own worry is real — 1-in-5 pots and 1-in-5 swords
   are very different — so it needs a per-recipe cap or a value ceiling.
3. **Roster with expiring grants.** Small, and the expiry is the point.
4. **Service orders.** `supply` is close to an existing build order; `craft` and
   `clear` are new; `haul` is Phase 2 by the doc's own reasoning.
5. **Reputation as four raw counters, no score.** Cheap, and the refusal to
   synthesise a rating is correct.
6. **Recovery vault**, so lapsing never destroys goods.

---

## 3. Corrected architecture

What §10 becomes in the architecture that exists.

```
proxy.rs
 ├── site module          policy, roster, fee application
 │                        (a module + tables, NOT an actor)
 ├── lease_monitor()      tokio loop, slow timer — extends rent_monitor (#14)
 ├── service_orders       posting, escrow, claim, settle, expire
 │                        (extends the market order path)
 └── existing market      already partitioned by market_id

SQLite (WAL, 1 connection)
 ├── plot            (exists — gains policy columns)
 ├── structure       (exists)
 ├── plot_grant      NEW — roster
 ├── service_order   NEW — or a `kind` column on market_order
 ├── reputation      NEW — four counters
 └── recovery_vault  NEW
```

**Settlement is a transaction, not a message.** Where §10.2 needs a pending
sub-compartment and a `HaulDelivered` message to avoid duplication, the real
system opens a transaction, moves the goods, releases the escrow, and commits.
The compare-and-clear pattern (#142, reused in #167 and #169) already handles
concurrent claims correctly.

---

## 4. Design pillars, reconciled

The document's five pillars, checked against what the game already demonstrates.

**P1 — Friction is the product.** Keep, unchanged. It is the right test, and the
mine already passes it: the furnace is public, but ore is capped by geography.

**P2 — No teleporting goods.** Keep, and note it is already enforced.

**P3 — Businesses are operated, not owned passively.** Keep. #167's fuel buffer
is exactly this mechanism, already working: a furnace with no charcoal does
nothing, and charcoal must be carried there by someone.

**P4 — Ownership is temporary.** Keep. Already true of plots, and the recovery
vault makes it humane.

**P5 — Legitimate density, not crime.** Keep. Worth stating loudly because it is
the pillar most likely to be quietly abandoned later.

**P6 — NEW: the capped faucet.** Every gold source must have a ceiling that does
not scale with player count, and every ceiling must be a *test*. #170 established
this and it is the property most easily destroyed by a well-meaning change.

---

## 5. What to build, in order

Each is one epic. Sequenced so every step is testable at small population.

### Epic A — Plots become businesses
The capital layer, on the table that already exists.

- Access policy on `plot`: `mode`, `fee_gp`, `fee_pct`, `fee_in_kind`, `skill_floor`
- `plot_grant` roster, four roles, all expiring
- Fees charged when a non-owner uses a station on someone's plot
- `fee_in_kind` with a per-recipe value cap (§13.5's worry, answered)
- Lease state machine gains `derelict` + recovery vault
- Client: lease board, management panel

**Prerequisite this exposes:** stations are currently authored world fixtures in
`zones.toml`, owned by nobody. Player-owned stations need a station instance
attached to a `structure` row. That is the real first task.

### Epic B — Service orders
Work for gold, on the escrow machinery that exists.

- `supply` and `craft` first — closest to build orders
- `clear` (labour hire) after
- Reputation counters, displayed raw
- Seed NPC orders to establish price floors, as #154 did for commodities

### Epic C — Regional price signal
The books are already separate; make the separation *visible*.

- Market board shows other districts' **stale, timestamped** prices
- Courier tick publishing snapshots
- No live cross-district view, ever

This is small and high-value: it turns an existing invisible property into a
playable one.

### Epic D — Haul, and the wilds
Deferred, correctly, by the document itself. Needs contested zones first.

---

## 6. Open questions, answered where possible

| § | Question | Answer |
|---|---|---|
| 13.1 | Sites transferable? | Yes, at the landlord NPC, flat fee, remaining term intact. The doc's own leaning is right and the fee is a sink. |
| 13.2 | Employees paid offline? | Yes. #167 already establishes that jobs complete and output waits; payment should follow the same rule. |
| 13.3 | Site cap per player? | Hard cap of 2 in Phase 1. Escalating rent is more elegant and, as the doc says, much harder to tune — and #170 showed that tuning without live data is guesswork. |
| 13.4 | Courier staleness 6h? | **Unanswerable from the armchair, and that is fine.** #170 established the pattern: ship a number, instrument it, read `gold_by_reason` and the candle history, then set it. Do not argue about it first. |
| 13.5 | `fee_in_kind` on high-value outputs? | Real problem. Cap the in-kind take by *value*, not count — take 1 in 5 pots or the gold equivalent of one pot, whichever is smaller. |

---

## 7. Standing lessons

Earned in this codebase, applicable to everything above.

**Config that nothing reads is worse than no config.** Epic #164 shipped three
fields — `swing_time_ms`, `requires_presence`, `required_level` — that were
declared, documented, tunable, and read absolutely nowhere. Each looked fine.
Before shipping a field, grep for a reader.

**A level gate of 1 locks out everyone.** `level_for_xp(0)` is 0, so 1 already
means "you have done this before". Hit twice, in #167 and #170.

**Balance numbers must be measured, not derived.** #170's ore price moved from 4
to 5 because two live runs measured 3.2 ore/min against a predicted 9.1. The
yield chance was correct; the *pacing* was not. Arithmetic could not have found
that.

**Live probes find what unit tests cannot.** Dogs drifting 1250 units. Two
stations 40 units apart with 40-unit radii, where server and client silently
disagreed about where the player stood. A `null` on the wire where a client
default does not apply. Every epic should end with a probe against running
servers.

**Verify a PR's contents before merging.** A `git checkout <ref> -- .` staged a
reversion that a later `--amend` committed, and PR #174 merged with 11 of 12
files missing. Check `changedFiles` and re-verify `main` afterwards.

---

## 8. Non-goals

Stated so they stop being re-proposed.

- A global exchange. Ever.
- Actor-framework rewrite. The gateway plus SQLite transactions is the model.
- Player-placed sites in Phase 1. Authored placement keeps the map designed.
- Crime, theft, or griefing loops as content (P5).
- CAPTCHA, click-timing, or any anti-bot measure that punishes low APM.
- Thirst, food, and stamina — repeatedly proposed, repeatedly deferred, and
  every item justified by them has been deferred with them.
