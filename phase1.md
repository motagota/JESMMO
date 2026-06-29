# Phase 1 — The Capital (MVP) Implementation Plan

*Companion to the design doc. This document turns the Phase 1 roadmap into a concrete,
buildable engineering plan: what we have, what we must add, and the order to build it in.*

Goal: **a complete, playable city-builder MMO core with zero PvP** — an empty safe
capital that players physically grow by gathering resources and fulfilling build orders,
with starter plots, basic homes, use-based skills, and a rent cycle — running on a proper
3D Godot client.

---

## 0. Where we are today (honest baseline)

The current `rust_server` is **not** a thin demo — it's a genuinely capable distributed
simulation backbone. We keep it and build on it. But it contains **zero** of the Phase 1
gameplay and **zero** persistence.

### What already exists and is reusable

| Capability | Where | Phase 1 relevance |
|---|---|---|
| Client gateway / routing | [proxy.rs](rust_server/src/bin/proxy.rs) | **Core.** Keep as the front door for all player connections. |
| Spatial world partition + position-based handoff | proxy `zone_at`, `handle_migrate_request` | **Core.** This *is* our zone/streaming system. |
| Seamless migration, split/merge auto-scaling, rolling updates | proxy `autoscale_monitor`, `rolling_update`, `merge_zones` | **Core infra.** Lets the capital scale with population. |
| Authoritative 20 Hz tick + entity sync | [zone_server.rs](rust_server/src/bin/zone_server.rs) `game_loop` | **Core.** The simulation heartbeat we hang gameplay off. |
| JSON-over-WebSocket protocol | both binaries | Keep the transport; **evolve** the message set. |
| Load/bot tooling | `bin/bots.rs`, `bin/loadtest.rs` | Keep for capacity testing. |
| 2D canvas client | [client.html](client/client.html) | **Throwaway** for gameplay, but invaluable as a debug/admin view. |

### What is missing (the entire Phase 1 feature set)

- **Persistence** — nothing survives a restart. No DB, no save/load. This is the single
  biggest gap and the precondition for everything else (a plot, a skill, or a rent timer
  is meaningless if it evaporates on restart).
- **Identity** — `player_id` is a random UUID minted per connection. No accounts, no login,
  no "this is the same person who logged in yesterday."
- **The Capital** — there is no concept of a safe hub, districts, or a buildable city. The
  world is an abstract 1200×1200 square full of mobs.
- **Gameplay** — no resources, gathering, inventory, build orders, structures, plots, homes,
  crafting, skills, or rent. The only "gameplay" is melee vs. wandering mobs (a *wilds*
  mechanic that Phase 1 explicitly excludes).

### Strategic implication

The combat/mob/territory-capture code is Phase 2 (wilds) material. For Phase 1 we **flag it
off** in the capital (zero PvP, safe zone) and **repurpose the mob/entity scaffolding** into
gatherable resource nodes and NPC props. We do not delete it — it moves to the wilds zone type later.

---

## 1. Target architecture for Phase 1

```
                 ┌──────────────────────────────────────────────┐
                 │                Godot 3D Client                │
                 │   (render, input, prediction, UI, build mode) │
                 └───────────────┬──────────────────────────────┘
                                 │  WebSocket (JSON now → binary later)
                 ┌───────────────▼──────────────┐
                 │           Gateway              │   ← proxy.rs, extended
                 │  auth handshake · session ·    │
                 │  partition · migration · scale │
                 └──┬───────────────┬─────────────┘
        ┌───────────▼───┐    ┌──────▼─────────┐         ┌──────────────────────┐
        │  Zone server  │    │  Zone server   │   …     │   Persistence layer  │
        │ (capital dist)│    │ (capital dist) │         │  Postgres + Redis    │
        │  sim + gameplay│   │  sim + gameplay│◄───────►│  accounts, plots,    │
        └────────────────┘   └────────────────┘         │  skills, inventory,  │
                 ▲                                       │  structures, rent    │
                 │   shared world state via DB/cache     └──────────────────────┘
                 └───────────────────────────────────────────────┘
```

