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

# --- gameplay: cosmetic terrain heightmap (#54) + native-res tile streaming ----
const C_TERRAIN_LIST := "terrain.list"
const S_TERRAIN_DATA := "terrain.data"
const C_TERRAIN_TILE_REQUEST := "terrain.tile_request"
const S_TERRAIN_TILE_DATA := "terrain.tile_data"
## Terrain editing (epic #72): a chunk's hand-authored edit layer. An
## in-range request ALWAYS answers (`has_delta: false` when unedited).
const C_TERRAIN_DELTA_REQUEST := "terrain.delta_request"
const S_TERRAIN_DELTA_DATA := "terrain.delta_data"
## One editor brush stroke: `{brush, cells: [[cx, cy, d_cm], ...]}` in WORLD
## corner coordinates. Requires `role == "editor"` server-side; rejected with
## `terrain.edit_error`. Accepted ops come back as `terrain.delta_patch`
## (the touched chunks' full current deltas) pushed to every client.
const C_TERRAIN_EDIT_OP := "terrain.edit_op"
const S_TERRAIN_EDIT_ERROR := "terrain.edit_error"
const S_TERRAIN_DELTA_PATCH := "terrain.delta_patch"

# --- gameplay: rent — ticker, pay/auto-pay, lapse -> reclaim (M4 #14) ---------
const S_RENT_STATUS := "rent.status"
const C_RENT_PAY := "rent.pay"
const S_RENT_WARNING := "rent.warning"
const S_RENT_RECLAIMED := "rent.reclaimed"
const C_RENT_SET_AUTOPAY := "rent.set_autopay"

# --- gameplay: mayor-commissioned city build orders (e.g. dirt paths) --------
## Restricted server-side to the account with `role == "mayor"` (see `welcome`'s
## `role` field); rejected with `S_MAYOR_BUILD_ERROR` for everyone else.
const C_MAYOR_BUILD_CREATE := "mayor.build_create"
const S_MAYOR_BUILD_ERROR := "mayor.build_error"

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

## Terrain streaming (native-resolution tiles near the player): the baked
## artifact's own manifest shape, carried on the extended `terrain.data`
## message, plus a registry of currently-loaded fine tiles. All zero/empty
## until `terrain.data` arrives; the registry fills and drains as
## `TerrainStreamer` requests tiles around the player and frees ones left
## behind. `terrain_height` transparently prefers a loaded fine tile over
## the coarse backdrop grid, so every existing caller gets native fidelity
## near the player with zero call-site changes.
static var _tile_size := 0          # cells per tile side
static var _tile_cell_m := 0.0      # meters per fine cell
static var _tiles_x := 0            # tile-grid columns
static var _tiles_y := 0            # tile-grid rows
static var _height_min_m := 0.0     # u16 sample decode range (manifest's)
static var _height_max_m := 0.0
static var _tiles: Dictionary = {}  # Vector2i(tx,ty) -> PackedFloat32Array (side*side meters)

## Store the heightmap the server sent in response to `terrain.list`.
static func apply_terrain_data(resolution: int, world_size: float, heights: PackedFloat32Array) -> void:
    _terrain_resolution = resolution
    _terrain_world_size = world_size
    _terrain_heights = heights

## Store the streamable tile grid's shape (the extended `terrain.data`
## fields). Clears any previously-loaded tiles: a new manifest means a new
## bake, and stale fine tiles from the old one must not shadow it.
static func apply_terrain_meta(tile_size: int, cell_size_m: float, tiles_x: int, tiles_y: int, height_min_m: float, height_max_m: float) -> void:
    _tile_size = tile_size
    _tile_cell_m = cell_size_m
    _tiles_x = tiles_x
    _tiles_y = tiles_y
    _height_min_m = height_min_m
    _height_max_m = height_max_m
    _tiles.clear()

## Register one decoded fine tile (heights already in meters, row-major
## `side*side` where `side == tile_size + 1` — the edge-duplication corner
## convention `terrain-common`'s HeightTile uses).
static func apply_terrain_tile(tx: int, ty: int, heights: PackedFloat32Array) -> void:
    _tiles[Vector2i(tx, ty)] = heights

