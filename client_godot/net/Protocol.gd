## Wire-protocol mirror of `docs/protocol.md` / `mmo::protocol`.
##
## Single source of truth for message type strings and the protocol version the
## client was built against, plus the movement/render tuning that must match the
## server's authoritative model. Keep in sync with the Rust `protocol.rs`.
class_name Protocol
extends RefCounted

## Bumped on incompatible changes; sent in handshake frames so the gateway can
## refuse a mismatched client (see proxy `run_handshake`).
const VERSION := 1

# --- server -> client ---------------------------------------------------------
const S_AUTH_REQUIRED := "auth_required"
const S_AUTH_OK := "auth_ok"
const S_AUTH_ERROR := "auth_error"
const S_WELCOME := "welcome"
const S_PARTITION := "partition"
const S_STATUS_UPDATE := "status_update"
const S_DESPAWN := "despawn"
const S_ZONE_MIGRATION := "zone_migration"
const S_ZONE_CAPTURE := "zone_capture"
const S_YOU_DIED := "you_died"

# --- client -> server ---------------------------------------------------------
const C_REGISTER := "register"
const C_LOGIN := "login"
const C_TOKEN := "token"
const C_GUEST := "guest"
const C_MOVE := "move"
const C_ATTACK := "attack"

# --- gameplay: gathering / inventory / skills / storage (M2) ------------------
const C_GATHER_START := "gather.start"
const C_GATHER_STOP := "gather.stop"
const S_GATHER_PROGRESS := "gather.progress"
const S_GATHER_RESULT := "gather.result"
const S_INV_UPDATE := "inv.update"
const S_SKILL_UPDATE := "skill.update"
const S_SKILL_LEVELUP := "skill.levelup"
const C_STORE_DEPOSIT := "store.deposit"
const C_STORE_WITHDRAW := "store.withdraw"
const S_STORE_UPDATE := "store.update"

# --- gameplay: build orders (M2) ----------------------------------------------
## `build.list` is bidirectional: the client sends it to request the district's
## board; the server also pushes it (hydration / after an unlock) with `orders`.
const C_BUILD_LIST := "build.list"
const S_BUILD_LIST := "build.list"
const C_BUILD_CONTRIBUTE := "build.contribute"
const S_BUILD_PROGRESS := "build.progress"
const S_BUILD_COMPLETED := "build.completed"
const S_BUILD_UNLOCKED := "build.unlocked"

# --- gameplay: starter plot allocation (M3) ------------------------------------
const S_PLOT_ASSIGNED := "plot.assigned"
## `plot.district` is bidirectional like `build.list`: the client can request a
## refresh; the server also pushes it (hydration / district crossing / a plot
## changing hands) with the current district's full roster (#18).
const C_PLOT_DISTRICT := "plot.district"
const S_PLOT_DISTRICT := "plot.district"

# --- gameplay: home structures — bed, storage, crafting station (M3 #12) ------
const C_BUILD_PLACE := "build.place"
const S_BUILD_PLACED := "build.placed"
const C_HOME_SET_RESPAWN := "home.set_respawn"
const S_HOME_RESPAWN_SET := "home.respawn_set"
const C_CRAFT_LIST := "craft.list"
const S_CRAFT_RECIPES := "craft.recipes"
const C_CRAFT_MAKE := "craft.make"
const S_CRAFT_MADE := "craft.made"

# --- gameplay: cosmetic terrain heightmap (#54) --------------------------------
const C_TERRAIN_LIST := "terrain.list"
const S_TERRAIN_DATA := "terrain.data"

# --- gameplay: rent — ticker, pay/auto-pay, lapse -> reclaim (M4 #14) ---------
const S_RENT_STATUS := "rent.status"
const C_RENT_PAY := "rent.pay"
const S_RENT_WARNING := "rent.warning"
const S_RENT_RECLAIMED := "rent.reclaimed"
const C_RENT_SET_AUTOPAY := "rent.set_autopay"

# --- gameplay: gated district transitions (M4 #15) ----------------------------
## The position/zone handoff itself is unrelated (see `S_ZONE_MIGRATION`) — this
## is purely the client-facing load/ready handshake for the transition curtain.
const C_DISTRICT_ENTER := "district.enter"
const S_DISTRICT_READY := "district.ready"

## Minimum time the transition curtain stays up, so an instant round-trip
## doesn't just flash (there's no real server-side loading in Phase 1).
const DISTRICT_TRANSITION_MIN_SECS := 0.6

## Fixed footprint (world units) for each placeable home structure kind — mirrors
## `mmo::world::structure_footprint`. Used for the ghost preview and to keep the
## client's sense of "fits on the plot" in sync with the server's.
const STRUCTURE_FOOTPRINT := {
    "bed": Vector2(20, 20),
    "storage": Vector2(16, 16),
    "crafting": Vector2(20, 20),
}
## World-unit grid step the placement ghost snaps to.
const PLACE_GRID := 10