**Key additions to the existing topology:**

1. **A persistence layer** (new). Postgres for durable state, Redis for hot/shared state and
   pub/sub between zones. Zones become *stateless-ish* simulators that load on demand and
   write through to the DB.
2. **An identity/auth service** (new, can live inside the gateway for MVP). Login → session
   token → stable `account_id` / `character_id` that replaces the per-connection UUID.
3. **A world-content layer** (new). The capital is authored data: districts (= zones), road
   graph, plot grid, build-order definitions, resource-node spawns, recipe/skill tables.
4. **Gameplay systems inside the zone server** (new modules): inventory, gathering, building,
   plots, skills, rent.

We are deliberately keeping the gateway/zone split, the position-based partition, and the
20 Hz tick. Those are the hard parts and they already work.

### Technology choices (recommended defaults — change if you have reason)

| Concern | Choice | Why |
|---|---|---|
| Server language | **Stay Rust** | The hard infra is already written in it and performs. |
| Durable DB | **Postgres** (via `sqlx`, async) | Relational data (accounts, plots, rent, inventory) fits SQL; `sqlx` is async-native for tokio. |
| Hot/shared state + pub/sub | **Redis** | Cross-zone messaging, session lookup, rate limits, ephemeral presence. |
| Dev DB | **SQLite** (same `sqlx` code path) | Zero-setup local iteration; migrate to Postgres for staging. |
| Client | **Godot 4.x, 3D, GDScript** (+ GDExtension/Rust later if hot paths demand) | Matches the design doc; fast iteration; good WebSocket support. |
| Wire format | **JSON now, add a binary codec later** | Keep JSON for velocity; the protocol is already JSON. Binary is a Phase 1.5 optimization. |
| Schema migrations | **`sqlx migrate` / `refinery`** | Versioned, reviewable DB changes from day one. |

---

## 2. Foundational work (must land before gameplay)

These three pieces are prerequisites for nearly every feature checkbox. Build them first.

### 2.1 Persistence layer

**New crate / module:** `rust_server/src/persistence/` (a shared library crate that both
`proxy` and `zone_server` depend on), or a new `persistence` lib target in `Cargo.toml`.

Responsibilities:
- Connection pool management (`sqlx::PgPool`).
- Schema + migrations (initial migration creates the tables below).
- Typed repository functions: `load_character`, `save_character`, `load_plot`, `claim_plot`,
  `apply_rent_tick`, `grant_skill_xp`, `add_to_inventory`, etc.
- A **write-through / write-behind** policy: gameplay mutates in-memory zone state for
  responsiveness, and a background flusher persists dirty entities every N seconds and on
  clean shutdown / migration / logout.

**Initial schema (v1):**

```sql
account(id, email, pw_hash, created_at, last_login)
character(id, account_id, name, x, y, hp, current_district, created_at, last_seen)
skill(character_id, skill_id, xp, level)                       -- use-based progression
inventory_item(id, character_id, item_id, qty, slot)           -- carried resources/goods
storage_item(id, character_id, item_id, qty)                   -- safe stash (home storage)
plot(id, owner_character_id, district, grid_x, grid_y, w, h,
     tier, rent_due_at, rent_paid_through, state)              -- state: active|lapsed|reclaimed
structure(id, plot_id, kind, x, y, rot, hp, built_by, data)    -- bed/storage/crafting/walls/etc
flair(id, owner_character_id, plot_id, item_id, x, y, rot)     -- décor, always protected
build_order(id, district, kind, required_json, progress_json,
            state, issued_at, completed_at)                    -- city build quests
resource_node(id, district, item_id, x, y, qty, respawn_at)    -- gatherables (may be cache-only)
```

