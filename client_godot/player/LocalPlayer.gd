## The local avatar: input -> movement, client-side prediction + reconciliation,
## and a third-person follow camera.
##
## The server is authoritative: every tick we send a `move {dx,dy}` delta AND
## apply it locally (prediction) so input feels instant. When an authoritative
## `status_update` for us arrives we only snap to it if it has drifted past
## `RECONCILE_DRIFT` (e.g. after a migration, respawn, or world-edge clamp) —
## otherwise prediction stays smooth. This mirrors the 2D client; true input-replay
## reconciliation needs sequence numbers the protocol doesn't carry yet.
class_name LocalPlayer
extends Node3D

signal move_requested(dx: int, dy: int)
signal attack_requested(dx: int, dy: int)
signal position_changed(wx: float, wy: float)

const _ATTACK_COOLDOWN := 0.3 # seconds; matches the server's swing cadence
const _CAM_SMOOTH := 8.0

# Predicted authoritative position in *world* units (the source of truth we render).
var _pos := Vector2(600, 600) # default at the town centre until the first snapshot
var _facing := Vector2(1, 0)
var _move_accum := 0.0
var _attack_accum := 0.0
var _active := false

var _mesh: MeshInstance3D
var _camera: Camera3D

func _ready() -> void:
	_mesh = MeshInstance3D.new()
	var cap := CapsuleMesh.new()
	cap.radius = 0.6
	cap.height = 2.2
	_mesh.mesh = cap
	var mat := StandardMaterial3D.new()
	mat.albedo_color = Color(0.20, 1.0, 0.55)
	mat.emission_enabled = true
	mat.emission = Color(0.10, 0.6, 0.3)
	_mesh.material_override = mat
	_mesh.position = Vector3(0, 1.2, 0)
	add_child(_mesh)

	_camera = Camera3D.new()
	_camera.position = Vector3(0, 11, 14)
	_camera.rotation_degrees = Vector3(-38, 0, 0)
	_camera.current = true
	add_child(_camera)

	global_position = Protocol.w2v(_pos.x, _pos.y)
	set_process(false) # inert until the session is live

## Begin local control (called on `welcome`). Optionally seed the start position.
func activate(start: Vector2 = _pos) -> void:
	_pos = start
	global_position = Protocol.w2v(_pos.x, _pos.y)
	_active = true
	visible = true
	set_process(true)
	position_changed.emit(_pos.x, _pos.y)

## Apply an authoritative snapshot for the local player (reconciliation).
func reconcile(state: Dictionary) -> void:
	var sx := float(state.get("x", _pos.x))
	var sy := float(state.get("y", _pos.y))
	var server := Vector2(sx, sy)
	if not _active:
		# First snapshot before activation: trust it outright.
		_pos = server
		return
	if _pos.distance_to(server) > Protocol.RECONCILE_DRIFT:
		_pos = server
		position_changed.emit(_pos.x, _pos.y)

func _process(delta: float) -> void:
	_attack_accum += delta

	var dir := _input_dir()
	if dir != Vector2.ZERO:
		_facing = dir

	# Send + predict on a steady tick rather than every frame.
	_move_accum += delta
	while _move_accum >= Protocol.MOVE_TICK:
		_move_accum -= Protocol.MOVE_TICK
		if dir != Vector2.ZERO:
			var dx := int(dir.x) * Protocol.MOVE_STEP
			var dy := int(dir.y) * Protocol.MOVE_STEP
			move_requested.emit(dx, dy)
			# Prediction: apply the same delta now, clamped to the world.
			_pos.x = clampf(_pos.x + dx, 0.0, 1200.0)
			_pos.y = clampf(_pos.y + dy, 0.0, 1200.0)
			position_changed.emit(_pos.x, _pos.y)

	if Input.is_action_just_pressed("ui_accept") or Input.is_physical_key_pressed(KEY_SPACE):
		_try_attack()

	# Render: ease toward the predicted/authoritative world position.
	var target := Protocol.w2v(_pos.x, _pos.y)
	global_position = global_position.lerp(target, clampf(_CAM_SMOOTH * delta, 0.0, 1.0))

func _try_attack() -> void:
	if _attack_accum < _ATTACK_COOLDOWN:
		return
	_attack_accum = 0.0
	attack_requested.emit(int(_facing.x), int(_facing.y))

## WASD / arrow keys -> a unit-ish world direction. Up (north) is -Y, matching the
## server's coordinate convention.
func _input_dir() -> Vector2:
	var v := Vector2.ZERO
	if Input.is_physical_key_pressed(KEY_W) or Input.is_physical_key_pressed(KEY_UP):
		v.y -= 1
	if Input.is_physical_key_pressed(KEY_S) or Input.is_physical_key_pressed(KEY_DOWN):
		v.y += 1
	if Input.is_physical_key_pressed(KEY_A) or Input.is_physical_key_pressed(KEY_LEFT):
		v.x -= 1
	if Input.is_physical_key_pressed(KEY_D) or Input.is_physical_key_pressed(KEY_RIGHT):
		v.x += 1
	return Vector2(signf(v.x), signf(v.y))

func world_pos() -> Vector2:
	return _pos
