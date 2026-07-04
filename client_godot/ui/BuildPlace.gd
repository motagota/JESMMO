## Build/place mode for home structures (#12): press B to toggle, Tab to cycle
## the structure kind, R to rotate 90°, Enter to confirm, Esc to cancel. The
## ghost previews at the player's current position, snapped to a grid — no
## aiming/raycasting, matching the issue's "keep it forgiving" guidance. Bounds/
## overlap/ownership are validated server-side; this is just the UX.
class_name BuildPlace
extends Node3D

signal do_place(kind: String, x: int, y: int, rot: int)
## Fired whenever the mode/kind/rotation changes, so the HUD can show a hint.
signal mode_changed(active: bool, kind: String, rot: int)

const _KINDS := ["bed", "storage", "crafting"]
const _COLORS := {
	"bed": Color(0.7, 0.5, 0.9, 0.5),
	"storage": Color(0.9, 0.75, 0.3, 0.5),
	"crafting": Color(0.9, 0.55, 0.25, 0.5),
}

var active := false
## The player's current world position, kept in sync by `Main` every frame.
var player_pos := Vector2(3200, 3200)

var _kind_index := 0
var _rot := 0
var _ghost: MeshInstance3D

var _b_down := false
var _tab_down := false
var _r_down := false
var _enter_down := false

func _ready() -> void:
	_ghost = MeshInstance3D.new()
	_ghost.visible = false
	add_child(_ghost)
	_rebuild_ghost()

func current_kind() -> String:
	return _KINDS[_kind_index]

func _process(_delta: float) -> void:
	_poll_keys()
	if active:
		_ghost.visible = true
		var snapped := _snapped_pos()
		global_position = Protocol.w2v(snapped.x, snapped.y, 0.05)
		_ghost.rotation_degrees = Vector3(0, _rot, 0)
	else:
		_ghost.visible = false

func _snapped_pos() -> Vector2:
	var step := float(Protocol.PLACE_GRID)
	return Vector2(round(player_pos.x / step) * step, round(player_pos.y / step) * step)

func _poll_keys() -> void:
	var b := Input.is_physical_key_pressed(KEY_B)
	if b and not _b_down:
		active = not active
		mode_changed.emit(active, current_kind(), _rot)
	_b_down = b

	if not active:
		_tab_down = Input.is_physical_key_pressed(KEY_TAB)
		_r_down = Input.is_physical_key_pressed(KEY_R)
		_enter_down = Input.is_physical_key_pressed(KEY_ENTER)
		return

	var tab := Input.is_physical_key_pressed(KEY_TAB)
	if tab and not _tab_down:
		_kind_index = (_kind_index + 1) % _KINDS.size()
		_rebuild_ghost()
		mode_changed.emit(active, current_kind(), _rot)
	_tab_down = tab

	var r := Input.is_physical_key_pressed(KEY_R)
	if r and not _r_down:
		_rot = (_rot + 90) % 360
		mode_changed.emit(active, current_kind(), _rot)
	_r_down = r

	var enter := Input.is_physical_key_pressed(KEY_ENTER)
	if enter and not _enter_down:
		var snapped := _snapped_pos()
		do_place.emit(current_kind(), int(snapped.x), int(snapped.y), _rot)
		active = false
		mode_changed.emit(active, current_kind(), _rot)
	_enter_down = enter

	if Input.is_physical_key_pressed(KEY_ESCAPE):
		active = false
		mode_changed.emit(active, current_kind(), _rot)

func _rebuild_ghost() -> void:
	var kind := current_kind()
	var footprint: Vector2 = Protocol.STRUCTURE_FOOTPRINT.get(kind, Vector2(20, 20))
	var box := BoxMesh.new()
	box.size = Vector3(footprint.x, 2.0, footprint.y) * Protocol.WORLD_SCALE
	_ghost.mesh = box
	var mat := StandardMaterial3D.new()
	mat.albedo_color = _COLORS.get(kind, Color(1, 1, 1, 0.5))
	mat.transparency = BaseMaterial3D.TRANSPARENCY_ALPHA
	_ghost.material_override = mat