**Acceptance:** a character can log out and back in (even across a server restart) and find
their position, skills, inventory, storage, and plot exactly as they left them.

### 2.2 Identity & sessions (accounts, login)

**Where:** an auth handshake in the gateway ([proxy.rs](rust_server/src/bin/proxy.rs)
`handle_client`), backed by the `account`/`character` tables.

Today `handle_client` mints `let player_id = Uuid::new_v4()` on connect and immediately joins
a zone. We change the connect flow to:

1. Client connects → gateway sends `auth_required`.
2. Client sends `login {email, password}` (or `register`, or a dev `guest` mode).
3. Gateway verifies against `account`, issues a **session token**, loads the character row,
   and uses the **persistent `character_id`** as the entity id everywhere the code currently
   uses the random UUID.
4. Reconnect with a valid token resumes the same character (and, if still within a grace
   window, the same zone/position).

**Notes**
- Password hashing: `argon2`. Never store plaintext. (MVP can start with a guest/dev path,
  but the table and hashing land now.)
- The gateway already has the right seam — every place that reads `player_id` keeps working;
  we just change *where the id comes from* and make it durable.
- Keep one **active character per account** for Phase 1 (multi-character is deferred).

**Acceptance:** two browser tabs logging into the same account collapse to one session; an
unknown account is rejected; a returning account lands as the same character.

### 2.3 Protocol evolution & versioning

The current protocol is a flat set of JSON `{type, ...}` messages. We:
- Add a `protocol_version` to the `welcome`/handshake so client and server can refuse
  mismatches cleanly.
- Group new gameplay messages by domain prefix for clarity: `inv.*`, `build.*`, `plot.*`,
  `skill.*`, `gather.*`, `rent.*`.
- Keep the existing movement/status/partition messages as-is (they work).

A living **protocol reference** lives at `docs/protocol.md` and is updated with every new
message. (See §5 for the new messages each feature introduces.)

---

## 3. World content: defining the Capital

The design's capital is **~40 km² split across multiple districts**, each a
simulation/streaming boundary. Our existing partition already models exactly this — a zone
owns a `Region` (rectangle) of the world and the gateway routes by position. We promote that
from an abstract square into an **authored city**.

### 3.1 Districts = zones

- Re-interpret each zone's `Region` as a **named capital district** (e.g. Market District,
  Starter Suburbs, Civic Centre).
