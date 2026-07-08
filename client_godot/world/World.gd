## The 3D capital: a ground plane, the authored district tiles, and the
## town-centre marker, all rebuilt from the gateway's `partition` message.
##
## The capital starts empty — no roads are authored; this draws the ground and
## districts (named, tinted by safety) plus a town-centre marker. Roads are real,
## buildable content now (mayor-issued dirt paths, see `upsert_dirt_road`), and
## other structures arrive as build orders complete.
class_name World
extends Node3D

const _GROUND_Y := 0.0
const _TILE_Y := 0.02   # district tiles sit just above the ground to avoid z-fighting
const _ROAD_Y := 0.05
const _ROAD_WIDTH := 6.0        # a dirt path is a footpath, not the old avenue
const _ROAD_SEGMENT_STEP := 40.0  # world units per subdivision — short enough to hug hills
const _ROAD_COLOR := Color(0.45, 0.33, 0.20)
const _TILE_SEGMENT_STEP := 160.0  # world units per district-tile subdivision — see _add_district_tile

var world_size := 6400.0

var _ground: MeshInstance3D
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
var _dirt_roads: Dictionary = {}

func _ready() -> void:
    add_child(_tiles_root)
    add_child(_roads_root)
    add_child(_home_root)
    add_child(_plots_root)

## Rebuild the district tiles from a `partition` message; lazily build the static
## ground/roads once the world size and the terrain heightmap are both known.
func apply_partition(msg: Dictionary) -> void:
    world_size = float(msg.get("world", world_size))
    _partition_received = true
    _maybe_build_static()
    _zones = msg.get("zones", [])
    _rebuild_tiles()

## The terrain heightmap (#54) arrived — build the ground/roads if the
## world size (from `partition`) is already known too. `_maybe_build_static`
## only ever builds once both flags are set, so the ground is never built
## with the flat `0.0` fallback and in need of a later redraw.
func on_terrain_data() -> void:
    _terrain_ready = true
    _maybe_build_static()

func _maybe_build_static() -> void:
    if _built_static or not _partition_received or not _terrain_ready:
        return
    _build_ground()
    _build_town_centre_marker()
    _built_static = true

func _rebuild_tiles() -> void:
    for child in _tiles_root.get_children():
        child.queue_free()

    for entry_v in _zones:
        var z: Dictionary = entry_v
        _add_district_tile(z)

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
## `Protocol.terrain_height`'s gentle rolling hills instead of looking dead
## flat — every vertex is placed via the same `w2v` everything else already
## goes through, so it's automatically consistent with where props/entities
## sit on it.
func _build_ground() -> void:
    var st := SurfaceTool.new()
    st.begin(Mesh.PRIMITIVE_TRIANGLES)
    # Must match the server's grid exactly (#54) — the client's height
    # lookups (`Protocol.terrain_height`) interpolate the *same* received
    # grid, so a mismatched local resolution here would make the rendered
    # surface disagree with where entities are placed on it.
    var resolution := Protocol.terrain_resolution()
    var step := world_size / float(resolution)
    for gy in range(resolution):
        for gx in range(resolution):
            var wx0 := gx * step
            var wy0 := gy * step
            var wx1 := wx0 + step
            var wy1 := wy0 + step
            var p00 := Protocol.w2v(wx0, wy0, _GROUND_Y)
            var p10 := Protocol.w2v(wx1, wy0, _GROUND_Y)
            var p01 := Protocol.w2v(wx0, wy1, _GROUND_Y)
            var p11 := Protocol.w2v(wx1, wy1, _GROUND_Y)
            # Two triangles per grid cell, wound so generated normals point
            # up (verified by tests/smoke_terrain.gd — Godot's generate_normals
            # gave downward-facing normals for the opposite order).
            st.add_vertex(p00)
            st.add_vertex(p10)
            st.add_vertex(p11)
            st.add_vertex(p00)
            st.add_vertex(p11)
            st.add_vertex(p01)
    st.index() # share vertices between adjacent triangles for smooth normals
    st.generate_normals()

    _ground = MeshInstance3D.new()
    _ground.mesh = st.commit()
    var mat := StandardMaterial3D.new()
    mat.albedo_color = Color(0.10, 0.14, 0.10)
    # The two triangles per cell (p00,p11,p10 / p00,p01,p11) already wind so
    # generated normals point up (verified by tests/smoke_terrain.gd) — no
    # need for BaseMaterial3D.CULL_DISABLED, which was a previous defensive
    # guess and, by letting backfaces render, is the likely cause of the
    # washed-out/"translucent" look reported against the noise-based terrain.
    _ground.material_override = mat
    add_child(_ground)

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

