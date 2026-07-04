# Wire Protocol

JSON-over-WebSocket. Every message is a JSON object with a `"type"` field. The
gateway (proxy) is the front door for clients; zones are internal peers.

- **Client endpoint:** `ws://127.0.0.1:8766`
- **Zone registration:** `ws://127.0.0.1:8764`
- **Admin UI:** `ws://127.0.0.1:8767`

**Protocol version:** `1` (see `mmo::protocol::PROTOCOL_VERSION`). The gateway
advertises it in `auth_required` and echoes it in `welcome`; bump it on any
incompatible change. A client **may** include `protocol_version` in its handshake
frame: if present and it does not match the gateway's, the connection is refused
with `auth_error` (`"protocol version mismatch: server N, client M"`) and closed —
retrying won't help a version skew. Clients that omit the field (the load-test
bots, the legacy 2D client) are accepted unchanged.

---

## Connection handshake (M0)

On connect, the gateway sends `auth_required` and waits for the client to
authenticate before assigning an identity or spawning it into the world.

```
client connects
  └─► server: auth_required           { protocol_version }
client authenticates (one of):
        register                       { email, password, name }
        login                          { email, password }
        token                          { token }            # resume a session
        guest                                               # ephemeral, not persisted
  ├─ on failure ─► server: auth_error  { message }          # client may retry (cap 5)
  └─ on success:
        server: auth_ok                { player_id, name, token }   # persistent only
        server: welcome                { player_id, zone, protocol_version, name }
        server: partition              { ... }
        (player is spawned into its zone)
```

**Backward compatibility.** A client that sends a gameplay frame (e.g. `move`)
instead of authenticating is treated as a **guest**, and that first frame is still
processed. This keeps the load-test bots and any older client working unchanged.

**One session per character.** A second login for a character that is already
online is rejected with `auth_error: "this character is already online"`.

### Client → server (handshake)

| type | fields | notes |
|---|---|---|
| `register` | `email`, `password`, `name` | creates account + starter character, returns a token |
| `login` | `email`, `password` | returns the account's character (with saved position) + token |
| `token` | `token` | resume a session issued earlier this gateway run (in-memory) |
| `guest` | — | ephemeral `guest_*` character; not persisted |

Any handshake frame above may also carry an optional `protocol_version`; a
mismatch is refused (see **Protocol version** at the top).

### Server → client (handshake)

| type | fields | notes |
|---|---|---|
| `auth_required` | `protocol_version` | first frame after connect |
| `auth_ok` | `player_id`, `name`, `token` | persistent logins only; client stores `token` |
| `auth_error` | `message` | human-readable; client may retry |
| `welcome` | `player_id`, `zone`, `protocol_version`, `name` | identity assigned; world join begins |

---

## Gameplay (existing)

### Client → server

| type | fields | notes |
|---|---|---|
| `move` | `dx`, `dy` | request a step; server validates/clamps and is authoritative |
| `attack` | `dx`, `dy` | flag a melee swing in a facing direction |

Any client-supplied `player_id` is ignored — the gateway stamps the real one.

### Server → client

| type | fields | notes |
|---|---|---|
| `welcome` | see above | |
| `partition` | `world`, `zones[]` | world size + each zone's region/owner/progress/`district`/`safety` (`safe`\|`wilds`); drives the map |
| `status_update` | `player_id`, `state{x,y,hp,max_hp,type,facing}`, `zone` | entity snapshot (~20 Hz) |
| `despawn` | `player_id` | stop rendering an entity (death/disconnect) |
| `zone_migration` | `zone` | the player's authoritative zone changed (seamless handoff) |
| `zone_capture` | `zone_id`, `owner`, `progress` | territory-control state (wilds mechanic; off in safe zones later) |
| `you_died` | `player_id` | local death feedback |

---

## The Capital (world content)

The world is the authored **Capital** (`mmo::world::capital()`): a 1200×1200 space
tiled into three named districts as vertical bands, all **safe** (zero-PvP) for
Phase 1. The capital starts **empty** — authored ground, roads, and a plot grid,
but no buildings; structures appear only as players complete build orders / build
homes.

