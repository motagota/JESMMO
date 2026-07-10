## Terrain streaming: native-resolution ground tiles near the player.
##
## The whole-world coarse backdrop (`World._build_ground`) is the permanent,
## zero-latency fallback; this node layers genuinely native-resolution (5m)
## terrain on top of it, one baked tile at a time, streamed in around the
## player and freed as they move away. Network-independent by design: tile
## wants are emitted as a `tile_requested` signal and arrivals come in via
## `on_tile_data`, so headless tests can drive the whole load/unload policy
## without a server (mirrors `EntityManager`'s dictionary-keyed spawn/despawn
## idiom).
class_name TerrainStreamer
extends Node3D

## Tiles kept resident in each direction around the player's current tile —
## 1 = a 3x3 ring (1920m across at the production bake's 640m tiles), ample
## for the tight third-person camera while keeping at most 9 tile meshes
## (~98k triangles each) alive.
const _LOAD_RADIUS_TILES := 1
## Scene-space lift applied to streamed tile meshes only (not to gameplay
## heights — `Protocol.terrain_height` stays bias-free): the fine mesh and
## the coarse backdrop disagree slightly wherever the backdrop's ~133m grid
## cuts corners the 5m data resolves, and without a bias the two surfaces
## z-fight wherever they nearly coincide. Same trick as `World._TILE_Y`.
const _STREAM_Y_BIAS := 0.03

## Emitted when the ring wants a tile that isn't loaded or in flight —
## `Main.gd` wires this to `NetworkClient.send_terrain_tile_request`.
signal tile_requested(tx: int, ty: int)
## Emitted alongside `tile_requested` for the same chunk — `Main.gd` wires
## this to `NetworkClient.send_terrain_delta_request` (terrain editing #72).
signal delta_requested(tx: int, ty: int)

var _loaded: Dictionary = {}   # Vector2i(tx,ty) -> MeshInstance3D
var _pending: Dictionary = {}  # Vector2i(tx,ty) -> true (requested, not yet arrived)
## Delta layer state per chunk (terrain editing #72). `_pending_deltas`
## mirrors `_pending`; `_delta_offsets` holds each *answered* chunk's dense
## side*side meter offsets (empty array = answered `has_delta: false`).
## Presence in `_delta_offsets` doubles as the duplicate-answer guard: the
## offsets for a chunk are composited into its heights exactly once.
var _pending_deltas: Dictionary = {}  # Vector2i(tx,ty) -> true
var _delta_offsets: Dictionary = {}   # Vector2i(tx,ty) -> PackedFloat32Array
var _current_tile := Vector2i(-1000, -1000)
## `partition` context for GroundPaint (same painting as the backdrop).
var _zones: Array = []
var _world_size := 6400.0

## The zone list / world size from the latest `partition` — needed to paint
## streamed tiles identically to the backdrop (GroundPaint is a pure
## function of these). Safety never changes for a world position (see
## GroundPaint), so already-built tile meshes aren't repainted.
func set_context(zones: Array, world_size: float) -> void:
    _zones = zones
    _world_size = world_size

## The set of tile coords a player at tile `center` should have resident: a
## `(2*radius+1)^2` window clamped to the tile grid. Pure/static so the
## streaming policy is testable without a scene tree or network. Returned as
## a Dictionary keyed by Vector2i (values `true`) for O(1) membership tests.
static func wanted_tiles_for(center: Vector2i, radius: int, tiles_x: int, tiles_y: int) -> Dictionary:
    var wanted: Dictionary = {}
    for ty in range(maxi(center.y - radius, 0), mini(center.y + radius, tiles_y - 1) + 1):
        for tx in range(maxi(center.x - radius, 0), mini(center.x + radius, tiles_x - 1) + 1):
            wanted[Vector2i(tx, ty)] = true
    return wanted

## Hook for `LocalPlayer.position_changed` (fires every prediction tick):
## recomputes the ring only when the player actually crosses into a
## different tile, so the steady-state cost is one Vector2i compare per tick.
func on_player_position(wx: float, wy: float) -> void:
    if Protocol.terrain_tile_extent_m() <= 0.0:
        return # terrain.data (and its tile-grid shape) hasn't arrived yet
    var tile := Protocol.terrain_tile_at(wx, wy)
    if tile == _current_tile:
        return
    _current_tile = tile
    _refresh_ring()

## A decoded tile arrived (`NetworkClient.terrain_tile_data`). Applied only
## if the ring still wants it — the player may have moved on while it was in
## flight, and applying a stale tile would resurrect ground the ring already
## decided to drop.
func on_tile_data(tx: int, ty: int, heights: PackedFloat32Array) -> void:
    var coord := Vector2i(tx, ty)
    _pending.erase(coord)
    if _loaded.has(coord):
        return # duplicate delivery — the mesh already exists
    var wanted := wanted_tiles_for(_current_tile, _LOAD_RADIUS_TILES, Protocol._tiles_x, Protocol._tiles_y)
    if not wanted.has(coord):
        return
    # If the chunk's delta answer arrived first, composite it in before the
    # mesh is ever built — the common cross-order on a fresh stream-in.
    var offsets: PackedFloat32Array = _delta_offsets.get(coord, PackedFloat32Array())
    Protocol.apply_terrain_tile(tx, ty, _composited(heights, offsets))
    _loaded[coord] = _build_tile_mesh(coord)