- For MVP, **gated transitions** (design's choice): crossing a district boundary triggers a
  brief load/handoff. The seamless `migrate_request` path already does the handoff; the
  client just shows a transition curtain instead of streaming continuously.
- Auto-scaling still applies *within* a busy district (split a crowded Market District), but
  district *identity* (its name, its plot grid, its build orders) is authored data keyed to
  a region, independent of how many sim processes back it.

### 3.2 The capital starts empty

- The world authoring defines the **ground plane, road graph, district boundaries, and plot
  grid** — but **no buildings**. Every structure in `structure` starts absent.
- A "town centre" anchor (spawn point + the first build-order board) is the only fixed prop.
- As build orders complete and players build homes, `structure` rows accumulate and the city
  visibly fills in — realizing the "watch the capital physically grow" pillar.

### 3.3 Safety / zero-PvP enforcement

- Tag each zone with a `safety: safe | wilds` flag. Capital districts are `safe`.
- In `safe` zones the zone server **disables** player-vs-player damage and (for Phase 1)
  mob aggression toward players. Resource nodes and friendly wildlife may exist; nothing
  loots or kills a player in the capital.

**Acceptance:** a fresh server boots an empty, navigable, named, multi-district capital with
a spawn point and a build-order board, and no player can take damage there.

---

## 4. Feature-by-feature implementation

Each subsection maps to a Phase 1 roadmap checkbox. Format: **data → server → client →
protocol → acceptance.**

### 4.1 Resource gathering (the quest loop's input)

The raw activity that feeds build orders and levels gathering skills.

- **Data:** `resource_node` rows per district (trees, stone, ore). Items defined in a static
  `item` table/registry (id, name, stack size, category).
- **Server (zone):** spawn nodes from authored data at tick start (reuse the mob-spawn
  scaffolding in `zone_server.rs`). A `gather` action: validate range to node, check/advance
  a swing timer (reuse the `attack_cooldown` pattern), decrement node `qty`, add the item to
  the player's `inventory_item`, grant gathering-skill XP, and schedule node `respawn_at`.
- **Client:** highlight nearby nodes, "gather" interaction (hold-to-gather progress bar),
  inventory updates, floating "+N wood" feedback.
- **Protocol:** `gather.start {node_id}`, `gather.progress {node_id, pct}`,
  `gather.result {item_id, qty}`, `inv.update {items}`, `node.depleted/node.respawn`.
- **Acceptance:** a player walks to a tree, gathers wood over a few seconds, sees their
  inventory and gathering skill rise, the node depletes and later respawns.

### 4.2 Inventory & storage

Carried items vs. the safe home stash — also the substrate for rent's "belongings → storage."

- **Data:** `inventory_item` (carried, finite slots) and `storage_item` (home stash, large).
- **Server (zone):** authoritative inventory ops (add/remove/move/split/stack), capacity
  limits, and **transfer to storage** only when standing at the player's home `storage`
  structure. All ops write through to the DB.
- **Client:** inventory panel, storage panel (opens when near home storage), drag/drop.
- **Protocol:** `inv.update`, `inv.move {from, to}`, `store.deposit/withdraw {item_id, qty}`.
- **Acceptance:** items gathered appear in inventory; at home storage a player can deposit
  items that then persist safely and don't count against carry capacity.

### 4.3 Build orders (the quest content + city growth)

The king/policy issues orders; players fulfil them with gathered resources; the city grows
and unlocks more.

- **Data:** `build_order` rows (kind, `required_json` = item costs, `progress_json` =
  contributed-so-far, state, prerequisites). An authored **tech tree** of orders: completing
  the Town Well unlocks Wall Section, which unlocks Market Stall, etc.
- **Server (district-owned, persisted):** a build order belongs to a district, not a single
  player — contributions are pooled. A `contribute` action moves items from inventory into
  the order's progress. When `progress >= required`, the order **completes**: spawn the
  corresponding `structure`(s) into the world, grant building-skill XP to contributors,
  and unlock dependent orders. The gateway (or a dedicated "city" service) is the natural
  authority since orders are district-scoped and shared; it persists progress through the DB.
- **Client:** a **build-order board** UI (open at the town centre): list of active orders,
  their costs, current progress, and a "contribute" flow from inventory. The structure
  visibly appears in the world on completion.
- **Protocol:** `build.list`, `build.contribute {order_id, item_id, qty}`,
  `build.progress {order_id, progress}`, `build.completed {order_id, structures}`,
  `build.unlocked {order_ids}`.
- **Acceptance:** players pool wood/stone into the "Town Well" order; when it fills, a well
  model appears at the town centre, contributors gain building XP, and a new order unlocks.
  This is the headline Phase 1 demo.

### 4.4 Starter plot on arrival

Every new player gets a generous plot in the Starter Suburbs.

- **Data:** the district's plot grid is authored (`plot` rows pre-seeded as `unowned`, small
  tier per the design's "plots start small"). On first login, allocate a free starter plot
  and set `owner_character_id`.
- **Server:** plot allocation service (gateway/city authority): find the next free starter
  plot, assign it, set initial `rent_due_at`. Idempotent so reconnect doesn't re-grant.
- **Client:** on first spawn, a "here's your plot" moment — camera framing, plot outline
  rendered, a marker pointing home.
- **Protocol:** `plot.assigned {plot_id, district, bounds, tier}`, `plot.info`.
- **Acceptance:** a brand-new character is given a distinct, outlined plot near the capital
  and can find their way back to it.

### 4.5 Basic player home: bed, storage, crafting station

The three starter structures that make a plot a home.

- **Data:** `structure` rows of kind `bed | storage | crafting`, owned via their `plot_id`.
- **Server (zone):** a **build/place mode** scoped to the player's own plot — validate the
  placement is inside owned plot bounds, not overlapping, on valid ground; create the
  `structure`; persist. **Bed** registers the plot as the player's respawn anchor (replaces
  the current "respawn at random region point" in `game_loop`). **Storage** unlocks the
  storage panel (§4.2). **Crafting station** unlocks recipes (§4.6 light crafting).
