## Height-brush input for `--editor-mode` (terrain editing #78): hold LMB to
## paint `raise` onto the terrain under the mouse (Shift inverts to lower),
## `[`/`]` shrink/grow the radius, `-`/`=` weaken/strengthen. Brush params
## load from `res://config/editor/brushes.cfg` (data-driven per the design
## doc's brush philosophy; ConfigFile rather than TOML because Godot parses
## it natively — same shape, zero parser code).
##
## One stroke (LMB down → up) accumulates per-corner centimeter totals; the
## whole stroke is bundled into ONE `terrain.edit_op` on mouse-up (one stroke
## = one op = one future undo step). While painting, the same increments are
## applied as a local preview through `TerrainStreamer.apply_edit_preview` —
## instant feedback, later replaced in place by the server's authoritative
## `terrain.delta_patch` (matching values, so no visual pop).
class_name BrushController
extends Node3D

## The finished stroke's cells (`[[cx, cy, d_cm], ...]`, world corner
## coordinates) — `Main` wires this to `NetworkClient.send_terrain_edit_op`.
signal stroke_committed(brush: String, cells: Array)
## Brush parameter changes, for a HUD hint.
signal status_changed(text: String)

const _BRUSHES_CFG := "res://config/editor/brushes.cfg"
## Server cap mirror (proxy.rs EDIT_MAX_CELLS_PER_OP) — a stroke bigger than
## this would be rejected wholesale, so refuse to grow one past it.
const _MAX_CELLS_PER_STROKE := 16384

var camera: Camera3D
var streamer: TerrainStreamer

var _brush_name := "raise"
var _falloff := "smooth"
var _radius := 40.0        # world units (= server meters)
var _radius_min := 5.0
var _radius_max := 320.0
var _strength := 0.4
var _strength_min := 0.05
var _strength_max := 1.0
var _rate_cm_per_s := 400.0

var _painting := false
var _stroke_total_cm: Dictionary = {}    # Vector2i(corner) -> float, exact accumulation
var _stroke_applied_cm: Dictionary = {}  # Vector2i(corner) -> int, already previewed
var _last_ground := Vector2(3200, 3200)
var _key_down: Dictionary = {}

func _ready() -> void:
	_load_brush(_brush_name)

## Load one brush's params from brushes.cfg; missing file/keys keep the
## defaults above (the tool must stay usable with a broken config).
func _load_brush(name_: String) -> void:
	var cfg := ConfigFile.new()
	if cfg.load(_BRUSHES_CFG) != OK:
		push_warning("BrushController: %s missing/unreadable — using built-in defaults" % _BRUSHES_CFG)
		return
	_falloff = String(cfg.get_value(name_, "falloff", _falloff))
	_rate_cm_per_s = float(cfg.get_value(name_, "rate_cm_per_s", _rate_cm_per_s))
	var radius: Dictionary = cfg.get_value(name_, "radius", {})
	_radius = float(radius.get("default", _radius))
	_radius_min = float(radius.get("min", _radius_min))
	_radius_max = float(radius.get("max", _radius_max))
	var strength: Dictionary = cfg.get_value(name_, "strength", {})
	_strength = float(strength.get("default", _strength))
	_strength_min = float(strength.get("min", _strength_min))
	_strength_max = float(strength.get("max", _strength_max))

func _process(delta: float) -> void:
	if camera == null or streamer == null:
		return
	_handle_param_keys()
	var lmb := Input.is_mouse_button_pressed(MOUSE_BUTTON_LEFT)
	if lmb:
		var g := _raycast_ground()
		var lower := Input.is_physical_key_pressed(KEY_SHIFT)
		_paint_tick(g, delta, lower)
		_painting = true
	elif _painting:
		_painting = false
		_commit_stroke()