## Render a completed `dirt_road` build order — a segment from `(x,y)` to
## `(x1,y1)` — as a real road: a multi-vertex ribbon, not a single rigid box.
## Every cross-section along the segment is placed with its own `Protocol.w2v`
## sample (exactly like `_build_ground`'s per-vertex grid), so the path actually
## follows the terrain's hills its whole length instead of floating/clipping
## between two flat endpoints. `id` is the `status_update` id
## (`structure_<build order kind>`) — idempotent, since a road never moves.
func upsert_dirt_road(id: String, state: Dictionary) -> void:
    if _dirt_roads.has(id):
        return
    var a := Vector2(float(state.get("x", 0)), float(state.get("y", 0)))
    var b := Vector2(float(state.get("x1", a.x)), float(state.get("y1", a.y)))
    _dirt_roads[id] = _build_road_ribbon(a, b)

func _build_road_ribbon(a: Vector2, b: Vector2) -> MeshInstance3D:
    var length := a.distance_to(b)
    var segments := maxi(1, ceili(length / _ROAD_SEGMENT_STEP))
    var dir := (b - a).normalized() if length > 0.001 else Vector2.RIGHT
    var perp := Vector2(-dir.y, dir.x) * (_ROAD_WIDTH * 0.5)

    var st := SurfaceTool.new()
    st.begin(Mesh.PRIMITIVE_TRIANGLES)
    var prev_left := Vector3.ZERO
    var prev_right := Vector3.ZERO
    for i in range(segments + 1):
        var t := float(i) / float(segments)
        var p := a.lerp(b, t)
        var left := Protocol.w2v(p.x + perp.x, p.y + perp.y, _ROAD_Y)
        var right := Protocol.w2v(p.x - perp.x, p.y - perp.y, _ROAD_Y)
        if i > 0:
            # Mirrors `_build_ground`'s winding (p00,p10,p11 / p00,p11,p01) so
            # generated normals point up here too.
            st.add_vertex(prev_left)
            st.add_vertex(left)
            st.add_vertex(right)
            st.add_vertex(prev_left)
            st.add_vertex(right)
            st.add_vertex(prev_right)
        prev_left = left
        prev_right = right
    st.index()
    st.generate_normals()

    var strip := MeshInstance3D.new()
    strip.mesh = st.commit()
    var mat := StandardMaterial3D.new()
    mat.albedo_color = _ROAD_COLOR
    strip.material_override = mat
    _roads_root.add_child(strip)
    return strip

func _add_strip(parent: Node3D, a: Vector2, b: Vector2, width: float, color: Color) -> void:
    var strip := MeshInstance3D.new()
    var box := BoxMesh.new()
    var length := a.distance_to(b)
    # Horizontal strip if a.y == b.y, else vertical — both axis-aligned here.
    if absf(a.y - b.y) < 0.5:
        box.size = Vector3(length, 0.04, width) * Protocol.WORLD_SCALE
    else:
        box.size = Vector3(width, 0.04, length) * Protocol.WORLD_SCALE
    strip.mesh = box
    var mid := (a + b) * 0.5
    strip.position = Protocol.w2v(mid.x, mid.y, _ROAD_Y)
    var mat := StandardMaterial3D.new()
    mat.albedo_color = color
    strip.material_override = mat
    parent.add_child(strip)

## Draw (or redraw) the player's home plot from a `plot.assigned` `bounds`
## rect: a bright filled outline on the ground plus a tall beacon, so it reads
## as a distinct, findable landmark from across the district (#11).
func show_home_plot(bounds: Dictionary) -> void:
    for child in _home_root.get_children():
        child.queue_free()

    var x0 := float(bounds.get("x", 0))
    var y0 := float(bounds.get("y", 0))
    var w := float(bounds.get("w", 0))
    var h := float(bounds.get("h", 0))
    if w <= 0.0 or h <= 0.0:
        return
    var gold := Color(1.0, 0.82, 0.15)

    var fill := MeshInstance3D.new()
    var plane := PlaneMesh.new()
    plane.size = Vector2(w, h) * Protocol.WORLD_SCALE
    fill.mesh = plane
    fill.position = Protocol.w2v(x0 + w * 0.5, y0 + h * 0.5, _TILE_Y + 0.01)
    var fill_mat := StandardMaterial3D.new()
    fill_mat.albedo_color = Color(gold.r, gold.g, gold.b, 0.35)
    fill_mat.transparency = BaseMaterial3D.TRANSPARENCY_ALPHA
    fill.material_override = fill_mat
    _home_root.add_child(fill)

    var bw := 1.5
    _add_strip(_home_root, Vector2(x0, y0), Vector2(x0 + w, y0), bw, gold)
    _add_strip(_home_root, Vector2(x0, y0 + h), Vector2(x0 + w, y0 + h), bw, gold)
    _add_strip(_home_root, Vector2(x0, y0), Vector2(x0, y0 + h), bw, gold)
    _add_strip(_home_root, Vector2(x0 + w, y0), Vector2(x0 + w, y0 + h), bw, gold)

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
    _home_root.add_child(label)

