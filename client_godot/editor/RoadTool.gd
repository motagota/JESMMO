## Road-laying for `--editor-mode` (#95): [R] toggles the tool; click anchors
## the path, further clicks add corners — the pending leg snaps to the 1m
## lattice and to an axis-aligned run along the dominant axis of the cursor
## (classic RTS wall-laying: bend the pointer, click, and the corner is
## committed). Enter submits the whole path as ONE `road.plan`, Esc cancels,
## Backspace removes the last corner.
##
## A translucent ghost previews every committed run plus the pending leg,
## with a live length/stone-cost readout (Protocol's display-only mirror of
## the server consts — the server's number is authoritative). Nothing is
## rendered permanently by this tool: the accepted plan comes back through
## the ordinary `build.list` broadcast and `World.apply_road_plans` stakes it
## for everyone (the object-tool reconcile philosophy).
##
## Input handling is a thin poll over testable methods (`anchor`,
## `add_corner`, `commit`, `cancel`, and the static snapping math) so the
## headless smoke test drives the same state machine the mouse does.
class_name RoadTool
extends Node3D

signal plan_committed(points: Array)
signal status_changed(text: String)
## true while the tool owns the mouse — Main disables the brush/object tool.
signal mode_changed(active: bool)

const _GHOST_COLOR := Color(0.95, 0.8, 0.25, 0.5)
const _GHOST_Y := 0.35
## Ghost strips sample the ground every few metres so long runs follow
## slopes instead of floating.
const _GHOST_STEP_M := 4.0

var camera: Camera3D
## False while another editor tool (brush stroke in flight, object tool
## active) owns the mouse.
var enabled := true

var active := false
var points: Array = [] # committed Vector2i lattice corners
var _preview_end := Vector2i.ZERO
var _last_ground := Vector2.ZERO
var _ghost_root: Node3D
var _lmb_down := false
var _key_down: Dictionary = {}

func _init() -> void:
	_ghost_root = Node3D.new()
	add_child(_ghost_root)

func _process(_delta: float) -> void:
	if not enabled:
		return
	if _key_edge(KEY_R):
		set_active(not active)
	if not active or camera == null:
		return
	_last_ground = Protocol.pick_ground(camera, get_viewport().get_mouse_position(), _last_ground)
	if not points.is_empty():
		var snapped := snap_next_point(points[-1], _last_ground)
		if snapped != _preview_end:
			_preview_end = snapped
			_rebuild_ghost()
	# Clicks over UI (the editor toolbar, #103) are button presses, not
	# corners — the raw poll can't tell the difference on its own.
	var lmb := Input.is_mouse_button_pressed(MOUSE_BUTTON_LEFT) \
		and get_viewport().gui_get_hovered_control() == null
	if lmb and not _lmb_down:
		if points.is_empty():
			anchor(_last_ground)
		else:
			add_corner(_last_ground)
	_lmb_down = lmb
	if _key_edge(KEY_ENTER):
		commit()
	if _key_edge(KEY_ESCAPE):
		cancel()
	if _key_edge(KEY_BACKSPACE) and points.size() > 1:
		points.pop_back()
		_preview_end = snap_next_point(points[-1], _last_ground)
		_rebuild_ghost()
		_announce_progress()

func set_active(on: bool) -> void:
	if on == active:
		return
	active = on
	if not active:
		points.clear()
		_clear_ghost()
	status_changed.emit(
		"Road: click to anchor, click to corner, [Enter] commit, [Esc] cancel, [Backspace] undo corner"
		if active else "Road tool off")
	mode_changed.emit(active)

## First click: the path's start, snapped to the lattice.
func anchor(ground: Vector2) -> void:
	points = [snap_lattice(ground)]
	_preview_end = points[0]
	_rebuild_ghost()
	_announce_progress()

## Later clicks: commit the pending snapped leg as a corner (no-op if the
## cursor hasn't left the last corner).
func add_corner(ground: Vector2) -> void:
	var next := snap_next_point(points[-1], ground)
	if next == points[-1]:
		return
	points.append(next)
	_preview_end = next
	_rebuild_ghost()
	_announce_progress()