- **Client:** build/place mode — ghost preview, snap-to-grid, rotate, confirm. Interactions:
  sleep/set-respawn at bed, open storage, open crafting.
- **Protocol:** `build.place {kind, x, y, rot}`, `build.placed {structure}`,
  `home.set_respawn {bed_id}`, `craft.list`, `craft.make {recipe_id}`.
- **Acceptance:** on their plot a player places a bed (and respawns there after a fall/logout),
  a storage chest (deposits persist), and a crafting station (can craft a basic item).

### 4.6 Use-based skills, no decay

Skills rise by doing; permanent; higher skill unlocks better builds/designs.

- **Data:** `skill` rows per character (gathering, crafting, building, and a combat skill
  reserved for the wilds). XP → level via a fixed curve.
- **Server:** every relevant action (§4.1 gather, §4.3 contribute/build, §4.5 craft) calls a
  single `grant_skill_xp(character, skill, amount)` that updates in-memory state and writes
  through. **No decay timer anywhere** — that's a deliberate non-feature. Level thresholds
  gate which build orders / structure tiers / recipes are available.
- **Client:** a skills panel (levels, progress bars), level-up feedback, and gating shown in
  build/craft menus ("requires Building 3").
- **Protocol:** `skill.update {skill_id, xp, level}`, `skill.levelup {skill_id, level}`.
- **Acceptance:** repeatedly gathering raises the gathering skill and never falls; reaching a
  threshold unlocks a previously greyed-out structure or recipe.

### 4.7 Rent system (the land sink)

Land is rented; belongings and flair are owned and protected. Lapse → reclaim land,
belongings to storage, flair protected.

- **Data:** `plot.rent_due_at`, `rent_paid_through`, `state (active|lapsed|reclaimed)`;
  a currency balance on `character` (a simple gold field for MVP).
