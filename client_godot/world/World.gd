## The 3D capital: a ground plane, painted by district safety, and the
## town-centre marker, all rebuilt from the gateway's `partition` message.
##
## The capital starts empty — no roads are authored; this draws the ground
## (safety-tinted directly on its surface — see `_build_ground`) plus a
## town-centre marker. Roads are real, buildable content now (mayor-issued
## dirt paths, see `upsert_dirt_road`), and other structures arrive as build
## orders complete.
class_name World
extends Node3D

const _GROUND_Y := 0.0
const _TILE_Y := 0.2    # plot/home markers sit just above the ground to avoid z-fighting
const _ROAD_Y := 0.5
const _ROAD_WIDTH := 6.0        # a dirt path is a footpath, not the old avenue
const _ROAD_SEGMENT_STEP := 40.0  # world units per subdivision — short enough to hug hills
const _ROAD_COLOR := Color(0.45, 0.33, 0.20)
const _PLOT_FILL_STEPS := 8       # fill-grid subdivisions per plot axis
const _PLOT_EDGE_STEP := 10.0     # plot edges are short; sample tighter than roads

## Sea level in world metres — the DEM's own water-surface convention: the
## river/bay NoData footprint is filled flat at exactly 0.0m (see
## terrain.toml's header), so 0.0 IS the water surface, and everything at
## or below it belongs under this plane.
const _WATER_LEVEL_M := 0.0
## Scene-space lift for the water plane, chosen against two floors it must
## clear: (1) the flat 0m NoData fill itself, which the coarse backdrop
## renders at scene 0 and streamed fine tiles at +0.3
## (`TerrainStreamer._STREAM_Y_BIAS`) — a plane at sea level exactly would
## z-fight both; (2) enough water column over BOTH ground tiers that the
## shader's murk and alpha saturate on each (see `murk_depth` and
## `shore_fade` in water.gdshader — 0.6 leaves 0.3 over the streamed fill,
## exactly full-alpha and ~4 murk depths), so the ±0.3 bias step doesn't
## draw itself as a blocky tint change along the streaming ring's edge.
## This was 1.2 while terrain.toml's water mask was empty and [detail]
## noise put ~±0.9 scene of dry-speckle micro-relief ON the fill; the #84
## sea_level_m = 0 rebake masks the fill (noise skipped, bed truly flat),
## which retired that third, tallest floor. The ~0.4m of world height 0.6
## nominally floods (0.6 / HEIGHT_SCALE) is riverbank fringe that already
## paints silt-brown (GroundPaint), so the waterline lands inside the
## wet-mud band where it belongs.
const _WATER_Y := 0.6

var world_size := 6400.0

var _ground: MeshInstance3D
var _water: MeshInstance3D
## Backdrop residency mask (one texel per terrain tile): white = a fine
## streamed tile is resident there, and the backdrop shader discards its
## fragments so the coarse mesh can never poke up through the real 5m
## ground (its ~66m interpolation runs metres above/below the true surface
## in places — enough to bury the player). Updated from
## `update_backdrop_mask` every time the streamer's resident set changes.
var _mask_image: Image
var _mask_texture: ImageTexture

## The backdrop's material: standard vertex-color look (white base, painted
## per-vertex albedo, roughness 1) plus the residency-mask discard above.
## Environment fog still applies — spatial shaders get it unless disabled.
const _GROUND_SHADER := "
shader_type spatial;
uniform sampler2D tile_mask : filter_nearest;
uniform vec2 world_extent = vec2(1.0, 1.0);
varying vec3 wpos;
void vertex() {
    wpos = VERTEX; // the mesh sits at the origin, so object space == world space
}
void fragment() {
    if (texture(tile_mask, wpos.xz / world_extent).r > 0.5) {
        discard;
    }
    ALBEDO = COLOR.rgb;
    ROUGHNESS = 1.0;
}
"