## One input tick of painting: brush falloff applied around `g`, exact float
## totals accumulated per corner, and the *integer-centimeter* change since
## the last tick pushed into the preview (so preview and the eventual op are
## built from identical rounding — reconciliation can't pop).
func _paint_tick(g: Vector2, delta: float, lower: bool) -> void:
	var cell := Protocol._tile_cell_m
	if cell <= 0.0:
		return
	var direction := -1.0 if lower else 1.0
	var increments: Dictionary = {}
	for corner in brush_corners(g, _radius, cell, Protocol._tile_size * Protocol._tiles_x, Protocol._tile_size * Protocol._tiles_y):
		if _stroke_total_cm.size() >= _MAX_CELLS_PER_STROKE and not _stroke_total_cm.has(corner):
			continue # refuse to grow a stroke past what the server accepts
		var dist := Vector2(corner.x * cell, corner.y * cell).distance_to(g)
		var f := falloff_factor(dist / _radius, _falloff)
		var total: float = _stroke_total_cm.get(corner, 0.0) + direction * _strength * _rate_cm_per_s * delta * f
		_stroke_total_cm[corner] = total
		var now_cm := int(round(total))
		var applied: int = _stroke_applied_cm.get(corner, 0)
		if now_cm != applied:
			increments[corner] = float(now_cm - applied) * 0.01 # cm -> m
			_stroke_applied_cm[corner] = now_cm
	if not increments.is_empty():
		streamer.apply_edit_preview(increments)

## Bundle the stroke into one edit op. Zero-net corners (raised then lowered
## back) are dropped — the server would just prune them anyway.
func _commit_stroke() -> void:
	var cells: Array = []
	for corner in _stroke_total_cm:
		var d_cm := int(round(_stroke_total_cm[corner]))
		if d_cm != 0:
			cells.append([corner.x, corner.y, d_cm])
	_stroke_total_cm.clear()
	_stroke_applied_cm.clear()
	if not cells.is_empty():
		stroke_committed.emit(_brush_name, cells)

## The world-corner indices a brush of `radius` around world point `g`
## touches, clamped to the corner grid. Static/pure so the selection math is
## headless-testable.
static func brush_corners(g: Vector2, radius: float, cell_m: float, max_cx: int, max_cy: int) -> Array:
	var out: Array = []
	var c0x := maxi(int(ceil((g.x - radius) / cell_m)), 0)
	var c1x := mini(int(floor((g.x + radius) / cell_m)), max_cx)
	var c0y := maxi(int(ceil((g.y - radius) / cell_m)), 0)
	var c1y := mini(int(floor((g.y + radius) / cell_m)), max_cy)
	for cy in range(c0y, c1y + 1):
		for cx in range(c0x, c1x + 1):
			if Vector2(cx * cell_m, cy * cell_m).distance_to(g) <= radius:
				out.append(Vector2i(cx, cy))
	return out

## Brush weight at normalized distance `t` (0 = center, 1 = rim). Matches
## the design doc's falloff names; unknown kinds read as `smooth`.
static func falloff_factor(t: float, kind: String) -> float:
	t = clampf(t, 0.0, 1.0)
	match kind:
		"linear":
			return 1.0 - t
		"sharp":
			return (1.0 - t) * (1.0 - t)
		_:
			return 1.0 - t * t * (3.0 - 2.0 * t) # inverse smoothstep

func _handle_param_keys() -> void:
	if _key_edge(KEY_BRACKETLEFT):
		_radius = maxf(_radius / 1.3, _radius_min)
		status_changed.emit("Brush radius: %.0fm" % _radius)
	if _key_edge(KEY_BRACKETRIGHT):
		_radius = minf(_radius * 1.3, _radius_max)
		status_changed.emit("Brush radius: %.0fm" % _radius)
	if _key_edge(KEY_MINUS):
		_strength = maxf(_strength - 0.1, _strength_min)
		status_changed.emit("Brush strength: %.2f" % _strength)
	if _key_edge(KEY_EQUAL):
		_strength = minf(_strength + 0.1, _strength_max)
		status_changed.emit("Brush strength: %.2f" % _strength)

func _key_edge(key: Key) -> bool:
	var down := Input.is_physical_key_pressed(key)
	var was: bool = _key_down.get(key, false)
	_key_down[key] = down
	return down and not was

## Camera-ray → ground world point, the same two-pass plane-then-terrain
## refinement `MayorRoad._raycast_ground` uses.
func _raycast_ground() -> Vector2:
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
