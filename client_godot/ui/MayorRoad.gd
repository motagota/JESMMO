## Mayor-only tool for commissioning a dirt path (#55): press M to toggle (a
## no-op unless `is_mayor` is true), click a start point, click again for the
## end point to commission it — any player can then fill the resulting build
## order, same as any other. A thin ghost line previews the path; Esc cancels.
## Mirrors `BuildPlace`'s raycast-onto-ground + key-toggle pattern.
class_name MayorRoad
extends Node3D

signal do_create(x0: int, y0: int, x1: int, y1: int)
## Fired whenever the mode/stage changes, so the HUD can show a hint.
signal mode_changed(active: bool, has_start: bool)

## Set by `Main` from the `welcome` payload's `role` field — everyone else's
## key press is silently ignored (the server would reject the message anyway;
## this just avoids offering a control that can't do anything).
var is_mayor := false
## Fed every frame by `Main`, same as `BuildPlace.camera`.
var camera: Camera3D

var active := false
var _has_start := false
var _start := Vector2.ZERO
var _last_ground := Vector2(3200, 3200)

var _ghost: MeshInstance3D
var _ghost_mat: StandardMaterial3D

var _m_down := false
var _click_down := false

func _ready() -> void:
	_ghost = MeshInstance3D.new()
	_ghost.visible = false
	_ghost_mat = StandardMaterial3D.new()
	_ghost_mat.albedo_color = Color(0.85, 0.6, 0.2, 0.85)
	_ghost_mat.transparency = BaseMaterial3D.TRANSPARENCY_ALPHA
	_ghost.material_override = _ghost_mat
	add_child(_ghost)

func _process(_delta: float) -> void:
	var m := Input.is_physical_key_pressed(KEY_M)
	if m and not _m_down and is_mayor:
		active = not active
		_has_start = false
		_ghost.visible = false
		mode_changed.emit(active, _has_start)
	_m_down = m

	if not active:
		_click_down = Input.is_mouse_button_pressed(MOUSE_BUTTON_LEFT)
		return

	var cur := _raycast_ground()
	if _has_start:
		_ghost.visible = true
		_update_ghost(_start, cur)

	var click := Input.is_mouse_button_pressed(MOUSE_BUTTON_LEFT)
	if click and not _click_down:
		if not _has_start:
			_start = cur
			_has_start = true
			mode_changed.emit(active, _has_start)
		else:
			do_create.emit(int(_start.x), int(_start.y), int(cur.x), int(cur.y))
			active = false
			_has_start = false
			_ghost.visible = false
			mode_changed.emit(active, _has_start)
	_click_down = click

	if Input.is_physical_key_pressed(KEY_ESCAPE):
		active = false
		_has_start = false
		_ghost.visible = false
		mode_changed.emit(active, _has_start)

## Raycast from the camera through the mouse position onto the ground, refined
## against the actual terrain height — same two-pass approach as
## `BuildPlace._raycast_ground` (see its comment for why one plane pass alone
## isn't accurate enough on sloped ground).
func _raycast_ground() -> Vector2:
	if camera == null:
		return _last_ground
	var mouse := get_viewport().get_mouse_position()
	var origin := camera.project_ray_origin(mouse)
	var dir := camera.project_ray_normal(mouse)
	if absf(dir.y) < 0.0001:
		return _last_ground
	var t := -origin.y / dir.y
	if t <= 0.0:
		return _last_ground
	var hit := origin + dir * t
	var approx := Vector2(hit.x / Protocol.WORLD_SCALE, hit.z / Protocol.WORLD_SCALE)

	var ground_y := Protocol.terrain_height(approx.x, approx.y)
	var t2 := (ground_y - origin.y) / dir.y
	if t2 <= 0.0:
		return _last_ground
	var hit2 := origin + dir * t2
	_last_ground = Vector2(hit2.x / Protocol.WORLD_SCALE, hit2.z / Protocol.WORLD_SCALE)
	return _last_ground

func _update_ghost(a: Vector2, b: Vector2) -> void:
	var length := maxf(a.distance_to(b), 0.1)
	var box := BoxMesh.new()
	box.size = Vector3(length, 0.3, 3.0) * Protocol.WORLD_SCALE
	_ghost.mesh = box
	var mid := (a + b) * 0.5
	_ghost.global_position = Protocol.w2v(mid.x, mid.y, 1.0)
	_ghost.rotation.y = -atan2(b.y - a.y, b.x - a.x)
