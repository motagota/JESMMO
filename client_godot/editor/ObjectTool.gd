## Object placement/deletion for `--editor-mode` (#86): [O] cycles the tool
## through off → place → delete → off. In *place* mode a translucent ghost
## of the poison tree tracks the cursor and LMB sends one `object.place`; in
## *delete* mode LMB picks the nearest placed object within a small radius
## and sends `object.delete`. While the tool is on, the terrain brush is
## disabled (Main wires `mode_changed` to `BrushController.set_enabled`) so
## a placement click can't also carve the ground.
##
## Nothing is rendered locally on click — the server's `object.placed` /
## `object.removed` broadcast is what updates `WorldObjects` (render acks,
## the same reconcile shape as brush strokes).
class_name ObjectTool
extends Node3D

signal place_requested(kind: String, x: int, y: int)
signal delete_requested(object_id: String)
signal status_changed(text: String)
## "off" | "place" | "delete" — Main disables the terrain brush while != off.
signal mode_changed(mode: String)

## v1 places the one registered kind; a future kind picker just swaps this.
const KIND := "poison_tree"
## Delete-mode pick radius (world metres) around the clicked ground point —
## generous enough to click a canopy's footprint, small enough to never grab
## a tree across a clearing.
const DELETE_PICK_RADIUS := 6.0

var camera: Camera3D
var objects: WorldObjects

var mode := "off"
var _ghost: Node3D
var _last_ground := Vector2.ZERO
var _lmb_down := false
var _key_down := false

func _process(_delta: float) -> void:
	_handle_mode_key()
	if camera == null or mode == "off":
		return
	var g := Protocol.pick_ground(camera, get_viewport().get_mouse_position(), _last_ground)
	_last_ground = g
	if _ghost != null:
		_ghost.position = Protocol.w2v(g.x, g.y)
	var lmb := Input.is_mouse_button_pressed(MOUSE_BUTTON_LEFT)
	if lmb and not _lmb_down:
		_click(g)
	_lmb_down = lmb

func _click(g: Vector2) -> void:
	match mode:
		"place":
			place_requested.emit(KIND, int(round(g.x)), int(round(g.y)))
		"delete":
			if objects == null:
				return
			var id := objects.object_at(g, DELETE_PICK_RADIUS)
			if id == "":
				status_changed.emit("Objects: nothing to delete there")
			else:
				delete_requested.emit(id)

func _handle_mode_key() -> void:
	var down := Input.is_physical_key_pressed(KEY_O)
	if down and not _key_down:
		match mode:
			"off":
				set_mode("place")
			"place":
				set_mode("delete")
			_:
				set_mode("off")
	_key_down = down

func set_mode(m: String) -> void:
	if m == mode:
		return
	mode = m
	if _ghost != null:
		_ghost.queue_free()
		_ghost = null
	if mode == "place":
		_ghost = WorldObjects.make_object_node(KIND, true)
		add_child(_ghost)
		_ghost.position = Protocol.w2v(_last_ground.x, _last_ground.y)
	match mode:
		"place":
			status_changed.emit("Objects: PLACE %s — click the ground ([O] for delete)" % KIND)
		"delete":
			status_changed.emit("Objects: DELETE — click a placed object ([O] to exit)")
		_:
			status_changed.emit("Objects: tool off — terrain brush active")
	mode_changed.emit(mode)