var _tiles_root := Node3D.new()
var _roads_root := Node3D.new()
var _home_root := Node3D.new()
var _plots_root := Node3D.new()
var _built_static := false
## `partition` and `terrain.data` (#54) are two independent round-trips with
## no guaranteed arrival order. Building the ground before the real
## heightmap lands would permanently bake in the flat `0.0` fallback (since
## `_built_static` only ever builds once), so both must have arrived before
## the static ground/roads are built.
var _partition_received := false
var _terrain_ready := false
## The last `partition`'s raw zone entries (`{x0,y0,x1,y1,district,...}`), kept
## around so `district_at` can answer "which district is this point in" without
## a server round-trip — the client already has everything it needs (#15).
var _zones: Array = []
## Rendered dirt-road segments, keyed by their `status_update` id
## (`structure_<build order kind>`) — a road is static once built, so a repeat
## update (e.g. a re-broadcast on district re-entry) is a no-op, not a rebuild.
## Each entry keeps its endpoints (`{a, b, node}`) so `refresh_plot_markers`
## can re-drape the ribbon when the terrain under it changes.
var _dirt_roads: Dictionary = {}
## Staked road plans (#95): accepted-but-unbuilt road orders, keyed by
## order_id — {"path": [Vector2, ...], "nodes": [MeshInstance3D, ...]}.
## Replaced wholesale on every `build.list` push and dropped on completion
## (#96's built road takes over).
var _road_plans: Dictionary = {}
## Surveyor look for a planned road: a bright translucent strip the width of
## the future road, orange survey stakes marching along it, and a floating
## "planned road" label — a player must be able to SEE where stone is
## wanted from a distance, not squint for a faint line (user feedback after
## #99 shipped). The strip sits slightly above road height so a plan never
## z-fights the road that later replaces it.
const _PLAN_COLOR := Color(1.0, 0.82, 0.20, 0.75)
const _PLAN_WIDTH := 4.5
const _STAKE_COLOR := Color(1.0, 0.45, 0.10)
const _STAKE_EVERY_M := 24.0
## Demolition sites (#107, kind `demo_*`) wear red so "coming down" never
## reads as "going up".
const _DEMO_COLOR := Color(0.95, 0.18, 0.14, 0.75)
const _DEMO_STAKE_COLOR := Color(0.95, 0.12, 0.10)
## Built road orders (state completed, kind road_*) tracked from the board
## for the demolish tool's picking (#107): order_id -> {"path": [Vector2..]}.
## Data only — the built ribbon renders via the structure entity.
var _completed_road_orders: Dictionary = {}
## What the plot markers were last drawn from, so they can be redrawn against
## fresh terrain heights (see `refresh_plot_markers`).
var _home_bounds: Dictionary = {}
var _roster_plots: Array = []
var _my_plot_id := ""
var _plots_dirty := false

func _ready() -> void:
    add_child(_tiles_root)
    add_child(_roads_root)
    add_child(_home_root)
    add_child(_plots_root)

## The displayed terrain changed under us (a fine tile streamed in/out, or an
## edit patch landed — `TerrainStreamer.terrain_changed`): the plot markers
## and roads are static meshes sampled at draw time, so without a redraw they
## stay buried under (or float over) the new surface. Deferred to `_process`
## so a burst of tile arrivals costs one rebuild, not one per tile.
func refresh_plot_markers() -> void:
    _plots_dirty = true

## Marker/road redraw cadence: during tile stream-in bursts terrain_changed
## fires nearly every frame, and redrawing the whole 240-plot roster each
## time is real stutter — 4Hz tracks the ground closely enough for static
## overlay meshes.
const _PLOT_REFRESH_INTERVAL := 0.25
var _plot_refresh_cooldown := 0.0

func _process(delta: float) -> void:
    _plot_refresh_cooldown = maxf(_plot_refresh_cooldown - delta, 0.0)
    if not _plots_dirty or _plot_refresh_cooldown > 0.0:
        return
    _plots_dirty = false
    _plot_refresh_cooldown = _PLOT_REFRESH_INTERVAL
    if not _home_bounds.is_empty():
        show_home_plot(_home_bounds)
    if not _roster_plots.is_empty():
        apply_plot_roster(_roster_plots, _my_plot_id)
    for id in _dirt_roads:
        var rec: Dictionary = _dirt_roads[id]
        for n in rec["nodes"]:
            n.queue_free()
        rec["nodes"] = _build_road_path(rec["path"])
    for id in _road_plans:
        _restake_plan(id)

## Rebuild the district name labels from a `partition` message; lazily build
## the static ground/roads once the world size and the terrain heightmap are
## both known. `_zones` must be set *before* `_maybe_build_static()` so the
## ground's first (and only) build already has real safety data to paint —
## see `_build_ground`.
func apply_partition(msg: Dictionary) -> void:
    world_size = float(msg.get("world", world_size))
    _partition_received = true
    _zones = msg.get("zones", [])
    _maybe_build_static()
    _rebuild_tiles()

## The terrain heightmap (#54) arrived — build the ground/roads if the
## world size (from `partition`) is already known too. `_maybe_build_static`
## only ever builds once both flags are set, so the ground is never built
## with the flat `0.0` fallback and in need of a later redraw.
func on_terrain_data() -> void:
    _terrain_ready = true
    _maybe_build_static()
    # Anything staked before the heightmap arrived (the board hydration can
    # beat terrain.data on login) was draped over the flat 0.0 fallback —
    # buried under real ground. Re-drape it now.
    refresh_plot_markers()

func _maybe_build_static() -> void:
    if _built_static or not _partition_received or not _terrain_ready:
        return
    _build_ground()
    _build_water()
    _build_town_centre_marker()
    _build_quarry_marker()
    _built_static = true