static func remove_terrain_tile(tx: int, ty: int) -> void:
    _tiles.erase(Vector2i(tx, ty))

static func has_terrain_tile(tx: int, ty: int) -> bool:
    return _tiles.has(Vector2i(tx, ty))

## One tile's world-space extent in meters (0.0 before `terrain.data`).
static func terrain_tile_extent_m() -> float:
    return _tile_size * _tile_cell_m

## The tile-grid coordinate owning world point `(wx, wy)`, clamped to the
## manifest's actual grid (mirrors `terrain_common::Terrain::locate`'s edge
## convention: the world's far edge belongs to the last tile, not a
## nonexistent one past it). `Vector2i(-1, -1)` before `terrain.data`.
static func terrain_tile_at(wx: float, wy: float) -> Vector2i:
    var extent := terrain_tile_extent_m()
    if extent <= 0.0 or _tiles_x <= 0 or _tiles_y <= 0:
        return Vector2i(-1, -1)
    var tx: int = clampi(int(floor(wx / extent)), 0, _tiles_x - 1)
    var ty: int = clampi(int(floor(wy / extent)), 0, _tiles_y - 1)
    return Vector2i(tx, ty)

## Decode a `terrain.tile_data` payload's raw bytes (terrain-common's
## `HeightTile::encode` format: 16-byte header — magic "TRHT", u16 LE
## format_version, u16 reserved, i32 LE tile_x, i32 LE tile_y — then
## side*side u16 LE samples) into meters via the manifest's height range.
## Returns `{tx, ty, heights}` or `{}` on any malformed input.
static func decode_height_tile(bytes: PackedByteArray) -> Dictionary:
    var side := _tile_size + 1
    if _tile_size <= 0 or bytes.size() != 16 + side * side * 2:
        return {}
    if bytes.slice(0, 4).get_string_from_ascii() != "TRHT":
        return {}
    var tx := bytes.decode_s32(8)
    var ty := bytes.decode_s32(12)
    var heights := PackedFloat32Array()
    heights.resize(side * side)
    var range_m := _height_max_m - _height_min_m
    for i in range(side * side):
        var raw := bytes.decode_u16(16 + i * 2)
        heights[i] = _height_min_m + (float(raw) / 65535.0) * range_m
    return {"tx": tx, "ty": ty, "heights": heights}

## Decode a `terrain.delta_data` payload's raw bytes (terrain-common's
## `SparseHeightDelta::encode` format: 8-byte header — magic "TRHD", u16 LE
## format_version, u16 reserved — then a block-presence bitmap of
## `ceil(ceil(side/16)^2 / 64)` u64 LE words, then each present 16x16
## block's 256 i16 LE centimeter offsets in ascending block-index order)
## into a dense `side*side` array of METER offsets, zero everywhere no
## block covers. Dense-on-decode keeps compositing a plain element-wise
## add against a streamed tile's heights. Returns an empty array on any
## malformed input (mirrors `decode_height_tile`'s silent-drop posture).
static func decode_height_delta(bytes: PackedByteArray) -> PackedFloat32Array:
    var side := _tile_size + 1
    if _tile_size <= 0 or bytes.size() < 8:
        return PackedFloat32Array()
    if bytes.slice(0, 4).get_string_from_ascii() != "TRHD":
        return PackedFloat32Array()
    var bps := int(ceil(side / 16.0))
    var words := int(ceil(bps * bps / 64.0))
    if bytes.size() < 8 + words * 8:
        return PackedFloat32Array()
    var indices: Array[int] = []
    for w in range(words):
        var word := bytes.decode_u64(8 + w * 8)
        for bit in range(64):
            if word & (1 << bit):
                indices.append(w * 64 + bit)
    if bytes.size() != 8 + words * 8 + indices.size() * 256 * 2:
        return PackedFloat32Array()
    var offsets := PackedFloat32Array()
    offsets.resize(side * side) # zero-filled: untouched corners offset by 0
    var pos := 8 + words * 8
    for idx in indices:
        if idx >= bps * bps:
            return PackedFloat32Array() # bitmap bit outside the block grid
        var block_row := int(floor(idx / float(bps)))
        var block_col := idx % bps
        for cy in range(16):
            var gy := block_row * 16 + cy
            for cx in range(16):
                var gx := block_col * 16 + cx
                var cm := bytes.decode_s16(pos)
                pos += 2
                # Edge blocks are partial: cells past `side` are stored
                # (as zeros) but out of the corner grid — skip them.
                if gx < side and gy < side:
                    offsets[gy * side + gx] = cm * 0.01
    return offsets