## Submit the committed corners as one road.plan (the pending un-clicked leg
## is deliberately NOT included — what you clicked is what you get).
func commit() -> void:
	if points.size() < 2:
		status_changed.emit("Road: need at least one run — click a corner before committing")
		return
	var out: Array = []
	for p in points:
		out.append([p.x, p.y])
	plan_committed.emit(out)
	status_changed.emit("Road: plan submitted (%dm, ~%d stone)" % [path_length(points), stone_cost(path_length(points))])
	points.clear()
	_clear_ghost()

func cancel() -> void:
	points.clear()
	_clear_ghost()
	status_changed.emit("Road: cancelled")

# --- snapping / cost math (static: headless-testable) --------------------------

## Nearest lattice point to a ground position.
static func snap_lattice(g: Vector2) -> Vector2i:
	return Vector2i(roundi(g.x), roundi(g.y))

## The pending leg's endpoint: from `last`, an axis-aligned run along the
## dominant axis of the cursor offset, snapped to the lattice.
static func snap_next_point(last: Vector2i, ground: Vector2) -> Vector2i:
	var d := ground - Vector2(last)
	if absf(d.x) >= absf(d.y):
		return Vector2i(last.x + roundi(d.x), last.y)
	return Vector2i(last.x, last.y + roundi(d.y))

## Total run length of a corner list, in metres.
static func path_length(corners: Array) -> int:
	var total := 0
	for i in range(1, corners.size()):
		var a: Vector2i = corners[i - 1]
		var b: Vector2i = corners[i]
		total += absi(b.x - a.x) + absi(b.y - a.y)
	return total

## Display-only mirror of the server's cost rule.
static func stone_cost(length_m: int) -> int:
	return maxi(length_m / Protocol.ROAD_STONE_PER_M_DEN, Protocol.ROAD_MIN_STONE)

# --- ghost ----------------------------------------------------------------------

func _announce_progress() -> void:
	var len_committed := path_length(points)
	var preview := points.duplicate()
	if _preview_end != points[-1]:
		preview.append(_preview_end)
	status_changed.emit("Road: %dm laid (~%d stone), leg to (%d, %d) — [Enter] commit" % [
		len_committed, stone_cost(maxi(len_committed, 1)), _preview_end.x, _preview_end.y])

func _clear_ghost() -> void:
	for c in _ghost_root.get_children():
		c.queue_free()

## Rebuild the ghost strips: every committed run plus the pending leg, as
## short ground-sampled slabs so they follow the terrain.
func _rebuild_ghost() -> void:
	_clear_ghost()
	var corners := points.duplicate()
	if corners.is_empty():
		return
	if _preview_end != corners[-1]:
		corners.append(_preview_end)
	if corners.size() == 1:
		_ghost_slab(Vector2(corners[0]), Vector2(corners[0]) + Vector2(1, 0), 1.0)
		return
	for i in range(1, corners.size()):
		var a := Vector2(corners[i - 1])
		var b := Vector2(corners[i])
		var length := a.distance_to(b)
		var chunks := maxi(1, ceili(length / _GHOST_STEP_M))
		for c in range(chunks):
			var t0 := float(c) / float(chunks)
			var t1 := float(c + 1) / float(chunks)
			_ghost_slab(a.lerp(b, t0), a.lerp(b, t1), 1.2)

func _ghost_slab(a: Vector2, b: Vector2, width: float) -> void:
	var mi := MeshInstance3D.new()
	var box := BoxMesh.new()
	var horizontal := absf(b.x - a.x) >= absf(b.y - a.y)
	var length := a.distance_to(b)
	box.size = Vector3(length, 0.08, width) if horizontal else Vector3(width, 0.08, length)
	mi.mesh = box
	var mat := StandardMaterial3D.new()
	mat.albedo_color = _GHOST_COLOR
	mat.transparency = BaseMaterial3D.TRANSPARENCY_ALPHA
	mi.material_override = mat
	var mid := (a + b) * 0.5
	mi.position = Protocol.w2v(mid.x, mid.y, _GHOST_Y)
	_ghost_root.add_child(mi)

func _key_edge(key: Key) -> bool:
	var down := Input.is_physical_key_pressed(key)
	var was: bool = _key_down.get(key, false)
	_key_down[key] = down
	return down and not was