## Repaint the backdrop's residency mask from the streamer's current
## resident-tile set (`TerrainStreamer.resident_tiles()`), wired to
## `terrain_changed` in Main — cheap enough (a tiles_x*tiles_y byte image)
## to just redo wholesale on every change.
func update_backdrop_mask(resident: Dictionary) -> void:
    if _mask_image == null:
        return
    _mask_image.fill(Color.BLACK)
    for coord in resident:
        if coord.x >= 0 and coord.x < _mask_image.get_width() \
                and coord.y >= 0 and coord.y < _mask_image.get_height():
            _mask_image.set_pixel(coord.x, coord.y, Color.WHITE)
    _mask_texture.update(_mask_image)

func _rebuild_tiles() -> void:
    for child in _tiles_root.get_children():
        child.queue_free()

    for entry_v in _zones:
        var z: Dictionary = entry_v
        _add_district_label(z)

## The district name containing world point `(wx, wy)`, or "" if it falls
## outside every known zone tile (shouldn't happen — the capital tiles the
## whole world) or before the first `partition` arrives (#15).
func district_at(wx: float, wy: float) -> String:
    for entry_v in _zones:
        var z: Dictionary = entry_v
        if wx >= float(z.get("x0", 0)) and wx < float(z.get("x1", 0)) \
                and wy >= float(z.get("y0", 0)) and wy < float(z.get("y1", 0)):
            return String(z.get("district", ""))
    return ""

## The raw zone entry (`{x0,y0,x1,y1,district,...}`) containing world point
## `(wx, wy)`, or `{}` if none match — the minimap needs the full bounds, not
## just the district name `district_at` returns (#18).
func district_rect_at(wx: float, wy: float) -> Dictionary:
    for entry_v in _zones:
        var z: Dictionary = entry_v
        if wx >= float(z.get("x0", 0)) and wx < float(z.get("x1", 0)) \
                and wy >= float(z.get("y0", 0)) and wy < float(z.get("y1", 0)):
            return z
    return {}

## A grid mesh (not a flat `PlaneMesh`) so the ground actually shows
## `Protocol.terrain_height`'s relief instead of looking dead flat — every
## vertex is placed via the same `w2v` everything else already goes through,
## so it's automatically consistent with where props/entities sit on it.
##
## District safety used to be a separate translucent overlay plane
## (`_add_district_tile`), sampled at just one height per district — fine
## against the old gentle synthetic terrain, but real DEM terrain (#69)
## carries enough relief across a whole district's footprint that a single
## flat plane visibly floated above/through the real ground. Painted
## directly into the ground mesh's own per-vertex colors instead: no second
## mesh, no z-fighting margin, and it's mechanically impossible for the tint
## to disagree with the surface it's tinting, since they're the same
## vertices.
func _build_ground() -> void:
    # Must match the server's grid exactly (#54) — the client's height
    # lookups (`Protocol.terrain_height`) interpolate the *same* received
    # grid, so a mismatched local resolution here would make the rendered
    # surface disagree with where entities are placed on it.
    #
    # Built as an indexed ArrayMesh with per-corner (not per-emitted-vertex)
    # positions/colors and analytic heightfield normals — the SurfaceTool
    # version of this was a ~1.1s freeze at session start. Triangles are
    # (p00,p10,p11)/(p00,p11,p01), the same split `Protocol._planar_height`
    # interpolates and the same upward winding tests/smoke_terrain.gd checks.
    var resolution := Protocol.terrain_resolution()
    var step := world_size / float(resolution)
    var side := resolution + 1
    var n := side * side
    var positions := PackedVector3Array()
    var colors := PackedColorArray()
    var normals := PackedVector3Array()
    var scene_y := PackedFloat32Array()
    positions.resize(n)
    colors.resize(n)
    normals.resize(n)
    scene_y.resize(n)
    var ws := Protocol.WORLD_SCALE
    var hs := Protocol.HEIGHT_SCALE
    for gy in range(side):
        var wy := gy * step
        var row := gy * side
        for gx in range(side):
            var i := row + gx
            var wx := gx * step
            var h := Protocol.terrain_height(wx, wy)
            scene_y[i] = h * hs
            positions[i] = Vector3(wx * ws, scene_y[i] + _GROUND_Y, wy * ws)
            colors[i] = GroundPaint.ground_color_at_height(_zones, world_size, wx, wy, h)
    var span := step * ws
    for gy in range(side):
        var row := gy * side
        for gx in range(side):
            var i := row + gx
            var xl: float = scene_y[i - 1] if gx > 0 else scene_y[i]
            var xr: float = scene_y[i + 1] if gx < resolution else scene_y[i]
            var zu: float = scene_y[i - side] if gy > 0 else scene_y[i]
            var zd: float = scene_y[i + side] if gy < resolution else scene_y[i]
            var span_x := span * 2.0 if gx > 0 and gx < resolution else span
            var span_z := span * 2.0 if gy > 0 and gy < resolution else span
            normals[i] = Vector3(-(xr - xl) / span_x, 1.0, -(zd - zu) / span_z).normalized()
    var indices := PackedInt32Array()
    indices.resize(resolution * resolution * 6)
    var k := 0
    for gy in range(resolution):
        for gx in range(resolution):
            var i00 := gy * side + gx
            var i11 := i00 + side + 1
            indices[k] = i00
            indices[k + 1] = i00 + 1      # i10
            indices[k + 2] = i11
            indices[k + 3] = i00
            indices[k + 4] = i11
            indices[k + 5] = i00 + side   # i01
            k += 6

    var arrays := []
    arrays.resize(Mesh.ARRAY_MAX)
    arrays[Mesh.ARRAY_VERTEX] = positions
    arrays[Mesh.ARRAY_NORMAL] = normals
    arrays[Mesh.ARRAY_COLOR] = colors
    arrays[Mesh.ARRAY_INDEX] = indices
    var ground_mesh := ArrayMesh.new()
    ground_mesh.add_surface_from_arrays(Mesh.PRIMITIVE_TRIANGLES, arrays)

    _ground = MeshInstance3D.new()
    _ground.mesh = ground_mesh
    # Shader material: same vertex-color-as-albedo look the old
    # StandardMaterial gave (the two triangles per cell already wind so
    # generated normals point up — verified by tests/smoke_terrain.gd), plus
    # the residency-mask discard so the backdrop never renders underneath a
    # resident fine tile (see `_mask_image`'s comment). Offline harnesses
    # that never learned the tile-grid shape get a 1x1 never-discard mask.
    var shader := Shader.new()
    shader.code = _GROUND_SHADER
    var mat := ShaderMaterial.new()
    mat.shader = shader
    var mask_w := maxi(Protocol._tiles_x, 1)
    var mask_h := maxi(Protocol._tiles_y, 1)
    _mask_image = Image.create(mask_w, mask_h, false, Image.FORMAT_L8)
    _mask_image.fill(Color.BLACK)
    _mask_texture = ImageTexture.create_from_image(_mask_image)
    mat.set_shader_parameter("tile_mask", _mask_texture)
    mat.set_shader_parameter("world_extent",
        Vector2(world_size, world_size) * Protocol.WORLD_SCALE)
    _ground.material_override = mat
    add_child(_ground)

