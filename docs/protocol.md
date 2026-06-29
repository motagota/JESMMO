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
| `partition` | `world`, `zones[]` | world size + each zone's region/owner/progress; drives the map |
| `status_update` | `player_id`, `state{x,y,hp,max_hp,type,facing}`, `zone` | entity snapshot (~20 Hz) |
| `despawn` | `player_id` | stop rendering an entity (death/disconnect) |
| `zone_migration` | `zone` | the player's authoritative zone changed (seamless handoff) |
| `zone_capture` | `zone_id`, `owner`, `progress` | territory-control state (wilds mechanic; off in safe zones later) |
| `you_died` | `player_id` | local death feedback |

---

## Reserved gameplay messages (Phase 1)

New gameplay messages are grouped by a **domain prefix** so the wire stays
self-describing as features land. The names below are **reserved now** (a single
source of truth in `mmo::protocol`, mirrored by the Godot client's `Protocol.gd`);
the gateway/zone handlers that act on them arrive with their milestones, so until
then sending one is a no-op. Direction: **C→S** client to server, **S→C** server
to client.

### `gather.*` — resource gathering (M2 §4.1)

| type | dir | fields |
|---|---|---|
| `gather.start` | C→S | `node_id` |
| `gather.progress` | S→C | `node_id`, `pct` |
| `gather.result` | S→C | `item_id`, `qty` |
| `node.depleted` | S→C | `node_id` |
| `node.respawn` | S→C | `node_id` |

### `inv.*` / `store.*` — inventory & storage (M2 §4.2)

| type | dir | fields |
|---|---|---|
| `inv.update` | S→C | `items` |
| `inv.move` | C→S | `from`, `to` |
| `store.deposit` | C→S | `item_id`, `qty` |
| `store.withdraw` | C→S | `item_id`, `qty` |

### `build.*` — build orders & placement (M2 §4.3, M3 §4.5)

| type | dir | fields |
|---|---|---|
| `build.list` | C→S | — (open orders for the district) |
| `build.contribute` | C→S | `order_id`, `item_id`, `qty` |
| `build.progress` | S→C | `order_id`, `progress` |
| `build.completed` | S→C | `order_id`, `structures` |
| `build.unlocked` | S→C | `order_ids` |
| `build.place` | C→S | `kind`, `x`, `y`, `rot` |
| `build.placed` | S→C | `structure` |

### `plot.*` — plots (M3 §4.4)

| type | dir | fields |
|---|---|---|
| `plot.assigned` | S→C | `plot_id`, `district`, `bounds`, `tier` |
| `plot.info` | C→S | — (current plot details) |

### `skill.*` — use-based skills (M2 §4.6)

| type | dir | fields |
|---|---|---|
| `skill.update` | S→C | `skill_id`, `xp`, `level` |
| `skill.levelup` | S→C | `skill_id`, `level` |

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
`zone_stats` with the gateway. See `proxy.rs` and `zone_server.rs`.

**M0 note on positions.** A returning character is recreated at its exact saved
position via `spawn_entity` to whichever zone owns that point (routed by
`zone_at`); a guest/new player uses `player_join` (the zone picks a spawn point).
The gateway persists each durable character's `(x, y, hp)` periodically and on
disconnect, so logout/restart restores the player where they were.
