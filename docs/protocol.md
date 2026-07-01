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
structures in #12 add more, reusing these same messages). Like gather, the **zone**
validates proximity and emits an internal `store_op` to the gateway, which performs
the durable transactional transfer and pushes the updated `inv.update` / `store.update`.

### `build.*` — build orders & placement (M2 §4.3, M3 §4.5)

| type | dir | fields | status |
|---|---|---|---|
| `build.list` | C→S / S→C | C→S: request the district's board. S→C: `orders[]` (`{order_id, kind, required, progress, state}`) — pushed on login and after any unlock, and in reply to a request | **live** |
| `build.contribute` | C→S | `order_id`, `item_id`, `qty` — pool carried items into an order (must be near a build board) | **live** |
| `build.progress` | S→C | `order_id`, `required`, `progress` (each an `{item_id: qty}` map) | **live** |
| `build.completed` | S→C | `order_id`, `structures[]` (`{kind, x, y}`) | **live** |
| `build.unlocked` | S→C | `order_ids[]` — dependents that just opened | **live** |
| `build.place` | C→S | `kind`, `x`, `y`, `rot` | reserved (per-plot player builds, #12) |
| `build.placed` | S→C | `structure` | reserved (#12) |

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

### `plot.*` — plots (M3 §4.4)

| type | dir | fields |
|---|---|---|
| `plot.assigned` | S→C | `plot_id`, `district`, `bounds`, `tier` |
| `plot.info` | C→S | — (current plot details) |

### `skill.*` — use-based skills (M2 §4.6)

| type | dir | fields | status |
|---|---|---|---|
| `skill.update` | S→C | `skill_id`, `xp`, `level` | **live** (sent on login and on XP gain) |
| `skill.levelup` | S→C | `skill_id`, `level` | reserved (#10) |

### `craft.*` / `home.*` — crafting & home (M3 §4.5)

| type | dir | fields |
|---|---|---|
| `home.set_respawn` | C→S | `bed_id` |
| `craft.list` | C→S | — (available recipes) |
| `craft.make` | C→S | `recipe_id` |

### `rent.*` — rent (M4 §4.7)

| type | dir | fields |
|---|---|---|
| `rent.status` | S→C | `plot_id`, `due_at`, `paid_through`, `state` |
| `rent.pay` | C→S | `plot_id` |
| `rent.warning` | S→C | `plot_id`, `due_at` |
| `rent.reclaimed` | S→C | `plot_id`, `moved_to_storage` |

### `district.*` — gated transitions (M4 §4.8)

| type | dir | fields |
|---|---|---|
| `district.enter` | C→S | `from`, `to` |
| `district.ready` | S→C | — (zone loaded; resume control) |

---

## Internal: zone ↔ gateway (unchanged)

Zones self-register (`register_zone`) and exchange `player_join` / `player_leave` /
`spawn_entity` / `move` / `attack` / `migrate_request` / `set_region` / `shutdown` /
`zone_stats` with the gateway. The zone also emits internal messages the gateway
consumes (never forwards) to perform durable writes and push the result:
**`gather_yield`** `{player_id, item_id, qty, skill, xp}` (persist gathered item +
skill XP → `inv.update`/`skill.update`), **`store_op`**
`{player_id, op, item_id, qty}` (transactional inventory↔storage transfer →
`inv.update`/`store.update`), and **`build_contribute`**
`{player_id, order_id, item_id, qty}` (pooled build-order contribution →
`inv.update`/`build.progress`, and on completion `skill.update`/`build.completed`/
`build.unlocked`). Resource nodes, storage points, build boards, and completed
structures are synced to clients as `status_update`s with `state.type` `"resource"` /
`"storage"` / `"build_board"` / `"structure"`. See `proxy.rs` and `zone_server.rs`.

**M0 note on positions.** A returning character is recreated at its exact saved
position via `spawn_entity` to whichever zone owns that point (routed by
`zone_at`); a guest/new player uses `player_join` (the zone picks a spawn point).
The gateway persists each durable character's `(x, y, hp)` periodically and on
disconnect, so logout/restart restores the player where they were.