## The bay/river water surface: one world-spanning translucent plane at sea
## level (`_WATER_LEVEL_M` + `_WATER_Y`, see those constants). A single
## static plane is correct here, not a per-tile or masked mesh: sea level is
## one global height, the terrain itself decides where water is visible
## (everywhere the ground dips below the plane — exactly the NoData 0m fill
## plus the real below-sea channel cells), and the shader's depth-buffer
## shore fade makes the waterline follow whichever ground LOD is currently
## rendered. Nav is untouched — terrain.toml's [water] mask stays empty, so
## this is purely a visual layer (its note says to revisit nav-blocking once
## water RENDERING exists; blocking is a separate, server-side decision).
func _build_water() -> void:
    var plane := PlaneMesh.new()
    plane.size = Vector2(world_size, world_size) * Protocol.WORLD_SCALE
    _water = MeshInstance3D.new()
    _water.mesh = plane
    var mid := world_size * 0.5 * Protocol.WORLD_SCALE
    _water.position = Vector3(mid,
        _WATER_LEVEL_M * Protocol.HEIGHT_SCALE + _WATER_Y, mid)
    var mat := ShaderMaterial.new()
    mat.shader = load("res://world/water.gdshader")
    _water.material_override = mat
    # A 25.6km translucent sheet must never contribute to shadows.
    _water.cast_shadow = GeometryInstance3D.SHADOW_CASTING_SETTING_OFF
    add_child(_water)

func _build_town_centre_marker() -> void:
    var mid := world_size * 0.5
    var marker := MeshInstance3D.new()
    var cyl := CylinderMesh.new()
    cyl.top_radius = 6.0 * Protocol.WORLD_SCALE
    cyl.bottom_radius = 6.0 * Protocol.WORLD_SCALE
    cyl.height = 3.0
    marker.mesh = cyl
    marker.position = Protocol.w2v(mid, mid, 1.5)
    var mm := StandardMaterial3D.new()
    mm.albedo_color = Color(0.95, 0.85, 0.30)
    marker.material_override = mm
    _roads_root.add_child(marker)

## The quarry site marker (#97; relocated to Mt Coot-tha's east flank in
## #99): a floor slab + distance-culled label at the authored working face's
## centre (mirrors `world.rs`'s `node_quarry_*` cluster, the same way the
## town-centre marker mirrors WORLD_SIZE/2). The rocks themselves are live
## resource nodes rendered by EntityManager.
const _QUARRY_SITE := Vector2(8232, 13915)

