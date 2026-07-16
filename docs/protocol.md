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

The world is the authored **Capital** (`mmo::world::capital()`): a 25600×25600
space (~655 km², the near-full-extent Brisbane DEM — world v3) tiled into five
named districts in a plus/cross layout, all **safe** (zero-PvP) for Phase 1. The
capital starts **empty** — authored ground, roads, and a plot grid, but no
buildings; structures appear only as players complete build orders / build homes.

| district | id | region `[x0,x1) × [y0,y1)` | notes |
|---|---|---|---|
| Starter Suburbs | `suburbs` | `[0,6400) × [0,25600)` | west band; starter plot grid (12×20 = 240 plots) |
| Market District | `market` | `[19200,25600) × [0,25600)` | east band (reaches the river mouth / bay) |
| Civic Centre | `civic` | `[6400,19200) × [6400,19200)` | centre; town centre + first build-order board |
| Craftworks Quarter | `craftworks` | `[6400,19200) × [0,6400)` | north band |
| Old Quarter | `old_quarter` | `[6400,19200) × [19200,25600)` | south band |

- **Town centre / spawn:** world centre `(12800, 12800)`, inside the Civic Centre.
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
| `plot.district` | S→C | `plots: [{plot_id, bounds, owner_id, owner_name, tier}]` — every plot in the requester's *current* district, owned or not (`owner_id`/`owner_name` are `null` for a still-free plot) | **live** (#18) |
| `plot.district` | C→S | — (re-sends the current district's roster) | **live** (#18) |

Every district that authors a **plot grid** (currently just the Suburbs, 12×20 =
240 starter plots) is pre-seeded with `unowned` plots on boot. On login the gateway
**idempotently** allocates a character's first free plot in that district (a
reconnect just re-sends the same one — `just_claimed` distinguishes the very first
grant, which drives the client's one-time "here's your plot" moment, from a re-send).
`bounds` is the plot's world-space rect, letting the client draw a distinct outlined
landmark and a compass reading back to it. Guests hold no land. Rent *enforcement*
(lapse/reclaim, #14) acts on `rent_due_at`/`rent_paid_through` seeded here — see
`rent.*` below.

`plot.district` (#18) is the district-wide counterpart: rather than just your own
plot, it lists every plot in whichever district you're currently standing in, so the
client can render everyone's land (own plot styled distinctly; others show the
owner's name if taken, or read as free/claimable). Pushed on login, on a
`district.enter` crossing, and in reply to an explicit request; also **broadcast**
to everyone already in the district whenever a plot changes hands (a new claim or a
rent reclaim), so it doesn't go stale until someone's next login/district-crossing.

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

### `terrain.*` — cosmetic ground heightmap (#54, #63) + native-res tile streaming

Purely visual: the server has no other concept of elevation, and every
gameplay position stays 2D (`x`, `y`). Heights are loaded once at boot from
the baked terrain artifact (`terrain-bake`/`terrain-common`, the terrain
pipeline epic #56 — see the repo-root `terrain.toml`) rather than generated
in-process, and are static for the whole session, so the coarse grid is
requested once (like `craft.list`) rather than pushed proactively or folded
into `partition` (which is rebroadcast on every zone split/merge/capture —
too frequent for a several-KB static payload). The coarse wire grid's
resolution (`mmo::world::TERRAIN_RESOLUTION`) is deliberately decoupled from
the baked artifact's own internal tile/cell resolution — the server samples
`terrain_common::Terrain::sample_height` at a fixed `(resolution+1)^2` grid
regardless of how detailed the underlying bake is. That coarse grid is the
permanent, always-present **backdrop**; genuinely native-resolution terrain
streams in separately, per baked tile, on demand (terrain streaming): the
client requests individual tiles around the player as they move
(`client_godot/world/TerrainStreamer.gd`) and frees them once left behind.
Tile requests are stateless and idempotent server-side — no per-connection
bookkeeping, same posture as `terrain.list`; a request for a tile outside
the manifest's grid is silently ignored.

| type | dir | fields | status |
|---|---|---|---|
| `terrain.list` | C→S | — (request the authored heightmap grid) | **live** |
| `terrain.data` | S→C | `resolution` (grid cells per axis), `world_size`, `heights` (`(resolution+1)^2` floats, row-major/y-major: `heights[gy*(resolution+1)+gx]`, in the same units as world x/y) — plus the baked artifact's own manifest shape for tile streaming: `tile_size` (cells per tile side), `tiles` (`[cols, rows]`), `cell_size_m`, `height_min_m`, `height_max_m` (the u16 sample decode range) | **live** |
| `terrain.tile_request` | C→S | `tx`, `ty` (tile-grid coordinate) | **live** |
| `terrain.tile_data` | S→C | `tx`, `ty`, `side` (`tile_size + 1` corner samples per side), `encoding` (`"tile_v1"`), `data_b64` — exactly `terrain_common::HeightTile::encode(1)`'s bytes, base64-wrapped: a 16-byte header (magic `TRHT`, u16 LE format_version, u16 reserved, i32 LE tile_x, i32 LE tile_y) then `side²` u16 LE samples, decoded to meters via `height_min_m`/`height_max_m` | **live** |
| `terrain.delta_request` | C→S | `tx`, `ty` (chunk = the same tile-grid coordinate) — request the chunk's hand-authored edit layer (terrain-editing epic #72) | **live** |
| `terrain.delta_data` | S→C | `tx`, `ty`, `has_delta`; when `has_delta` is true also `revision` (monotonic per chunk), `encoding` (`"delta_v1"`), `data_b64` — `terrain_common::SparseHeightDelta::encode(1)`'s bytes, base64-wrapped: an 8-byte header (magic `TRHD`, u16 LE format_version, u16 reserved), a block-presence bitmap (`ceil(ceil(side/16)²/64)` u64 LE words), then each present 16×16 block's 256 i16 LE **centimeter offsets** in ascending block-index order. Composited client-side onto the corresponding streamed tile's corner heights *before* mesh build. Unlike `terrain.tile_request`, an in-range chunk **always** answers (`has_delta: false` when unedited) so the client never confuses "no answer yet" with "nothing here"; out-of-range stays silently ignored | **live** |
| `terrain.edit_op` | C→S | `brush` (freeform label, recorded), `cells` (`[[cx, cy, d_cm], …]`) — one editor brush stroke of height increments in **world corner coordinates** (`cx ∈ [0, tile_size·cols]`), centimeters. Requires `role == "editor"`. The server maps each corner to *every* chunk that stores it (duplicated-edge convention), so a stroke across a chunk seam updates both sides atomically; validation is all-or-nothing (bounds, ±50 m accumulated per-corner cap, ≤16 384 cells/op) | **live** |
| `terrain.edit_error` | S→C | `message` — the op was rejected (not an editor / out of bounds / over cap / malformed); nothing was saved | **live** |
| `terrain.delta_patch` | S→C | `tx`, `ty`, `revision`, `encoding` (`"delta_v1"`), `data_b64` — pushed to **every** connected client after an accepted edit op *or revert*, once per chunk the op touched. Carries the chunk's *full current* delta (same payload format as `terrain.delta_data`), replace-not-merge, so clients holding the chunk apply it with the same decode path and clients without it ignore it | **live** |
| `terrain.edit_ack` | S→C | `op_id`, `brush` — sent to the accepted op's **author only**, before its patches: the server-minted id is the undo handle the history UI records | **live** |
| `terrain.revert_op` | C→S | `op_id` — undo one accepted op: every block it touched is restored to its logged **pre-op content** (whole 512-byte block snapshots from the append-only op log), revisions bump, and `terrain.delta_patch` broadcasts per affected chunk like a normal edit. Editor-role-gated; unknown or already-reverted ids are rejected with `terrain.edit_error` (the revert claim is atomic — racing double-reverts can't both apply). Whole-block restore means an out-of-order revert can clobber a later overlapping stroke: clients should undo newest-first | **live** |
| `terrain.revert_ack` | S→C | `op_id` — the revert was applied (its patches arrive separately) | **live** |

**Which surfaces carry hand-authored edits (#80):** `terrain.data` (the coarse backdrop) and `terrain.tile_data` (streamed chunk bytes) are always the immutable *base* bake — the backdrop is a static once-per-session payload (compositing edits in would leave it stale after the first live edit), and the client composites deltas onto streamed chunks itself from `terrain.delta_data`/`terrain.delta_patch`. Server-side, the effective (base + delta) height is answered by `proxy.rs::composited_ground_height`; the #80 audit confirmed no gameplay system consumes elevation today (movement validation is 2D clamping, ground-snap is client-visual only), so that helper is the door any future consumer must use.

Clients reconstruct the ground surface by treating each grid cell as two
triangles (split along the `(0,0)`–`(1,1)` diagonal) and must use the exact
same triangle-planar interpolation for both the rendered mesh and any height
lookup (e.g. placing entities on the ground), so the two can never disagree —
this applies identically to the coarse backdrop and to streamed fine tiles
(`Protocol.terrain_height` prefers a loaded fine tile over the backdrop for
any point one covers).

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

### `object.*` — placed world props (player-attributes epic #83, #85)

Editor-authored props with gameplay meaning; first kind: `poison_tree` (the
poison hazard, #88, reads placed trees' positions). World-scoped like
`terrain.*`: every connected client sees every object regardless of
zone/district, and coordinates are world units (metres) — the same space as
structures and resource nodes. The gateway owns the whole system (a
`world_object` table behind an in-memory cache, hydrated on first use);
zones know nothing about objects.

| message | dir | payload | status |
|---|---|---|---|
| `object.list` | C→S | — (request the full current object roster) | **live** |
| `object.list` | S→C | `objects` (`[{id, kind, x, y}, …]`) — answered from the gateway's cache; explicit even when empty, so a client never confuses "no answer yet" with "nothing placed". Request once when the world is up, then stay current via the broadcasts below | **live** |
| `object.place` | C→S | `kind`, `x`, `y` — place one object. Requires `role == "editor"`; `kind` must be registered (`poison_tree`), `(x, y)` inside the world | **live** |
| `object.delete` | C→S | `object_id` — delete one placed object. Requires `role == "editor"`; the row delete is the claim, so racing deletes of the same object produce one `object.removed` and one error | **live** |
| `object.placed` | S→C | `id`, `kind`, `x`, `y` — pushed to **every** connected client after an accepted place (the author included — clients render acks, the `terrain.delta_patch` reconcile shape) | **live** |
| `object.removed` | S→C | `id` — pushed to every connected client after an accepted delete | **live** |
| `object.edit_error` | S→C | `message` — the place/delete was rejected (not an editor / unknown kind / out of bounds / no such object / malformed / no database); nothing was saved | **live** |

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

**`env_state`** `{player_id, submerged, poison_sources}` (#87/#88) is the same
downward pattern for the environment: the zone owns hp and the damage tick but
knows no terrain or object positions, so the gateway's ~1/s environment ticker
computes each connected player's flags (submerged = the baked water mask, or
composited ground more than a threshold below sea level — so editor-dug ponds
count; `poison_sources` = poison trees within `POISON_RADIUS_M` of the player,
from the #85 object cache) and pushes them unconditionally to the player's
owning zone; the zone stores the verdict on the live entity and its tick
applies the mechanics. Unconditional re-send (rather than on-change) makes
entity recreation (split/merge/respawn/migrate resets zone-side flags to
their defaults) self-heal within a second with zero bookkeeping.

The zone-side mechanics (#87 breath, #88 poison): while `submerged`, breath
drains (~10s), then suffocation damage (~15 hp/s); surfacing refills at 3×.
While `poison_sources > 0`, `poison_buildup` rises (5s to the threshold near
one tree, faster among more), and decays (~2.5s) once clear; at the threshold
it **procs** — `poisoned` sticks, dealing ~20 hp/s with no cure in v1; only
death (respawn recreates the entity clean) ends it. Environmental damage
deliberately ignores the safe-district guard — that guard is scoped to mob/PvP
damage, and the whole capital is safe, so a safe-gated river could never drown
anyone. Player `status_update`s gain `breath` / `max_breath` / `submerged` /
`poison_buildup` / `max_poison` / `poisoned` alongside `hp` for the vitals HUD
(#89).

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

**Migration safety note (#16).** `status_update`'s `state` can carry an
in-progress gather job (`gather_node`, `gather_progress`) alongside position,
so the gateway's per-entity migration cache always reflects it; `spawn_entity`
carries those same two fields when present, and the receiving zone resumes the
`GatherJob` (only if that node still exists in its map — silent no-op
otherwise). This makes `split_zone`/`merge_zones`/`rolling_update` — the
gateway-initiated hand-offs, as opposed to an ordinary boundary-crossing
`migrate_request`, which can never fire mid-gather since gathering requires
standing still — never silently drop a player's progress toward a unit.