## Rebuild every *other* plot from a `plot.district` roster (#18) — the
## player's own plot (`my_plot_id`) is skipped, since `show_home_plot` already
## draws it distinctly with a tall beacon; a second flat tile under that would
## just clutter the same spot. Each other plot gets a flat tile + border (no
## beacon — keeps mine the one standout landmark): green with nothing further
## to say if it's free, red with a small signpost naming the owner if it's
## taken.
func apply_plot_roster(plots: Array, my_plot_id: String) -> void:
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

    var fill := MeshInstance3D.new()
    var plane := PlaneMesh.new()
    plane.size = Vector2(w, h) * Protocol.WORLD_SCALE
    fill.mesh = plane
    fill.position = Protocol.w2v(x0 + w * 0.5, y0 + h * 0.5, _TILE_Y + 0.005)
    var fill_mat := StandardMaterial3D.new()
    fill_mat.albedo_color = Color(tint.r, tint.g, tint.b, 0.28)
    fill_mat.transparency = BaseMaterial3D.TRANSPARENCY_ALPHA
    fill.material_override = fill_mat
    _plots_root.add_child(fill)

    var bw := 1.0
    _add_strip(_plots_root, Vector2(x0, y0), Vector2(x0 + w, y0), bw, tint)
    _add_strip(_plots_root, Vector2(x0, y0 + h), Vector2(x0 + w, y0 + h), bw, tint)
    _add_strip(_plots_root, Vector2(x0, y0), Vector2(x0, y0 + h), bw, tint)
    _add_strip(_plots_root, Vector2(x0 + w, y0), Vector2(x0 + w, y0 + h), bw, tint)

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
    _plots_root.add_child(label)

## A district tile can span a large fraction of the whole world (districts
## are quadrants/halves of `WORLD_SIZE`) -- with real DEM terrain (issue #69)
## carrying far more relief than the old synthetic placeholder, a single flat
## `PlaneMesh` sampled at just the tile's center visibly floats above/through
## the real ground everywhere else in the tile. Subdivided into a
## terrain-following grid instead, the same technique `_build_ground` and
## `_build_road_ribbon` already use, so the tint overlay actually hugs the
## surface underneath it.
func _add_district_tile(z: Dictionary) -> void:
    var x0 := float(z.get("x0", 0))
    var y0 := float(z.get("y0", 0))
    var x1 := float(z.get("x1", 0))
    var y1 := float(z.get("y1", 0))
    var w := x1 - x0
    var h := y1 - y0
    if w <= 0.0 or h <= 0.0:
        return

    var cols := maxi(1, ceili(w / _TILE_SEGMENT_STEP))
    var rows := maxi(1, ceili(h / _TILE_SEGMENT_STEP))
    var st := SurfaceTool.new()
    st.begin(Mesh.PRIMITIVE_TRIANGLES)
    for gy in range(rows):
        for gx in range(cols):
            var wx0 := x0 + w * (float(gx) / float(cols))
            var wy0 := y0 + h * (float(gy) / float(rows))
            var wx1 := x0 + w * (float(gx + 1) / float(cols))
            var wy1 := y0 + h * (float(gy + 1) / float(rows))
            var p00 := Protocol.w2v(wx0, wy0, _TILE_Y)
            var p10 := Protocol.w2v(wx1, wy0, _TILE_Y)
            var p01 := Protocol.w2v(wx0, wy1, _TILE_Y)
            var p11 := Protocol.w2v(wx1, wy1, _TILE_Y)
            st.add_vertex(p00)
            st.add_vertex(p10)
            st.add_vertex(p11)
            st.add_vertex(p00)
            st.add_vertex(p11)
            st.add_vertex(p01)
    st.index()
    st.generate_normals()

    var tile := MeshInstance3D.new()
    tile.mesh = st.commit()

    var safe := String(z.get("safety", "wilds")) == "safe"
    var mat := StandardMaterial3D.new()
    mat.albedo_color = (Color(0.12, 0.30, 0.18, 0.5) if safe
        else Color(0.30, 0.12, 0.12, 0.5))
    mat.transparency = BaseMaterial3D.TRANSPARENCY_ALPHA
    tile.material_override = mat
    _tiles_root.add_child(tile)

    var district_name: String = String(z.get("district", z.get("zone_id", "")))
    if district_name != "":
        var label := Label3D.new()
        label.text = district_name
        label.billboard = BaseMaterial3D.BILLBOARD_ENABLED
        label.modulate = Color(0.7, 1.0, 0.8) if safe else Color(1.0, 0.7, 0.7)
        label.pixel_size = 0.05
        label.position = Protocol.w2v(x0 + w * 0.5, y0 + h * 0.5, 6.0)
        _tiles_root.add_child(label)