func _build_quarry_marker() -> void:
    var slab := MeshInstance3D.new()
    var cyl := CylinderMesh.new()
    cyl.top_radius = 22.0 * Protocol.WORLD_SCALE
    cyl.bottom_radius = 22.0 * Protocol.WORLD_SCALE
    cyl.height = 0.4
    slab.mesh = cyl
    slab.position = Protocol.w2v(_QUARRY_SITE.x, _QUARRY_SITE.y, 0.2)
    var sm := StandardMaterial3D.new()
    sm.albedo_color = Color(0.60, 0.57, 0.50) # worked, dusty ground
    slab.material_override = sm
    _roads_root.add_child(slab)

    var label := Label3D.new()
    label.text = "⛏ Quarry"
    label.billboard = BaseMaterial3D.BILLBOARD_ENABLED
    label.no_depth_test = true
    label.fixed_size = true
    label.pixel_size = 0.004
    label.modulate = Color(0.95, 0.9, 0.75)
    label.outline_size = 8
    label.position = Protocol.w2v(_QUARRY_SITE.x, _QUARRY_SITE.y, 8.0)
    label.visibility_range_end = 900.0
    label.visibility_range_end_margin = 100.0
    label.visibility_range_fade_mode = GeometryInstance3D.VISIBILITY_RANGE_FADE_SELF
    _roads_root.add_child(label)

## Render a completed `dirt_road` build order — a segment from `(x,y)` to
## `(x1,y1)` — as a real road: a multi-vertex ribbon, not a single rigid box.
## Every cross-section along the segment is placed with its own `Protocol.w2v`
## sample (exactly like `_build_ground`'s per-vertex grid), so the path actually
## follows the terrain's hills its whole length instead of floating/clipping
## between two flat endpoints. `id` is the `status_update` id
## (`structure_<build order kind>`) — idempotent, since a road never moves.
## A completed dirt road. Editor-laid roads (#96) carry their full multi-run
## grid `path`; the mayor's two-click segments still arrive as x/y..x1/y1 and
## render as the single run they are — both store the same
## `{"path": [Vector2, ...], "nodes": [ribbon, ...]}` shape.
func upsert_dirt_road(id: String, state: Dictionary) -> void:
    if _dirt_roads.has(id):
        return
    var path: Array = []
    var raw: Array = state.get("path", [])
    if raw.size() >= 2:
        for p in raw:
            path.append(Vector2(float(p[0]), float(p[1])))
    else:
        var a := Vector2(float(state.get("x", 0)), float(state.get("y", 0)))
        path = [a, Vector2(float(state.get("x1", a.x)), float(state.get("y1", a.y)))]
    _dirt_roads[id] = {"path": path, "nodes": _build_road_path(path)}

func _build_road_path(path: Array) -> Array:
    var nodes: Array = []
    for i in range(1, path.size()):
        nodes.append(_build_road_ribbon(path[i - 1], path[i]))
    return nodes

func _build_road_ribbon(a: Vector2, b: Vector2) -> MeshInstance3D:
    return _build_ribbon(_roads_root, a, b, _ROAD_WIDTH, _ROAD_COLOR, _ROAD_Y, _ROAD_SEGMENT_STEP)

## Staked road plans (#95): rebuild the whole set from a `build.list` push.
## Replace-not-merge — the board is authoritative, so a plan that finished
## (or a stale render from before a reconnect) simply isn't re-staked.
func apply_road_plans(orders: Array) -> void:
    for id in _road_plans.keys():
        for n in _road_plans[id]["nodes"]:
            n.queue_free()
    _road_plans.clear()
    _completed_road_orders.clear()
    for o_v in orders:
        var o: Dictionary = o_v
        var raw: Array = o.get("path", [])
        if raw.size() < 2:
            continue
        var path: Array = []
        for p in raw:
            path.append(Vector2(float(p[0]), float(p[1])))
        var state := String(o.get("state", ""))
        var kind := String(o.get("kind", ""))
        if state == "open":
            var progress_total := 0
            for v in (o.get("progress", {}) as Dictionary).values():
                progress_total += int(v)
            _road_plans[String(o.get("order_id", ""))] = {
                "path": path, "nodes": [], "kind": kind, "progress_total": progress_total,
            }
        elif state == "completed" and kind.begins_with("road_"):
            # Built roads, for the demolish tool's picking (#107).
            _completed_road_orders[String(o.get("order_id", ""))] = {"path": path}
    for id in _road_plans:
        _restake_plan(id)

## A built road was demolished (#107): its structure entity despawned —
## un-render the ribbons. No-op for non-road despawns.
func remove_dirt_road(id: String) -> void:
    if not _dirt_roads.has(id):
        return
    for n in _dirt_roads[id]["nodes"]:
        n.queue_free()
    _dirt_roads.erase(id)

## An order completed (#96 renders the built road) — drop its stakes.
func remove_road_plan(order_id: String) -> void:
    if not _road_plans.has(order_id):
        return
    for n in _road_plans[order_id]["nodes"]:
        n.queue_free()
    _road_plans.erase(order_id)