| district | id | region `[x0,x1) × [y0,y1)` | notes |
|---|---|---|---|
| Market District | `market` | `[0,400) × [0,1200)` | |
| Civic Centre | `civic` | `[400,800) × [0,1200)` | town centre + first build-order board |
| Starter Suburbs | `suburbs` | `[800,1200) × [0,1200)` | starter plot grid (3×8 = 24 plots) |

- **Town centre / spawn:** world centre `(600, 600)`, inside the Civic Centre.
- **District identity is keyed to world geometry**, not to sim processes. The
  gateway labels each shard's `district` and `safety` in `partition` by its region
  centre, so the capital stays correctly named however the world is split/merged.
- **Safe-zone enforcement (zero-PvP):** a zone whose region is a `safe` district
  disables mob aggression and **never applies damage to a player**, and the
  territory-capture (wilds) mechanic is off. Regions outside the authored capital
  default to `wilds` (Phase 2). The zone re-evaluates safety from its current
  region each tick, so a split that moves it is honored immediately.
- **Seeded on boot (idempotent):** the starter plot grid (as `unowned` plots) and
  the first build order (`town_well`, Civic Centre). A restart never duplicates them.

---

## Reserved gameplay messages (Phase 1)

New gameplay messages are grouped by a **domain prefix** so the wire stays
self-describing as features land. The names below are **reserved now** (a single
source of truth in `mmo::protocol`, mirrored by the Godot client's `Protocol.gd`);
the gateway/zone handlers that act on them arrive with their milestones, so until
then sending one is a no-op. Direction: **C→S** client to server, **S→C** server
to client.

### `gather.*` — resource gathering (M2 §4.1) — **live**

| type | dir | fields |
|---|---|---|
| `gather.start` | C→S | `node_id` — begin gathering a node in range |
| `gather.stop` | C→S | — (optional; gathering also stops on depletion or walking away) |
| `gather.progress` | S→C | `node_id`, `pct` (0–100 for the current unit) |
| `gather.result` | S→C | `item_id`, `qty` (one unit yielded; floating feedback) |
| `node.depleted` / `node.respawn` | S→C | reserved — node lifecycle currently rides the entity sync (see below) |

**Resource nodes** are synced as ordinary entities: a `status_update` with
`state.type = "resource"` (and `item_id`, `qty`) spawns/updates the node on the
client; a `despawn` removes a depleted one; a later `status_update` brings it back
on respawn. So `node.depleted`/`node.respawn` stay reserved for richer client FX.

The gather loop is server-authoritative: the **zone** validates range and runs the
swing timer; each completed unit decrements the node, emits `gather.result`, and
sends an internal `gather_yield` to the gateway, which persists the item + XP and
pushes the authoritative `inv.update` / `skill.update`. Gathering persists only for
logged-in characters (guests see `gather.result` feedback but nothing is saved).

### `inv.*` / `store.*` — inventory & storage (M2 §4.2)

| type | dir | fields | status |
|---|---|---|---|
| `inv.update` | S→C | `items[]` (`{item_id, qty, slot}`), `used`, `capacity` — full carried inventory + carry usage | **live** (login, gather, deposit/withdraw) |
| `store.update` | S→C | `items[]` (`{item_id, qty}`) — full safe-storage contents | **live** (login, deposit/withdraw) |
| `store.deposit` | C→S | `item_id`, `qty` — move carried → storage (must be near a storage point) | **live** |
| `store.withdraw` | C→S | `item_id`, `qty` — move storage → carried (bounded by capacity) | **live** |
| `inv.move` | C→S | `from`, `to` | reserved (slot drag/drop, later) |

Carried inventory has a finite **carry capacity** (`MAX_CARRY`); storage is unbounded
and does **not** count against it. Gathering stops yielding into a full inventory;
depositing frees it. Deposit/withdraw are gated server-side on standing near a
**storage point** (an authored town storehouse in M2; per-plot home `storage`
structures in #12/#13 add more, reusing these same messages). Like gather, the
**zone** validates proximity — to the town storehouse, or to a specific placed home
chest (#13; the gateway pushes the zone every home structure's position, since the
zone has no DB access — see `home_structures_sync`/`home_structure_added` below) —
and emits an internal `store_op` to the gateway, which performs the durable
transactional transfer and pushes the updated `inv.update` / `store.update`.

### `build.*` — build orders & placement (M2 §4.3, M3 §4.5)

| type | dir | fields | status |
|---|---|---|---|
| `build.list` | C→S / S→C | C→S: request the district's board. S→C: `orders[]` (`{order_id, kind, required, progress, state, required_skill, required_level}`) — pushed on login and after any unlock, and in reply to a request. `required_level` 0 = ungated; otherwise the client greys the order until the player reaches `required_skill` level `required_level` (the server enforces the same gate on `build.contribute`) | **live** |
| `build.contribute` | C→S | `order_id`, `item_id`, `qty` — pool carried items into an order (must be near a build board) | **live** |
| `build.progress` | S→C | `order_id`, `required`, `progress` (each an `{item_id: qty}` map) | **live** |
| `build.completed` | S→C | `order_id`, `structures[]` (`{kind, x, y}`) | **live** |
| `build.unlocked` | S→C | `order_ids[]` — dependents that just opened | **live** |
| `build.place` | C→S | `kind` (`bed`\|`storage`\|`crafting`), `x`, `y`, `rot` — place a home structure on your own plot | **live** |
| `build.placed` | S→C | `structure` (`{id, plot_id, kind, x, y, rot, built_by}`) — ack once placement succeeds | **live** |

City build orders are **district-scoped and gateway-owned** (pooled across every zone
sharding that district). Each order has item **costs** (`required`) that contributions
(`progress`) fill; a **tech tree** gates dependents behind a prerequisite order (seeded
`locked`, opened on the prerequisite's completion). Like gather/store, the **zone**
validates that the player is standing near a **build board** and emits an internal
`build_contribute` op to the gateway, which performs the durable pooled contribution
(bounded by the order's remaining need and what's carried) and pushes the results. On
completion the order flips to `completed`, each **contributor** is granted **building**
XP (lump-sum, split by units contributed), the authored **structure** spawns, and any
dependents unlock. Completed city structures are durable via the `build_order` row
itself (no `structure` row in Phase 1) and render as `status_update` entities.
Contributing persists only for logged-in characters (guests are a no-op).

**Home structures (#12)** split validation the same way: the **zone** only checks
*geometry* — is the target point on some authored plot cell at all? — and forwards an
internal `build_place`; the **gateway** alone knows *ownership* (whose plot is this?)
and durable state, so it resolves the caller's own plot, validates the kind and its
fixed footprint (`bed`/`crafting` 20×20, `storage` 16×16) fully inside the plot bounds
with no overlap against structures already there, then persists and pushes
`build.placed` plus a `status_update` broadcast to the whole district (so neighbours
see new homes appear). Multiple structures of each kind are allowed per plot. Home
structures render with their **own** kind as `state.type` (not `"structure"`), so a
home `storage` chest transparently reuses the same rendering/proximity plumbing as the
authored town storehouse — see `store.*` below.

### `plot.*` — plots (M3 §4.4)

| type | dir | fields | status |
|---|---|---|---|
| `plot.assigned` | S→C | `plot_id`, `district`, `bounds` (`{x,y,w,h}`), `tier`, `just_claimed` — pushed on login (allocating a starter plot on a brand-new character) and in reply to `plot.info` | **live** |
| `plot.info` | C→S | — (re-sends the character's current plot as `plot.assigned`) | **live** |

Every district that authors a **plot grid** (currently just the Suburbs, 3×8 = 24
starter plots) is pre-seeded with `unowned` plots on boot. On login the gateway
**idempotently** allocates a character's first free plot in that district (a
reconnect just re-sends the same one — `just_claimed` distinguishes the very first
grant, which drives the client's one-time "here's your plot" moment, from a re-send).
`bounds` is the plot's world-space rect, letting the client draw a distinct outlined
landmark and a compass reading back to it. Guests hold no land. Rent *enforcement*
(lapse/reclaim, #14) acts on `rent_due_at`/`rent_paid_through` seeded here — see
`rent.*` below.

### `skill.*` — use-based skills (M2 §4.6)

| type | dir | fields | status |
|---|---|---|---|
| `skill.update` | S→C | `skill_id`, `xp`, `level` | **live** (sent on login and on XP gain) |
| `skill.levelup` | S→C | `skill_id`, `level` — fired alongside `skill.update` when an XP grant crosses a level boundary | **live** |

### `craft.*` / `home.*` — crafting & home (M3 §4.5)

| type | dir | fields | status |
|---|---|---|---|
| `home.set_respawn` | C→S | `bed_id` — must name a `bed`-kind structure on the caller's own plot | **live** |
| `home.respawn_set` | S→C | `bed_id` — ack once validated | **live** |
| `craft.list` | C→S | — (request the recipe registry) | **live** |
| `craft.recipes` | S→C | `recipes[]` (`{id, name, inputs: [{item_id, qty}], output_item, output_qty}`) | **live** |
| `craft.make` | C→S | `recipe_id` — must be standing near a `crafting`-kind structure (#13) that's on your own plot | **live** |
| `craft.made` | S→C | `recipe_id`, `item_id`, `qty` — feedback once the craft succeeds (`inv.update` and a `crafting` `skill.update` follow separately) | **live** |

The starter recipes (`mmo::world::recipes()`): `plank` (2 wood → 2 plank) and
`tool_kit` (1 wood + 1 stone → 1 tool_kit). Crafting is instant (no timer) and
atomic — `craft.make` either succeeds (ingredients debited, output credited, flat
`crafting`-skill XP granted per craft — `CRAFT_XP_PER_CRAFT`) or is a silent no-op
(not near a station, unknown recipe, insufficient ingredients); there's no error
protocol surface, matching `store.deposit`/`build.contribute`'s convention.

**Proximity (#13).** Both `store.deposit`/`store.withdraw` at a home chest and
`craft.make` require standing near the *specific* placed structure — not just
anywhere on the plot (#12's original, looser scope). Since the zone has no DB
access, it can't look up where structures are on its own; the gateway pushes their
positions down (see `home_structures_sync`/`home_structure_added` below), and the
zone gates purely on that cached geometry. Ownership/durable state (whose plot is
this, do the ingredients check out) stays gateway-side either way.

**Bed-based respawn.** A character's respawn point is whichever bed they last set
via `home.set_respawn` (`character.respawn_structure_id`); with none set, death
falls back to the default town-centre spawn. Since a death can happen in one zone
while the respawn point belongs to another, the zone doesn't respawn the player
itself — it reports the death (`player_died`, see below) and the gateway resolves
the destination and hands off exactly like a `migrate_request`.

### `rent.*` — rent (M4 §4.7)

| type | dir | fields | status |
|---|---|---|---|
| `rent.status` | S→C | `plot_id`, `due_at`, `paid_through`, `state`, `auto_pay`, `gold` — pushed on login and after any rent-affecting action (pay, auto-pay toggle, a ticker-driven pay/lapse) | **live** |
| `rent.pay` | C→S | `plot_id` — deduct `RENT_COST_GOLD` and extend by one rent period; must own the plot and afford it | **live** |
| `rent.warning` | S→C | `plot_id`, `due_at` — fired once per due cycle, `RENT_WARNING_LEAD_SECS` before `due_at` | **live** |
| `rent.reclaimed` | S→C | `plot_id`, `moved_to_storage` (always `[]` — see below) | **live** |
| `rent.set_autopay` | C→S | `plot_id`, `enabled` — opt-in per plot, off by default | **live** |

A background **rent ticker** (`Proxy::rent_monitor`, every `RENT_TICK_INTERVAL`)
sweeps every owned plot regardless of whether its owner is currently connected —
pushes are best-effort (silently dropped if offline; the DB row is the durable
source of truth, picked up on next login via the hydration push above). Per plot,
per tick: if **auto-pay** is enabled and the plot is due, try to pay from gold
first (silently falls through to the lapse path below if unaffordable); otherwise,
within `RENT_WARNING_LEAD_SECS` of `due_at`, send one `rent.warning` (tracked by
`plot.warned`, cleared on payment, so it fires exactly once per cycle); otherwise
advance the durable state machine (`active → lapsed` past due, `lapsed → reclaimed`
past a grace period) via `Db::apply_rent_tick`.

**Reclaim.** Structures on a reclaimed plot are **demolished** (deleted, and
despawned client-side) — they belong to the land, which is what's being taken
back. **Flair is preserved**, just unattached (`plot_id` set to `NULL`) — it's
owned by the character, not the land, and is never destroyed. `moved_to_storage`
is genuinely always empty: home storage is a single **character-global** stash
(#12/#13, not plot-scoped), so nothing was ever at risk there to begin with. If
the former owner's respawn pointed at a demolished bed, that reference is cleared
(falls back to the default spawn). **Currency is sunk**, not refunded or handed to
a city treasury (Phase 1's open-decision #1 — see `phase1.md` §9).

There is no earning mechanic yet in Phase 1 — every character starts with a flat
`STARTING_GOLD` balance (the `character.gold` migration column default).

### `district.*` — gated transitions (M4 §4.8)

| type | dir | fields | status |
|---|---|---|---|
| `district.enter` | C→S | `from`, `to` — the client announces it crossed a district gate | **live** |
| `district.ready` | S→C | — (district-scoped content refreshed; resume control) | **live** |

The actual position/zone handoff is unrelated to this handshake and already
happens seamlessly via the existing `migrate_request`/`zone_migration` machinery
(#4) the moment a player's position crosses *any* zone-region boundary, district
gate or not. `district.*` is purely the **client-facing transition curtain**:
the client already knows every zone's district from `partition`, so it detects a
district crossing itself (comparing its live position against the district
tiles it's already drawing), shows a brief transition screen, and sends
`district.enter` — the gateway refreshes district-scoped content for wherever
the player actually now is (currently just the build board, `build.list`; other
per-character state like inventory/skills/plot/rent isn't district-scoped and
needs no refresh) and acks `district.ready`, so the client drops the curtain.
There's no real "loading" work server-side in Phase 1 (the client enforces a
minimum curtain duration itself so an instant round-trip doesn't just flash).

---

## Internal: zone ↔ gateway

Zones self-register (`register_zone`) and exchange `player_join` / `player_leave` /
`spawn_entity` / `move` / `attack` / `migrate_request` / `set_region` / `shutdown` /
`zone_stats` with the gateway. The zone also emits internal messages the gateway
consumes (never forwards) to perform durable writes and push the result:
**`gather_yield`** `{player_id, item_id, qty, skill, xp}` (persist gathered item +
skill XP → `inv.update`/`skill.update`), **`store_op`**
`{player_id, op, item_id, qty}` (transactional inventory↔storage transfer →
`inv.update`/`store.update`), **`build_contribute`**
`{player_id, order_id, item_id, qty}` (pooled build-order contribution →
`inv.update`/`build.progress`, and on completion `skill.update`/`build.completed`/
`build.unlocked`), **`build_place`** `{player_id, kind, x, y, rot}` (the zone only
confirmed the target is on *some* plot; the gateway resolves ownership/bounds/overlap
→ `build.placed`), **`craft_make`** `{player_id, recipe_id}` (the zone only confirmed
the player is standing near *a* crafting station; the gateway confirms it's on their
own plot and attempts the craft → `inv.update`/`craft.made`/`skill.update`), and
**`player_died`** `{player_id, hp}` (the zone removed the dead player from its own map
rather than respawning them in place; the gateway resolves the respawn point — a set
bed, or the default spawn — and relocates them exactly like a `migrate_request`,
since the point may belong to a different zone).

The gateway also pushes **down** to zones, since a zone has no DB access and can't
otherwise know where placed home structures are: **`home_structures_sync`**
`{structures: [{id, kind, x, y}]}` (full replace — sent on zone registration and
whenever its region changes, i.e. split/merge), **`home_structure_added`**
`{id, kind, x, y}` (one newly-placed structure, sent the moment `build_place`
succeeds), and **`home_structure_removed`** `{id}` (a rent reclaim demolished it,
#14). The zone caches these purely as geometry (kind + position) to gate
`store.deposit`/`store.withdraw`/`craft.make` on proximity to the *specific*
structure (#13) — it never learns or needs to know who owns it.

Resource nodes, storage points, build boards, completed city structures, and home
structures are synced to clients as `status_update`s with `state.type` `"resource"` /
`"storage"` / `"build_board"` / `"structure"` (city) / `"bed"` / `"storage"` /
`"crafting"` (home — note a home storage chest deliberately shares `"storage"` with
the authored town storehouse, so it reuses the same rendering and proximity
plumbing). See `proxy.rs` and `zone_server.rs`.

**M0 note on positions.** A returning character is recreated at its exact saved
position via `spawn_entity` to whichever zone owns that point (routed by
`zone_at`); a guest/new player uses `player_join` (the zone picks a spawn point).
The gateway persists each durable character's `(x, y, hp)` periodically and on
disconnect, so logout/restart restores the player where they were.