## Must be within this many world units of a node to gather it (mirrors the server).
const GATHER_RANGE := 50.0
## Must be within this of a storage point to deposit/withdraw (mirrors the server).
const STORAGE_RANGE := 60.0
## Must be within this of a build board to contribute (mirrors the server).
const BOARD_RANGE := 60.0

# --- movement / render tuning (mirrors client.html and the server) ------------
## World units sent per move tick, per axis. The server applies the delta directly.
const MOVE_STEP := 10
## Seconds between move sends (~16/s) — a steady cadence, not OS key-repeat.
const MOVE_TICK := 0.06
## Accept the server's position as a correction only past this drift (units), so
## local prediction stays smooth between authoritative snapshots.
const RECONCILE_DRIFT := 30.0
## World units -> metres in the 3D scene (6400-unit world -> 640 m).
const WORLD_SCALE := 0.1

## Server-authored heightmap (`terrain.data`, #54) — purely cosmetic, the
## server has no other concept of height/elevation, and every gameplay
## position stays 2D. `resolution` grid cells per axis, `heights` is
## `(resolution+1)^2` floats, row-major/y-major:
## `heights[gy*(resolution+1)+gx]`. Empty until the first `terrain.data`
## arrives — `terrain_height` returns a flat `0.0` fallback until then.
static var _terrain_resolution := 0
static var _terrain_world_size := 0.0
static var _terrain_heights: PackedFloat32Array = PackedFloat32Array()

## Store the heightmap the server sent in response to `terrain.list`.
static func apply_terrain_data(resolution: int, world_size: float, heights: PackedFloat32Array) -> void:
    _terrain_resolution = resolution
    _terrain_world_size = world_size
    _terrain_heights = heights

## Grid cells per axis of the received heightmap (0 before `terrain.data`
## arrives) — `World._build_ground` must use this exact resolution so its
## mesh and `terrain_height`'s lookups share an identical grid.
static func terrain_resolution() -> int:
    return _terrain_resolution

## Ground height at world point `(wx, wy)`, sourced from the server's
## heightmap grid. Locates the enclosing grid cell and does **planar**
## (not bilinear) interpolation using whichever of the cell's two triangles
## `(wx, wy)` actually falls in — this must exactly match the triangle split
## `World._build_ground` uses to build the mesh, so a queried height can
## never disagree with the rendered surface (the "falling through" bug was
## caused by exactly this kind of mismatch, back when this was raw noise
## sampled independently of the piecewise-flat mesh).
static func terrain_height(wx: float, wy: float) -> float:
    if _terrain_heights.is_empty():
        return 0.0
    var n := _terrain_resolution
    var step := _terrain_world_size / float(n)
    var gxf: float = clampf(wx / step, 0.0, float(n))
    var gyf: float = clampf(wy / step, 0.0, float(n))
    var gx: int = clampi(int(floor(gxf)), 0, n - 1)
    var gy: int = clampi(int(floor(gyf)), 0, n - 1)
    var fx := gxf - gx
    var fy := gyf - gy
    var stride := n + 1
    var h00 := _terrain_heights[gy * stride + gx]
    var h10 := _terrain_heights[gy * stride + gx + 1]
    var h01 := _terrain_heights[(gy + 1) * stride + gx]
    var h11 := _terrain_heights[(gy + 1) * stride + gx + 1]
    if fy <= fx:
        # Triangle (p00, p11, p10).
        return h00 + (fx - fy) * (h10 - h00) + fy * (h11 - h00)
    else:
        # Triangle (p00, p01, p11).
        return h00 + (fy - fx) * (h01 - h00) + fx * (h11 - h00)

## Map a server world position `(wx, wy)` to a ground-plane point in the 3D
## scene. The server's Y axis becomes the scene's Z axis; `y` is a height
## *above* the (now not-quite-flat) terrain surface, so every existing caller
## passing "how high above the ground" keeps working unchanged, automatically
## following the terrain everywhere it's placed.
static func w2v(wx: float, wy: float, y: float = 0.0) -> Vector3:
    return Vector3(wx * WORLD_SCALE, y + terrain_height(wx, wy), wy * WORLD_SCALE)

## Mirror of the server's XP → level curve (`persistence::level_for_xp`): level n at
## 100·n² xp. Kept here so the skills panel can render progress-to-next-level and the
## build board can grey orders the player can't yet contribute to.
static func level_for_xp(xp: int) -> int:
    if xp <= 0:
        return 0
    return int(floor(sqrt(float(xp) / 100.0)))

## Total xp required to reach the start of `level`'s band (inverse of level_for_xp).
static func xp_for_level(level: int) -> int:
    return 100 * level * level