func _restake_plan(order_id: String) -> void:
    var rec: Dictionary = _road_plans[order_id]
    for n in rec["nodes"]:
        n.queue_free()
    var nodes: Array = []
    var path: Array = rec["path"]
    var demo: bool = String(rec.get("kind", "")).begins_with("demo_")
    var strip_color := _DEMO_COLOR if demo else _PLAN_COLOR
    var stake_color := _DEMO_STAKE_COLOR if demo else _STAKE_COLOR
    var total := 0.0
    for i in range(1, path.size()):
        var a: Vector2 = path[i - 1]
        var b: Vector2 = path[i]
        total += a.distance_to(b)
        nodes.append(_build_ribbon(
            _roads_root, a, b,
            _PLAN_WIDTH, strip_color, _ROAD_Y + 0.15, _PLOT_EDGE_STEP))
        # Survey stakes marching along the run (plus one on each corner).
        var stakes := maxi(1, int(a.distance_to(b) / _STAKE_EVERY_M))
        for s2 in range(stakes + 1):
            nodes.append(_plan_stake(a.lerp(b, float(s2) / float(stakes)), stake_color))
    # A floating label at the path's midpoint so the build site announces
    # itself from a distance (same treatment as fixture labels).
    var mid: Vector2 = path[path.size() / 2]
    var label := Label3D.new()
    label.text = "🔨 Demolition — bring a tool kit" if demo else "🚧 Planned road — bring stone"
    label.billboard = BaseMaterial3D.BILLBOARD_ENABLED
    label.no_depth_test = true
    label.fixed_size = true
    label.pixel_size = 0.004
    label.modulate = Color(1.0, 0.85, 0.4)
    label.outline_size = 8
    label.position = Protocol.w2v(mid.x, mid.y, 6.0)
    label.visibility_range_end = 700.0
    label.visibility_range_end_margin = 80.0
    label.visibility_range_fade_mode = GeometryInstance3D.VISIBILITY_RANGE_FADE_SELF
    _roads_root.add_child(label)
    nodes.append(label)
    rec["nodes"] = nodes

## One survey stake, planted on the ground (orange = plan, red = demolition).
func _plan_stake(p: Vector2, color: Color = _STAKE_COLOR) -> MeshInstance3D:
    var mi := MeshInstance3D.new()
    var post := BoxMesh.new()
    post.size = Vector3(0.28, 1.5, 0.28)
    mi.mesh = post
    var m := StandardMaterial3D.new()
    m.albedo_color = color
    m.emission_enabled = true
    m.emission = color
    m.emission_energy_multiplier = 0.25
    mi.material_override = m
    mi.position = Protocol.w2v(p.x, p.y, 0.9)
    _roads_root.add_child(mi)
    return mi

## A terrain-following strip of `width` from `a` to `b`: every cross-section
## is placed with its own `Protocol.w2v` sample, so it drapes over hills and
## edited ground instead of floating/clipping between two flat endpoints.
## Shared by the dirt roads and the plot borders.
func _build_ribbon(parent: Node3D, a: Vector2, b: Vector2, width: float, color: Color,
        y: float, step: float) -> MeshInstance3D:
    var length := a.distance_to(b)
    var segments := maxi(1, ceili(length / step))
    var dir := (b - a).normalized() if length > 0.001 else Vector2.RIGHT
    var perp := Vector2(-dir.y, dir.x) * (width * 0.5)

    var st := SurfaceTool.new()
    st.begin(Mesh.PRIMITIVE_TRIANGLES)
    var prev_left := Vector3.ZERO
    var prev_right := Vector3.ZERO
    for i in range(segments + 1):
        var t := float(i) / float(segments)
        var p := a.lerp(b, t)
        var left := Protocol.w2v(p.x + perp.x, p.y + perp.y, y)
        var right := Protocol.w2v(p.x - perp.x, p.y - perp.y, y)
        if i > 0:
            # Winding: Godot front faces are CLOCKWISE seen from the camera,
            # so an upward-facing strip must wind clockwise viewed from
            # above (the same order `_build_ground`/`_add_plot_fill` use).
            # The original (prev_left, left, right / …) order here was
            # counter-clockwise from above — every ribbon (dirt roads, plot
            # borders, staked plans) rendered only from UNDERNEATH, found
            # via the #99 stake-visibility report.
            st.add_vertex(prev_left)
            st.add_vertex(right)
            st.add_vertex(left)
            st.add_vertex(prev_left)
            st.add_vertex(prev_right)
            st.add_vertex(right)
        prev_left = left
        prev_right = right
    st.index()
    st.generate_normals()

    var strip := MeshInstance3D.new()
    strip.mesh = st.commit()
    var mat := StandardMaterial3D.new()
    mat.albedo_color = color
    if color.a < 1.0:
        # Translucent ribbons (the staked road plans, #95) — opaque callers
        # keep the cheap path.
        mat.transparency = BaseMaterial3D.TRANSPARENCY_ALPHA
    strip.material_override = mat
    parent.add_child(strip)
    return strip

