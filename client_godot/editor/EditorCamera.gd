## Free-fly camera for `--editor-mode` (terrain editing #78): hold RMB to
## mouse-look (cursor captured, same convention as the player's orbit rig),
## WASD to fly, Q/E down/up, mouse wheel to scale speed. The normal player
## rig stays inactive in editor mode, so there's no input contention.
class_name EditorCamera
extends Camera3D

const _MOUSE_SENSITIVITY := 0.005
const _SPEED_DEFAULT := 30.0     # scene units/s (world is 640m across at 0.1 scale)
const _SPEED_MIN := 2.0
const _SPEED_MAX := 300.0
const _SPEED_WHEEL_FACTOR := 1.25

var _speed := _SPEED_DEFAULT
var _yaw := 0.0
var _pitch := -0.9   # start looking down at the terrain
var _looking := false

func _ready() -> void:
	rotation = Vector3(_pitch, _yaw, 0.0)

## Place the camera above a world (server-unit) position, looking down.
func place_over(wx: float, wy: float, height_m: float = 60.0) -> void:
	var ground := Protocol.terrain_height(wx, wy)
	global_position = Vector3(wx * Protocol.WORLD_SCALE, ground + height_m, wy * Protocol.WORLD_SCALE)
	rotation = Vector3(_pitch, _yaw, 0.0)

## The camera's position in world (server) units — feeds `TerrainStreamer`
## the same way the player's position does in normal play.
func world_pos() -> Vector2:
	return Vector2(global_position.x / Protocol.WORLD_SCALE, global_position.z / Protocol.WORLD_SCALE)

func _unhandled_input(event: InputEvent) -> void:
	if event is InputEventMouseButton:
		if event.button_index == MOUSE_BUTTON_RIGHT:
			_looking = event.pressed
			Input.mouse_mode = Input.MOUSE_MODE_CAPTURED if _looking else Input.MOUSE_MODE_VISIBLE
		elif event.pressed and event.button_index == MOUSE_BUTTON_WHEEL_UP:
			_speed = minf(_speed * _SPEED_WHEEL_FACTOR, _SPEED_MAX)
		elif event.pressed and event.button_index == MOUSE_BUTTON_WHEEL_DOWN:
			_speed = maxf(_speed / _SPEED_WHEEL_FACTOR, _SPEED_MIN)
	elif event is InputEventMouseMotion and _looking:
		_yaw -= event.relative.x * _MOUSE_SENSITIVITY
		_pitch = clampf(_pitch - event.relative.y * _MOUSE_SENSITIVITY, -1.5, 1.5)
		rotation = Vector3(_pitch, _yaw, 0.0)

func _process(delta: float) -> void:
	var move := Vector3.ZERO
	if Input.is_physical_key_pressed(KEY_W):
		move -= basis.z
	if Input.is_physical_key_pressed(KEY_S):
		move += basis.z
	if Input.is_physical_key_pressed(KEY_A):
		move -= basis.x
	if Input.is_physical_key_pressed(KEY_D):
		move += basis.x
	if Input.is_physical_key_pressed(KEY_E):
		move += Vector3.UP
	if Input.is_physical_key_pressed(KEY_Q):
		move -= Vector3.UP
	if move != Vector3.ZERO:
		global_position += move.normalized() * _speed * delta
