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
## Move mode (#105): an existing open plan re-routed — Main wires this to
## `NetworkClient.send_road_replan`.
signal replan_committed(order_id: String, points: Array)
signal status_changed(text: String)
## true while the tool owns the mouse — the toolbar disables the other tools.
signal mode_changed(active: bool)

const _GHOST_COLOR := Color(0.95, 0.8, 0.25, 0.5)
const _GHOST_Y := 0.35
## Ghost strips sample the ground every few metres so long runs follow
## slopes instead of floating.
const _GHOST_STEP_M := 4.0

## Move-mode pick radius: click within this of a staked plan's runs to
## select it (matches the feel of the object tool's delete pick).
const MOVE_PICK_RADIUS := 10.0

var camera: Camera3D
## False while another editor tool (brush stroke in flight, object tool
## active) owns the mouse.
var enabled := true
## The staked-plan source for move-mode picking (#105) — Main hands the
## World in; the tool only reads `_road_plans`' paths.
var world_ref: World

var active := false
## Move mode (#105): [M] — clicks pick an existing plan instead of
## anchoring a new one; the picked polyline loads into this same laying
## machine and commits as a `road.replan`.
var move_mode := false
var editing_order_id := ""
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
		if active and not move_mode:
			_set_state(false, false)
		else:
			_set_state(true, false)
	if _key_edge(KEY_M):
		if active and move_mode:
			_set_state(false, false)
		else:
			_set_state(true, true)
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
		if move_mode and editing_order_id == "":
			pick_at(_last_ground)
		elif points.is_empty():
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
	_set_state(on, false)

## Enter/leave move mode (#105) — the toolbar's "Move" slot.
func set_move_active(on: bool) -> void:
	_set_state(on, on)

func _set_state(on: bool, mv: bool) -> void:
	if on == active and (mv == move_mode or not on):
		return
	active = on
	move_mode = mv if on else false
	points.clear()
	editing_order_id = ""
	_clear_ghost()
	if not active:
		status_changed.emit("Road tool off")
	elif move_mode:
		status_changed.emit("Move: click within %dm of a staked plan to pick it up" % int(MOVE_PICK_RADIUS))
	else:
		status_changed.emit("Road: click to anchor, click to corner, [Enter] commit, [Esc] cancel, [Backspace] undo corner")
	mode_changed.emit(active)

## Move mode: pick the staked plan nearest the click (#105) and load its
## polyline into the laying machine — from here on Backspace/clicks/Enter
## behave exactly as when laying, but commit as a replan. The original
## stakes stay rendered underneath as the reference until the server's
## board broadcast re-stakes the moved plan.
func pick_at(ground: Vector2) -> void:
	if world_ref == null:
		return
	var id := pick_plan(world_ref._road_plans, ground, MOVE_PICK_RADIUS)
	if id == "":
		status_changed.emit("Move: no planned road there — click within %dm of a staked plan" % int(MOVE_PICK_RADIUS))
		return
	var path: Array = world_ref._road_plans[id]["path"]
	points.clear()
	for p in path:
		points.append(Vector2i(p))
	editing_order_id = id
	_preview_end = points[-1]
	_rebuild_ghost()
	status_changed.emit("Move: editing plan %s (%dm) — click corners to extend, [Backspace] to trim, [Enter] commit, [Esc] drop" % [id.substr(0, 8), path_length(points)])

## The staked plan whose runs pass nearest `g` (within `max_d`), or "".
## `plans` is World's `_road_plans` shape: id -> {"path": [Vector2, ...]}.
static func pick_plan(plans: Dictionary, g: Vector2, max_d: float) -> String:
	var best := ""
	var best_d := max_d
	for id in plans:
		var path: Array = plans[id]["path"]
		for i in range(1, path.size()):
			var c := Geometry2D.get_closest_point_to_segment(g, path[i - 1], path[i])
			var d := g.distance_to(c)
			if d <= best_d:
				best_d = d
				best = id
	return best

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
	if editing_order_id != "":
		replan_committed.emit(editing_order_id, out)
		status_changed.emit("Move: replan submitted (%dm, ~%d stone)" % [path_length(points), stone_cost(path_length(points))])
		editing_order_id = ""
	else:
		plan_committed.emit(out)
		status_changed.emit("Road: plan submitted (%dm, ~%d stone)" % [path_length(points), stone_cost(path_length(points))])
	points.clear()
	_clear_ghost()

func cancel() -> void:
	points.clear()
	editing_order_id = ""
	_clear_ghost()
	status_changed.emit("Road: cancelled")

# --- snapping / cost math (static: headless-testable) --------------------------

## Nearest lattice point to a ground position.
static func snap_lattice(g: Vector2) -> Vector2i:
	return Vector2i(roundi(g.x), roundi(g.y))

## The pending leg's endpoint: the cursor snapped to the lattice — segments
## run at ANY angle now (#112; the axis-dominant snap died with the
## staircase roads). `_last` only matters for the no-op-corner equality
## check in `add_corner`.
static func snap_next_point(_last: Vector2i, ground: Vector2) -> Vector2i:
	return snap_lattice(ground)

## Total chord length of a corner list, in metres (Euclidean — mirrors the
## server's #111 pricing; identical to Manhattan for axis-aligned paths).
static func path_length(corners: Array) -> int:
	var total := 0.0
	for i in range(1, corners.size()):
		total += Vector2(corners[i - 1]).distance_to(Vector2(corners[i]))
	return roundi(total)

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

## Rebuild the ghost: the SAME smoothed spline the world will stake/build
## (#112 — World.sample_spline through committed corners + the pending
## point), as short ground-sampled slabs so it follows the terrain.
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
	var path: Array = []
	for c in corners:
		path.append(Vector2(c))
	var samples := World.sample_spline(path, _GHOST_STEP_M)
	for i in range(1, samples.size()):
		_ghost_slab(samples[i - 1], samples[i], 1.2)

func _ghost_slab(a: Vector2, b: Vector2, width: float) -> void:
	var mi := MeshInstance3D.new()
	var box := BoxMesh.new()
	var length := a.distance_to(b)
	box.size = Vector3(maxf(length, 0.5), 0.08, width)
	mi.mesh = box
	var mat := StandardMaterial3D.new()
	mat.albedo_color = _GHOST_COLOR
	mat.transparency = BaseMaterial3D.TRANSPARENCY_ALPHA
	mi.material_override = mat
	var mid := (a + b) * 0.5
	mi.position = Protocol.w2v(mid.x, mid.y, _GHOST_Y)
	# Segments run at any angle now: yaw the slab along a->b (world y is
	# scene z, so the 2D angle maps to a negative yaw).
	mi.rotation.y = -(b - a).angle()
	_ghost_root.add_child(mi)

func _key_edge(key: Key) -> bool:
	var down := Input.is_physical_key_pressed(key)
	var was: bool = _key_down.get(key, false)
	_key_down[key] = down
	return down and not was