## A terrain-conforming translucent fill over rect `(x0,y0,w,h)`: a small
## per-vertex grid (the same w2v-every-vertex approach as `_build_ground`),
## so the tile hugs the surface instead of burying its corners on sloped or
## brush-edited ground the way the old single flat plane did.
func _add_plot_fill(parent: Node3D, x0: float, y0: float, w: float, h: float,
        color: Color, y: float) -> void:
    var st := SurfaceTool.new()
    st.begin(Mesh.PRIMITIVE_TRIANGLES)
    var sx := w / float(_PLOT_FILL_STEPS)
    var sy := h / float(_PLOT_FILL_STEPS)
    for gy in range(_PLOT_FILL_STEPS):
        for gx in range(_PLOT_FILL_STEPS):
            var wx0 := x0 + gx * sx
            var wy0 := y0 + gy * sy
            var p00 := Protocol.w2v(wx0, wy0, y)
            var p10 := Protocol.w2v(wx0 + sx, wy0, y)
            var p01 := Protocol.w2v(wx0, wy0 + sy, y)
            var p11 := Protocol.w2v(wx0 + sx, wy0 + sy, y)
            st.add_vertex(p00)
            st.add_vertex(p10)
            st.add_vertex(p11)
            st.add_vertex(p00)
            st.add_vertex(p11)
            st.add_vertex(p01)
    st.index()
    st.generate_normals()

    var fill := MeshInstance3D.new()
    fill.mesh = st.commit()
    var mat := StandardMaterial3D.new()
    mat.albedo_color = color
    mat.transparency = BaseMaterial3D.TRANSPARENCY_ALPHA
    fill.material_override = mat
    parent.add_child(fill)

## The four edge ribbons of rect `(x0,y0,w,h)`. The horizontal edges are
## extended by half the border width past each corner so the corners meet
## without notches (the old axis-aligned boxes got this for free).
func _add_plot_border(parent: Node3D, x0: float, y0: float, w: float, h: float,
        bw: float, color: Color) -> void:
    var hw := bw * 0.5
    _build_ribbon(parent, Vector2(x0 - hw, y0), Vector2(x0 + w + hw, y0), bw, color, _ROAD_Y, _PLOT_EDGE_STEP)
    _build_ribbon(parent, Vector2(x0 - hw, y0 + h), Vector2(x0 + w + hw, y0 + h), bw, color, _ROAD_Y, _PLOT_EDGE_STEP)
    _build_ribbon(parent, Vector2(x0, y0), Vector2(x0, y0 + h), bw, color, _ROAD_Y, _PLOT_EDGE_STEP)
    _build_ribbon(parent, Vector2(x0 + w, y0), Vector2(x0 + w, y0 + h), bw, color, _ROAD_Y, _PLOT_EDGE_STEP)

## Draw (or redraw) the player's home plot from a `plot.assigned` `bounds`
## rect: a bright filled outline on the ground plus a tall beacon, so it reads
## as a distinct, findable landmark from across the district (#11).
func show_home_plot(bounds: Dictionary) -> void:
    _home_bounds = bounds
    for child in _home_root.get_children():
        child.queue_free()

    var x0 := float(bounds.get("x", 0))
    var y0 := float(bounds.get("y", 0))
    var w := float(bounds.get("w", 0))
    var h := float(bounds.get("h", 0))
    if w <= 0.0 or h <= 0.0:
        return
    var gold := Color(1.0, 0.82, 0.15)

    _add_plot_fill(_home_root, x0, y0, w, h,
        Color(gold.r, gold.g, gold.b, 0.35), _TILE_Y + 0.01)
    _add_plot_border(_home_root, x0, y0, w, h, 1.5, gold)

    var beacon := MeshInstance3D.new()
    var cyl := CylinderMesh.new()
    cyl.top_radius = 0.3
    cyl.bottom_radius = 0.6
    cyl.height = 14.0
    beacon.mesh = cyl
    beacon.position = Protocol.w2v(x0 + w * 0.5, y0 + h * 0.5, 7.0)
    var beacon_mat := StandardMaterial3D.new()
    beacon_mat.albedo_color = gold
    beacon_mat.emission_enabled = true
    beacon_mat.emission = gold
    beacon_mat.emission_energy_multiplier = 1.5
    beacon.material_override = beacon_mat
    _home_root.add_child(beacon)

    var label := Label3D.new()
    label.text = "Your Plot"
    label.billboard = BaseMaterial3D.BILLBOARD_ENABLED
    label.no_depth_test = true
    label.fixed_size = true
    label.pixel_size = 0.006
    label.modulate = gold
    label.outline_size = 8
    label.position = Protocol.w2v(x0 + w * 0.5, y0 + h * 0.5, 15.0)
    # Homing aid, so it carries further than fixture labels — but still
    # distance-culled: fixed_size + no_depth_test would otherwise render it
    # full-size through 20km of terrain (the HUD's home arrow covers the
    # cross-map case).
    label.visibility_range_end = 2000.0
    label.visibility_range_end_margin = 200.0
    label.visibility_range_fade_mode = GeometryInstance3D.VISIBILITY_RANGE_FADE_SELF
    _home_root.add_child(label)