## A chunk's delta answer arrived (`NetworkClient.terrain_delta_data`).
## Every in-range request answers exactly once (has_delta false when the
## chunk is unedited), so this is the other half of `on_tile_data`'s
## either-order pairing: tile-first means composite + rebuild here;
## delta-first means stash and let `on_tile_data` composite at build time.
func on_delta_data(tx: int, ty: int, has_delta: bool, offsets: PackedFloat32Array) -> void:
    var coord := Vector2i(tx, ty)
    _pending_deltas.erase(coord)
    if _delta_offsets.has(coord):
        return # duplicate answer — offsets were already recorded/applied
    var wanted := wanted_tiles_for(_current_tile, _LOAD_RADIUS_TILES, Protocol._tiles_x, Protocol._tiles_y)
    if not wanted.has(coord):
        return # stale: the ring moved on while the answer was in flight
    _delta_offsets[coord] = offsets if has_delta else PackedFloat32Array()
    if has_delta and _loaded.has(coord):
        # The tile arrived first and its mesh was built from base heights —
        # composite the offsets into the registry and rebuild that one mesh.
        var base: PackedFloat32Array = Protocol._tiles.get(coord, PackedFloat32Array())
        Protocol.apply_terrain_tile(tx, ty, _composited(base, offsets))
        _loaded[coord].queue_free()
        _loaded[coord] = _build_tile_mesh(coord)

## Element-wise `heights + offsets` (no-op on an empty/absent offsets
## array). Pure, so the composition rule is one testable place.
static func _composited(heights: PackedFloat32Array, offsets: PackedFloat32Array) -> PackedFloat32Array:
    if offsets.is_empty():
        return heights
    var out := heights.duplicate()
    for i in range(mini(out.size(), offsets.size())):
        out[i] += offsets[i]
    return out

func _refresh_ring() -> void:
    var wanted := wanted_tiles_for(_current_tile, _LOAD_RADIUS_TILES, Protocol._tiles_x, Protocol._tiles_y)
    for coord in wanted:
        if not _loaded.has(coord) and not _pending.has(coord):
            _pending[coord] = true
            tile_requested.emit(coord.x, coord.y)
            # The delta rides along with every tile stream-in; `_delta_offsets`
            # presence covers the tile-rebuilt-while-delta-known case.
            if not _delta_offsets.has(coord) and not _pending_deltas.has(coord):
                _pending_deltas[coord] = true
                delta_requested.emit(coord.x, coord.y)
    for coord in _pending.keys().duplicate():
        if not wanted.has(coord):
            _pending.erase(coord) # never arrived and no longer wanted
    for coord in _pending_deltas.keys().duplicate():
        if not wanted.has(coord):
            _pending_deltas.erase(coord)
    for coord in _loaded.keys().duplicate():
        if not wanted.has(coord):
            _loaded[coord].queue_free()
            _loaded.erase(coord)
            # Height queries over that footprint fall back to the coarse
            # backdrop again, matching the backdrop mesh that's all that
            # remains visible there.
            Protocol.remove_terrain_tile(coord.x, coord.y)
            # Drop the delta state too: a re-entered chunk re-requests both,
            # so edits made while we were away are picked up fresh.
            _delta_offsets.erase(coord)

## Build one tile's ground mesh: the same per-vertex height (`Protocol.w2v`)
## + paint (`GroundPaint`) + triangle winding as `World._build_ground`, just
## over one tile's footprint at native cell size instead of the whole world
## at backdrop resolution. Corner positions/colors are precomputed per
## corner (side^2) rather than per emitted vertex (6 per cell) since
## GroundPaint lookups dominate the build cost.
func _build_tile_mesh(coord: Vector2i) -> MeshInstance3D:
    var tile_size := Protocol._tile_size
    var cell_m := Protocol._tile_cell_m
    var extent := Protocol.terrain_tile_extent_m()
    var side := tile_size + 1
    var positions := PackedVector3Array()
    var colors := PackedColorArray()
    positions.resize(side * side)
    colors.resize(side * side)
    for gy in range(side):
        for gx in range(side):
            var wx := coord.x * extent + gx * cell_m
            var wy := coord.y * extent + gy * cell_m
            positions[gy * side + gx] = Protocol.w2v(wx, wy, _STREAM_Y_BIAS)
            colors[gy * side + gx] = GroundPaint.ground_color_at(_zones, _world_size, wx, wy)

    var st := SurfaceTool.new()
    st.begin(Mesh.PRIMITIVE_TRIANGLES)
    for gy in range(tile_size):
        for gx in range(tile_size):
            var i00 := gy * side + gx
            var i10 := gy * side + gx + 1
            var i01 := (gy + 1) * side + gx
            var i11 := (gy + 1) * side + gx + 1
            # Same winding as World._build_ground (p00,p10,p11 / p00,p11,p01)
            # so generated normals point up and Protocol._planar_height's
            # triangle split matches the rendered surface exactly.
            st.set_color(colors[i00])
            st.add_vertex(positions[i00])
            st.set_color(colors[i10])
            st.add_vertex(positions[i10])
            st.set_color(colors[i11])
            st.add_vertex(positions[i11])
            st.set_color(colors[i00])
            st.add_vertex(positions[i00])
            st.set_color(colors[i11])
            st.add_vertex(positions[i11])
            st.set_color(colors[i01])
            st.add_vertex(positions[i01])
    st.index()
    st.generate_normals()

    var mi := MeshInstance3D.new()
    mi.mesh = st.commit()
    var mat := StandardMaterial3D.new()
    mat.albedo_color = Color.WHITE
    mat.vertex_color_use_as_albedo = true
    mi.material_override = mat
    add_child(mi)
    return mi
