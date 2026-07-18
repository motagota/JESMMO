## Road removal for `--editor-mode` (#107): [X] toggles the tool; click near
## a staked plan OR a built road to target it, click the SAME target again
## to confirm — removal is consequential, so one click never fires. The tool
## routes the request itself: a pristine plan (zero contributed stone) goes
## as a free `road.cancel`; anything with stone in it — part-built plans and
## built roads — goes as `road.demolish`, posting the tool-kit salvage job.
## Demolition orders (kind `demo_*`) are not valid targets.
##
## Nothing is removed locally: the board broadcast un-stakes cancelled
## plans, and a demolished road's structure `despawn` un-renders the ribbon
## (the reconcile philosophy, as with every editor tool).
class_name DemolishTool
extends Node3D

signal cancel_requested(order_id: String)
signal demolish_requested(order_id: String)
signal status_changed(text: String)
signal mode_changed(active: bool)

const PICK_RADIUS := 10.0

var camera: Camera3D
var world_ref: World
## False while another editor tool owns the mouse (the toolbar drives this).
var enabled := true

var active := false
var _pending := "" # order_id awaiting the confirmation click
var _pending_built := false
var _pending_progress := 0
var _last_ground := Vector2.ZERO
var _lmb_down := false
var _key_down: Dictionary = {}

func _process(_delta: float) -> void:
	if not enabled:
		return
	if _key_edge(KEY_X):
		set_active(not active)
	if not active or camera == null:
		return
	_last_ground = Protocol.pick_ground(camera, get_viewport().get_mouse_position(), _last_ground)
	var lmb := Input.is_mouse_button_pressed(MOUSE_BUTTON_LEFT) \
		and get_viewport().gui_get_hovered_control() == null
	if lmb and not _lmb_down:
		click(_last_ground)
	_lmb_down = lmb

func set_active(on: bool) -> void:
	if on == active:
		return
	active = on
	_pending = ""
	status_changed.emit(
		"Demolish: click a staked plan or a built road, click again to confirm"
		if active else "Demolish tool off")
	mode_changed.emit(active)

## One click: target or confirm. Testable without input.
func click(g: Vector2) -> void:
	if world_ref == null:
		return
	var target := pick_target(g)
	if target.is_empty():
		_pending = ""
		status_changed.emit("Demolish: nothing there — click within %dm of a plan or built road" % int(PICK_RADIUS))
		return
	var id: String = target["order_id"]
	if id == _pending:
		_pending = ""
		if _pending_built or _pending_progress > 0:
			demolish_requested.emit(id)
			status_changed.emit("Demolish: salvage job requested for %s" % id.substr(0, 8))
		else:
			cancel_requested.emit(id)
			status_changed.emit("Demolish: cancelling pristine plan %s" % id.substr(0, 8))
		return
	_pending = id
	_pending_built = target["built"]
	_pending_progress = target["progress"]
	if _pending_built:
		status_changed.emit("Demolish BUILT road %s — click again to post the salvage job (tool kit needed; stone refunds)" % id.substr(0, 8))
	elif _pending_progress > 0:
		status_changed.emit("Demolish plan %s (%d stone in) — click again to post the salvage job" % [id.substr(0, 8), _pending_progress])
	else:
		status_changed.emit("Cancel pristine plan %s — click again to confirm (free, nothing built)" % id.substr(0, 8))

## The nearest removable target within PICK_RADIUS: staked plans (except
## demolition orders themselves) and built roads (from the board's completed
## road orders). Returns {} or {order_id, built, progress}.
func pick_target(g: Vector2) -> Dictionary:
	var best := {}
	var best_d := PICK_RADIUS
	for id in world_ref._road_plans:
		var rec: Dictionary = world_ref._road_plans[id]
		if String(rec.get("kind", "")).begins_with("demo_"):
			continue # a demolition can't be demolished
		var d := _path_distance(rec["path"], g)
		if d <= best_d:
			best_d = d
			best = {"order_id": id, "built": false, "progress": int(rec.get("progress_total", 0))}
	for id in world_ref._completed_road_orders:
		var d := _path_distance(world_ref._completed_road_orders[id]["path"], g)
		if d <= best_d:
			best_d = d
			best = {"order_id": id, "built": true, "progress": 0}
	return best

static func _path_distance(path: Array, g: Vector2) -> float:
	var best := INF
	for i in range(1, path.size()):
		var c := Geometry2D.get_closest_point_to_segment(g, path[i - 1], path[i])
		best = minf(best, g.distance_to(c))
	return best

func _key_edge(key: Key) -> bool:
	var down := Input.is_physical_key_pressed(key)
	var was: bool = _key_down.get(key, false)
	_key_down[key] = down
	return down and not was