## Rebuild every *other* plot from a `plot.district` roster (#18) — the
## player's own plot (`my_plot_id`) is skipped, since `show_home_plot` already
## draws it distinctly with a tall beacon; a second flat tile under that would
## just clutter the same spot. Each other plot gets a flat tile + border (no
## beacon — keeps mine the one standout landmark): green with nothing further
## to say if it's free, red with a small signpost naming the owner if it's
## taken.
func apply_plot_roster(plots: Array, my_plot_id: String) -> void:
    _roster_plots = plots
    _my_plot_id = my_plot_id
    for child in _plots_root.get_children():
        child.queue_free()

    for entry_v in plots:
        var p: Dictionary = entry_v
        if String(p.get("plot_id", "")) == my_plot_id:
            continue
        _add_plot_marker(p.get("bounds", {}), p.get("owner_name"))

func _add_plot_marker(bounds: Dictionary, owner_name) -> void:
    var x0 := float(bounds.get("x", 0))
    var y0 := float(bounds.get("y", 0))
    var w := float(bounds.get("w", 0))
    var h := float(bounds.get("h", 0))
    if w <= 0.0 or h <= 0.0:
        return
    var taken := owner_name != null and String(owner_name) != ""
    var tint := Color(0.85, 0.30, 0.25) if taken else Color(0.25, 0.85, 0.35)

    _add_plot_fill(_plots_root, x0, y0, w, h,
        Color(tint.r, tint.g, tint.b, 0.28), _TILE_Y + 0.005)
    _add_plot_border(_plots_root, x0, y0, w, h, 1.0, tint)

    # A free plot just reads as green — nothing more to say about it. Only a
    # taken one gets a signpost naming the owner.
    if taken:
        _add_signpost(x0 + w * 0.5, y0 + h * 0.5, String(owner_name), tint)

## A small sign — a thin post with a name-plank on top — for a taken plot,
## instead of a large floating label; a district full of claimed plots
## shouldn't turn into a wall of oversized text.
func _add_signpost(cx: float, cy: float, owner_name: String, tint: Color) -> void:
    var post := MeshInstance3D.new()
    var post_mesh := CylinderMesh.new()
    post_mesh.top_radius = 0.15
    post_mesh.bottom_radius = 0.15
    post_mesh.height = 2.2
    post.mesh = post_mesh
    post.position = Protocol.w2v(cx, cy, 1.1)
    var post_mat := StandardMaterial3D.new()
    post_mat.albedo_color = Color(0.4, 0.3, 0.22)
    post.material_override = post_mat
    _plots_root.add_child(post)

    var plank := MeshInstance3D.new()
    var plank_mesh := BoxMesh.new()
    plank_mesh.size = Vector3(1.4, 0.6, 0.08)
    plank.mesh = plank_mesh
    plank.position = Protocol.w2v(cx, cy, 2.1)
    var plank_mat := StandardMaterial3D.new()
    plank_mat.albedo_color = tint
    plank.material_override = plank_mat
    _plots_root.add_child(plank)

    var label := Label3D.new()
    label.text = owner_name
    label.billboard = BaseMaterial3D.BILLBOARD_ENABLED
    label.no_depth_test = true
    label.fixed_size = true
    label.pixel_size = 0.0025
    label.modulate = Color(1, 1, 1)
    label.outline_size = 4
    label.position = Protocol.w2v(cx, cy, 2.15)
    # A signpost name only matters standing near the plot — cull it early
    # (fixed_size + no_depth_test would otherwise show every owner's name
    # across the whole district, through terrain).
    label.visibility_range_end = 150.0
    label.visibility_range_end_margin = 30.0
    label.visibility_range_fade_mode = GeometryInstance3D.VISIBILITY_RANGE_FADE_SELF
    _plots_root.add_child(label)

## Just the floating district-name label now — safety is painted straight
## into the ground mesh (`_build_ground`), so there's no separate tile mesh
## to draw here any more.
func _add_district_label(z: Dictionary) -> void:
    var x0 := float(z.get("x0", 0))
    var y0 := float(z.get("y0", 0))
    var x1 := float(z.get("x1", 0))
    var y1 := float(z.get("y1", 0))
    var w := x1 - x0
    var h := y1 - y0
    if w <= 0.0 or h <= 0.0:
        return

    var district_name: String = String(z.get("district", z.get("zone_id", "")))
    if district_name == "":
        return
    var safe := String(z.get("safety", "wilds")) == "safe"
    var label := Label3D.new()
    label.text = district_name
    label.billboard = BaseMaterial3D.BILLBOARD_ENABLED
    label.modulate = Color(0.7, 1.0, 0.8) if safe else Color(1.0, 0.7, 0.7)
    # World-sized on purpose (not fixed_size) so it reads as far-off signage;
    # sized for the metric scene, where a district band is kilometres across.
    label.pixel_size = 0.5
    label.position = Protocol.w2v(x0 + w * 0.5, y0 + h * 0.5, 60.0)
    _tiles_root.add_child(label)
