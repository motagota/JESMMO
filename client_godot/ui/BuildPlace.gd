## Build/place mode for home structures (#12): press B to toggle, Tab to cycle
## the structure kind, R to rotate 90°, click (or Enter) to confirm, Esc to
## cancel. The ghost follows the mouse (raycast onto the ground plane) rather
## than the player, snapped to a grid, and previews validity live: green where
## the placement would be accepted, red where it wouldn't (off your plot, or
## overlapping something already there) — mirrors the server's own bounds/
## overlap check in `apply_build_place` so the preview needs no round-trip.
## Confirming while red is a silent no-op, matching the rest of the protocol's
## invalid-action convention.
class_name BuildPlace
extends Node3D

signal do_place(kind: String, x: int, y: int, rot: int)
## Fired whenever the mode/kind/rotation changes, so the HUD can show a hint.
signal mode_changed(active: bool, kind: String, rot: int)

const _KINDS := ["bed", "storage", "crafting"]
const _COLOR_VALID := Color(0.30, 0.90, 0.35, 0.55)
const _COLOR_INVALID := Color(0.90, 0.25, 0.20, 0.55)

var active := false
## Fed every frame by `Main`: the camera to raycast from, the player's own
## plot bounds (`{x,y,w,h}`, empty if not yet known), and the live entity
## roster (to check overlap against already-placed structures).
var camera: Camera3D
var plot_bounds: Dictionary = {}
var entities: EntityManager

var _kind_index := 0
var _rot := 0
var _ghost: MeshInstance3D
var _ghost_mat: StandardMaterial3D
## The ghost's current placement position (world units, top-left corner of
## the footprint — matching the server's placement semantics) and whether
## it's currently valid there.
var _ghost_pos := Vector2(3200, 3200)
var _valid := false
## Last successful ground-raycast hit, kept as a fallback for the rare frame
## where the camera looks dead level or past the horizon (no sane ground hit).
var _last_ground := Vector2(3200, 3200)

var _b_down := false
var _tab_down := false
var _r_down := false
var _enter_down := false
var _click_down := false

func _ready() -> void:
	_ghost = MeshInstance3D.new()
	_ghost.visible = false
	_ghost_mat = StandardMaterial3D.new()
	_ghost_mat.transparency = BaseMaterial3D.TRANSPARENCY_ALPHA
	_ghost.material_override = _ghost_mat
	add_child(_ghost)
	_rebuild_ghost_mesh()

func current_kind() -> String:
	return _KINDS[_kind_index]

func _process(_delta: float) -> void:
	var b := Input.is_physical_key_pressed(KEY_B)
	if b and not _b_down:
		active = not active
		mode_changed.emit(active, current_kind(), _rot)
	_b_down = b

	if not active:
		_ghost.visible = false
		# Still track key-down state while inactive so re-entering active mode
		# doesn't misread a held-over key press as a fresh edge.
		_tab_down = Input.is_physical_key_pressed(KEY_TAB)
		_r_down = Input.is_physical_key_pressed(KEY_R)
		_enter_down = Input.is_physical_key_pressed(KEY_ENTER)
		_click_down = Input.is_mouse_button_pressed(MOUSE_BUTTON_LEFT)
		return

	_ghost.visible = true
	var footprint: Vector2 = Protocol.STRUCTURE_FOOTPRINT.get(current_kind(), Vector2(20, 20))
	_ghost_pos = _snapped_pos()
	_valid = _is_valid_placement(_ghost_pos, footprint)
	# The ghost mesh is centred on its own node origin, but placement is
	# top-left-corner (matching the server) — offset the visual by half the
	# footprint so it actually sits over the exact rect being validated/sent.
	global_position = Protocol.w2v(_ghost_pos.x + footprint.x * 0.5, _ghost_pos.y + footprint.y * 0.5, 0.05)
	_ghost.rotation_degrees = Vector3(0, _rot, 0)
	_ghost_mat.albedo_color = _COLOR_VALID if _valid else _COLOR_INVALID

	var tab := Input.is_physical_key_pressed(KEY_TAB)
	if tab and not _tab_down:
		_kind_index = (_kind_index + 1) % _KINDS.size()
		_rebuild_ghost_mesh()
		mode_changed.emit(active, current_kind(), _rot)
	_tab_down = tab

	var r := Input.is_physical_key_pressed(KEY_R)
	if r and not _r_down:
		_rot = (_rot + 90) % 360
		mode_changed.emit(active, current_kind(), _rot)
	_r_down = r

	var confirm := Input.is_physical_key_pressed(KEY_ENTER)
	var click := Input.is_mouse_button_pressed(MOUSE_BUTTON_LEFT)
	if ((confirm and not _enter_down) or (click and not _click_down)) and _valid:
		do_place.emit(current_kind(), int(_ghost_pos.x), int(_ghost_pos.y), _rot)
		active = false
		mode_changed.emit(active, current_kind(), _rot)
	_enter_down = confirm
	_click_down = click

	if Input.is_physical_key_pressed(KEY_ESCAPE):
		active = false
		mode_changed.emit(active, current_kind(), _rot)

## Raycast from the camera through `screen_pos` (the current mouse position by
## default — overridable for tests) onto the ground, returning the hit in
## world units. Falls back to the last good hit if the camera looks dead
## level or the ground is behind it (no sane intersection that frame).
##
## The ground isn't flat (`Protocol.terrain_height`), so a single plane
## intersection would land slightly off on sloped ground — refined with one
## extra pass against the actual terrain height at the first-pass estimate,
## which is accurate enough for gentle, smoothly-varying hills without
## iteratively ray-marching the surface.
func _raycast_ground(screen_pos: Variant = null) -> Vector2:
	if camera == null:
		return _last_ground
	var mouse: Vector2 = screen_pos if screen_pos != null else get_viewport().get_mouse_position()
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

func _snapped_pos() -> Vector2:
	var raw := _raycast_ground()
	var step := float(Protocol.PLACE_GRID)
	return Vector2(round(raw.x / step) * step, round(raw.y / step) * step)

## Mirrors the server's own placement check (`apply_build_place`): the
## footprint (top-left `corner`, world-unit `footprint` size) must fit inside
## the player's own plot bounds and not overlap an already-placed structure.
func _is_valid_placement(corner: Vector2, footprint: Vector2) -> bool:
	if plot_bounds.is_empty() or entities == null:
		return false
	var bx0 := float(plot_bounds.get("x", 0))
	var by0 := float(plot_bounds.get("y", 0))
	var bx1 := bx0 + float(plot_bounds.get("w", 0))
	var by1 := by0 + float(plot_bounds.get("h", 0))
	if corner.x < bx0 or corner.y < by0 or corner.x + footprint.x > bx1 or corner.y + footprint.y > by1:
		return false
	return not entities.overlaps_home_structure(corner, footprint)

func _rebuild_ghost_mesh() -> void:
	var footprint: Vector2 = Protocol.STRUCTURE_FOOTPRINT.get(current_kind(), Vector2(20, 20))
	var box := BoxMesh.new()
	box.size = Vector3(footprint.x, 2.0, footprint.y) * Protocol.WORLD_SCALE
	_ghost.mesh = box
