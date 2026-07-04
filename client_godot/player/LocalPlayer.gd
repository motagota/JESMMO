## The local avatar: input -> movement, client-side prediction + reconciliation,
## and an orbiting third-person camera.
##
## The server is authoritative: every tick we send a `move {dx,dy}` delta AND
## apply it locally (prediction) so input feels instant. When an authoritative
## `status_update` for us arrives we only snap to it if it has drifted past
## `RECONCILE_DRIFT` (e.g. after a migration, respawn, or world-edge clamp) —
## otherwise prediction stays smooth. This mirrors the 2D client; true input-replay
## reconciliation needs sequence numbers the protocol doesn't carry yet.
##
## Camera: hold the right mouse button to orbit (mouse stays free otherwise, so
## clicking the UI panels still works). WASD is camera-relative — "forward"
## always means "away from the camera" — so movement direction is recomputed
## from the camera's current yaw every tick rather than fixed to world axes.
class_name LocalPlayer
extends Node3D

signal move_requested(dx: int, dy: int)
signal attack_requested(dx: int, dy: int)
signal gather_pressed
signal position_changed(wx: float, wy: float)

const _ATTACK_COOLDOWN := 0.3 # seconds; matches the server's swing cadence
const _CAM_SMOOTH := 8.0
const _CAM_DISTANCE := 10.0 # how far back the camera sits from the yaw pivot
const _CAM_HEIGHT := 1.4 # yaw pivot height (roughly chest/eye level)
const _MOUSE_SENSITIVITY := 0.006 # radians of orbit per pixel of mouse motion
const _PITCH_MIN := -1.31 # ~-75 deg; steepest look-down before it'd flip
const _PITCH_MAX := 0.35 # ~20 deg; a little above level, without going overhead

# Predicted authoritative position in *world* units (the source of truth we render).
var _pos := Vector2(3200, 3200) # default at the town centre until the first snapshot
var _facing := Vector2(1, 0)
var _move_accum := 0.0
var _attack_accum := 0.0
var _gather_down := false
var _active := false
# World edge for clamping predicted movement (#17). Updated from `partition` via
# `set_world_size` — the pre-partition default just needs to not clamp too early.
var _world_size := 6400.0

# Camera orbit state: yaw/pitch of the rig, and whether RMB is currently held
# (mouse is captured while looking, freed otherwise so UI panels stay clickable).
var _cam_yaw := 0.0
var _cam_pitch := -0.44 # ~-25 deg default, looking down at the player from behind
var _looking := false

var _mesh: MeshInstance3D
var _camera: Camera3D
var _cam_yaw_node: Node3D
var _cam_pitch_node: Node3D

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

	# Camera rig: a yaw pivot (at head height) holding a pitch pivot, holding
	# the camera itself at a fixed offset straight back along local Z. Because
	# the camera has no rotation of its own, it always faces back toward the
	# pivot's origin no matter how yaw/pitch are set — turning the rig orbits
	# the camera around the player rather than requiring hand-derived trig.
	_cam_yaw_node = Node3D.new()
	_cam_yaw_node.position = Vector3(0, _CAM_HEIGHT, 0)
	add_child(_cam_yaw_node)

	_cam_pitch_node = Node3D.new()
	_cam_yaw_node.add_child(_cam_pitch_node)

	_camera = Camera3D.new()
	_camera.position = Vector3(0, 0, _CAM_DISTANCE)
	_camera.current = true
	_cam_pitch_node.add_child(_camera)

	_apply_camera_rotation()

	global_position = Protocol.w2v(_pos.x, _pos.y)
	set_process(false) # inert until the session is live

func _apply_camera_rotation() -> void:
	_cam_yaw_node.rotation.y = _cam_yaw
	_cam_pitch_node.rotation.x = _cam_pitch

## Hold the right mouse button to orbit the camera; release to get the cursor
## back for the UI panels. Only active once the session is live.
func _unhandled_input(event: InputEvent) -> void:
	if not _active:
		return
	if event is InputEventMouseButton and event.button_index == MOUSE_BUTTON_RIGHT:
		_looking = event.pressed
		Input.mouse_mode = Input.MOUSE_MODE_CAPTURED if _looking else Input.MOUSE_MODE_VISIBLE
	elif event is InputEventMouseMotion and _looking:
		_cam_yaw -= event.relative.x * _MOUSE_SENSITIVITY
		_cam_pitch = clampf(_cam_pitch - event.relative.y * _MOUSE_SENSITIVITY, _PITCH_MIN, _PITCH_MAX)
		_apply_camera_rotation()

## The server's current world edge (from `partition`), so local prediction
## clamps to the same bound the server enforces instead of a stale default.
func set_world_size(size: float) -> void:
	_world_size = size

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
			_pos.x = clampf(_pos.x + dx, 0.0, _world_size)
			_pos.y = clampf(_pos.y + dy, 0.0, _world_size)
			position_changed.emit(_pos.x, _pos.y)

	if Input.is_physical_key_pressed(KEY_SPACE):
		_try_attack()

	# Gather: edge-detect the key so one press starts one gather request.
	var g := Input.is_physical_key_pressed(KEY_E)
	if g and not _gather_down:
		gather_pressed.emit()
	_gather_down = g

	# Render: ease toward the predicted/authoritative world position.
	var target := Protocol.w2v(_pos.x, _pos.y)
	global_position = global_position.lerp(target, clampf(_CAM_SMOOTH * delta, 0.0, 1.0))

func _try_attack() -> void:
	if _attack_accum < _ATTACK_COOLDOWN:
		return
	_attack_accum = 0.0
	attack_requested.emit(int(_facing.x), int(_facing.y))

## WASD / arrow keys -> a unit-ish world direction, camera-relative: "forward"
## (W) always means "away from the camera," whatever the current orbit yaw is.
## Rotates the raw (forward, strafe) input by `_cam_yaw` using the same
## `Vector3.rotated(UP, ...)` the camera rig's yaw uses, so the two stay in
## lockstep by construction rather than by hand-matched trig signs. World y
## maps to the 3D Z axis (matching `Protocol.w2v`), so the result reads off
## `.x`/`.z`.
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
	if v == Vector2.ZERO:
		return v
	var forward3 := Vector3.FORWARD.rotated(Vector3.UP, _cam_yaw)
	var right3 := Vector3.RIGHT.rotated(Vector3.UP, _cam_yaw)
	var world := right3 * v.x + forward3 * (-v.y)
	return Vector2(signf(world.x), signf(world.z))

func world_pos() -> Vector2:
	return _pos

func camera() -> Camera3D:
	return _camera

func facing() -> Vector2:
	return _facing