## Grid cells per axis of the received heightmap (0 before `terrain.data`
## arrives) — `World._build_ground` must use this exact resolution so its
## mesh and `terrain_height`'s lookups share an identical grid.
static func terrain_resolution() -> int:
    return _terrain_resolution

## Planar (triangle-split, NOT bilinear) interpolation within one grid cell,
## given its 4 corner heights and the fractional position inside it. The
## split must exactly match the triangle winding the meshes use
## (`World._build_ground` and `TerrainStreamer`'s per-tile builder share it:
## p00-p10-p11 / p00-p11-p01), so a queried height can never disagree with
## the rendered surface (the "falling through" bug was caused by exactly
## this kind of mismatch, back when heights were raw noise sampled
## independently of the piecewise-flat mesh).
static func _planar_height(h00: float, h10: float, h01: float, h11: float, fx: float, fy: float) -> float:
    if fy <= fx:
        # Triangle (p00, p11, p10).
        return h00 + (fx - fy) * (h10 - h00) + fy * (h11 - h00)
    else:
        # Triangle (p00, p01, p11).
        return h00 + (fy - fx) * (h01 - h00) + fx * (h11 - h00)

## Ground height at world point `(wx, wy)`. Prefers a loaded
## native-resolution streamed tile when one covers the point (terrain
## streaming); otherwise falls back to the coarse whole-world backdrop grid
## from `terrain.data` — the permanent fallback, so there is always *an*
## answer everywhere from the moment the backdrop arrives.
static func terrain_height(wx: float, wy: float) -> float:
    var fine := _tile_height(wx, wy)
    if not is_nan(fine):
        return fine
    if _terrain_heights.is_empty():
        return 0.0
    var n := _terrain_resolution
    var step := _terrain_world_size / float(n)
    var gxf: float = clampf(wx / step, 0.0, float(n))
    var gyf: float = clampf(wy / step, 0.0, float(n))
    var gx: int = clampi(int(floor(gxf)), 0, n - 1)
    var gy: int = clampi(int(floor(gyf)), 0, n - 1)
    var stride := n + 1
    return _planar_height(
        _terrain_heights[gy * stride + gx],
        _terrain_heights[gy * stride + gx + 1],
        _terrain_heights[(gy + 1) * stride + gx],
        _terrain_heights[(gy + 1) * stride + gx + 1],
        gxf - gx, gyf - gy)

## Fine-tile height at `(wx, wy)`, or NAN when no loaded tile covers it.
## Same planar interpolation as the backdrop, just against the tile's own
## `side*side` corner grid (side = tile_size + 1, edges deliberately
## duplicated with neighbors so any interior point resolves from one tile).
static func _tile_height(wx: float, wy: float) -> float:
    if _tiles.is_empty():
        return NAN
    var coord := terrain_tile_at(wx, wy)
    var heights: PackedFloat32Array = _tiles.get(coord, PackedFloat32Array())
    if heights.is_empty():
        return NAN
    var extent := terrain_tile_extent_m()
    var gxf: float = clampf((wx - coord.x * extent) / _tile_cell_m, 0.0, float(_tile_size))
    var gyf: float = clampf((wy - coord.y * extent) / _tile_cell_m, 0.0, float(_tile_size))
    var gx: int = clampi(int(floor(gxf)), 0, _tile_size - 1)
    var gy: int = clampi(int(floor(gyf)), 0, _tile_size - 1)
    var stride := _tile_size + 1
    return _planar_height(
        heights[gy * stride + gx],
        heights[gy * stride + gx + 1],
        heights[(gy + 1) * stride + gx],
        heights[(gy + 1) * stride + gx + 1],
        gxf - gx, gyf - gy)

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