- **Server (city/gateway authority + scheduled job):** a periodic **rent ticker** (cron-like
  task, analogous to `autoscale_monitor`'s interval loop) that, per plot:
  - on `pay`/auto-pay, advances `rent_paid_through` and deducts currency;
  - when `now > rent_due_at` → mark `lapsed` (grace period);
  - when grace elapses → **reclaim**: move all `structure` belongings/contents and the
    player's on-plot `inventory` into their `storage` (safe), **preserve all `flair`**
    (never destroyed — it was purchased), set plot `unowned`/`reclaimed` so it re-enters the
    pool. The character keeps everything they owned; they just lose the *land*.
- **Client:** rent status on the plot/home UI (paid-through date, "pay rent" button),
  warnings as due date approaches, and a clear "your plot lapsed; your belongings are safe in
  storage" message.
- **Protocol:** `rent.status {plot_id, due_at, paid_through, state}`, `rent.pay {plot_id}`,
  `rent.warning`, `rent.reclaimed {plot_id, moved_to_storage}`.
- **Open decision (track in design doc #1):** where reclaimed-rent currency goes — gold sink
  (delete it) vs. city treasury. For Phase 1, **sink it** (simplest, deflationary, no
  contestable city ownership needed yet). Revisit in Phase 2.
- **Acceptance:** let a test plot's rent lapse → the plot returns to the pool, the player's
  items are intact in storage, their flair is untouched, and another player can claim the plot.

### 4.8 Gated zone transitions

District-to-district movement with a brief load (design's MVP choice over seamless streaming).

- **Server:** already implemented as position-based handoff (`handle_migrate_request`). Add a
  `transition` signal so the client knows to show a curtain rather than expecting continuous
  streaming, plus a load/ready handshake (`zone_ready`) before fully handing control over.
- **Client:** at a district gate, show a transition screen, request the handoff, load the new
  district's content/props, ack `zone_ready`, then resume.
- **Protocol:** existing `zone_migration` + new `district.enter {from, to}` /
  `district.ready`.
- **Acceptance:** walking through a district gate shows a brief, clean transition and lands
  the player in the next district at the correct position with that district's content loaded.

---

## 5. The Godot 3D client

Replace the 2D canvas with a real 3D client. The 2D `client.html` stays as a **debug/admin
spectator** (top-down partition view is genuinely useful for ops).

### 5.1 Project structure

```
client_godot/
  project.godot
  net/        NetworkClient.gd   (WebSocket, JSON codec, message dispatch)
  net/        Protocol.gd        (message type constants, mirrors docs/protocol.md)
  world/      World.gd           (district loader, ground, roads, props)
  world/      EntityManager.gd   (spawn/despawn/interp other players, nodes, structures)
  player/     LocalPlayer.gd     (input, client-side prediction + reconciliation)
  build/      BuildMode.gd       (ghost placement, snap-to-grid, plot-bounds validation)
  ui/         (inventory, storage, build-order board, skills, rent, plot HUD)
  scenes/     (district scenes, structure models, resource-node models)
```

### 5.2 Networking & state sync

- WebSocket to the gateway (`ws://…:8766`), JSON to start (matches server).
- Login/handshake flow (§2.2), then receive `welcome` + `partition` + initial state.
- **Interpolation** of remote entities from `status_update` snapshots (server is 20 Hz; the
  client renders at display rate and interpolates between snapshots).
- **Client-side prediction + reconciliation** for the local player's movement, so input feels
  instant while the server stays authoritative (it already validates and clamps movement).

### 5.3 Rendering & gameplay UX

- 3D ground per district, road graph, and **structures spawned from `structure` data** so the
  city you see is the city the server has persisted (empty at first, growing over time).
- Resource nodes as interactable 3D props; gather progress bars.
- **Build mode**: ghost model, grid snap, rotate, plot-boundary highlighting, valid/invalid
  tint, confirm → send `build.place`.
- Full UI suite: inventory, storage, build-order board, skills, rent/plot HUD, the
  "here's your plot" onboarding moment.
- Camera: third-person 3D (orbit + follow); a top-down toggle helps for build mode.

### 5.4 Acceptance

A player launches the Godot client, logs in, spawns in the 3D capital, walks to a tree and
gathers, opens the build-order board and contributes, watches a structure appear, returns to
their plot, places a bed/storage/crafting station, and sees their skills rise — all persisted.

---

## 6. Cross-cutting concerns

- **Authority & anti-cheat:** the server stays authoritative for movement, gathering,
  inventory, building, and rent. The client never asserts results — it requests, the server
  validates (range, ownership, cost, cooldown) and responds. The gateway already refuses to
  trust client-supplied ids (`data["player_id"] = json!(player_id)`); extend that discipline
  to every new action.
- **Persistence consistency:** write-through for high-value events (claim plot, place
  structure, complete build order, pay rent), write-behind (periodic flush) for high-frequency
  state (position, partial gather progress). Flush on logout, migration, and graceful shutdown.
- **Shared/district-scoped state:** build orders, plot ownership, and rent are *not* owned by
  a single zone process (zones split/merge/restart). They live in the DB and are mediated by a
  city authority (start it inside the gateway; extract to its own service if it gets hot).
- **Migration safety:** the existing rolling-update/split/merge paths cache only
  `(x, y, hp)`. Extend the cached/transferred per-entity payload to include the
  gameplay-relevant handle (`character_id`, carried inventory ref) so a mid-session zone
  restart never drops gameplay state. Anything durable should also be in the DB as the source
  of truth.
- **Observability:** extend the admin UI/snapshot with gameplay counters (active plots, open
  build orders, rent reclaims/day, DB write latency) alongside the existing zone/pop/dropped-
  frame stats.
- **Testing:** keep the bot/loadtest harness; teach bots to gather + contribute so we can load-
  test the *gameplay* loop, not just connection density. Add DB integration tests for rent
  reclaim and plot lifecycle (the riskiest correctness areas).

---

## 7. Milestones & sequencing

Ordered so every milestone is demoable and each unblocks the next.

### M0 — Foundations (no visible gameplay)
- Persistence crate + Postgres/SQLite, initial schema + migrations.
- Auth handshake, accounts, sessions; durable `character_id` replaces per-connection UUID.
- Protocol versioning + `docs/protocol.md`.
- **Demo:** log in, move around the existing world, log out and back in to the same position.

### M1 — The empty capital + Godot client skeleton
- World authoring: districts, road graph, plot grid, spawn/town-centre; `safe` zone flag
  (disable combat in the capital).
- Godot client: connect, login, render one 3D district, move with prediction, see other
  players.
- **Demo:** walk around an empty, safe, named 3D capital with other players.

### M2 — The core loop (gather → build order → city grows)
- Resource nodes + gathering + inventory.
- Build orders (board UI, contribute, completion spawns a structure) + building/gathering XP.
- Skills panel + use-based progression, no decay.
- **Demo (headline):** players gather wood, fill the Town Well order, and watch the well
  appear in the capital.

### M3 — Home & land
- Starter plot allocation on first login.
- Build/place mode for bed (respawn) / storage (safe stash) / crafting station.
- Storage deposit/withdraw; basic crafting at the station.
- **Demo:** new player gets a plot, builds a home, respawns at their bed, stashes items.

### M4 — Rent & polish
- Rent ticker, pay/auto-pay, lapse → reclaim (belongings to storage, flair protected).
- Gated district transitions with load curtain.
- Admin/gameplay observability; gameplay-aware bots; rent/plot integration tests.
- **Demo:** full Phase 1 loop end-to-end, including a plot lapsing safely and recirculating.

**Phase 1 is "done" when** all eight roadmap checkboxes pass their acceptance criteria on the
Godot client against a persistent backend, with zero PvP in the capital.

---

## 8. Risks & watch-items

- **Persistence is load-bearing and new** — the project currently has none. Getting the
  write-through/write-behind boundary right (responsiveness vs. durability) is the main
  engineering risk. Start simple (write-through everything), optimize later.
- **District-scoped state vs. ephemeral zones** — zones split/merge/restart freely today;
  build orders, plots, and rent must not live solely in a zone process. The DB-as-truth +
  city-authority pattern (§6) addresses this but must be honored everywhere.
- **Scope of "build mode"** — placement validation (bounds, overlap, ground, ownership) is
  fiddly; keep the first version grid-snapped and forgiving.
- **Godot from scratch** — the 3D client is net-new work; the 2D debug client de-risks the
  protocol so client work can proceed against a known-good server.
- **Repurposing combat code** — make sure the `safe` flag fully disables player damage in the
  capital before any playtest, or zero-PvP is violated.

## 9. Open decisions (carried from the design doc)

1. **Reclaimed rent → sink vs. city treasury.** Phase 1 recommendation: **sink it.** Revisit
   when cities become contestable (Phase 2).
2. **Bed-as-respawn when homes are raidable.** N/A in Phase 1 (capital is safe); design now,
   honor in Phase 2.
3. **Settlement leadership model.** Out of scope for Phase 1 (capital is admin-run).
4. **Working title.** Doesn't block engineering; pick before any public build.
