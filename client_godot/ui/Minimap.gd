## A local-area 2D minimap (#18): a fixed world-radius window centred on the
## player, panning as they move. Shows every plot in the current district
## (gold = yours, red = someone else's, green = free), the district's
## boundary, a player marker, and — if your own plot falls outside the
## visible radius — a clamped arrow at the edge pointing toward it.
##
## `CanvasLayer` (matching every other UI element here — `Hud`, `StoragePanel`,
## etc. — all of which extend `CanvasLayer` rather than `Control`, since a
## bare `Control` added under `Main`'s 3D scene has no such layer and won't
## reliably render). The actual custom-drawn (`_draw()`) surface is the nested
## `_View` `Control` below, since `CanvasLayer` itself isn't a `CanvasItem` and
## can't draw — this is the first place in the client that needs many small,
## cheaply-updated markers projected from world coordinates we already have,
## which doesn't warrant a live top-down `SubViewport`/camera render.
class_name Minimap
extends CanvasLayer

const _SIZE := 180.0
const _MARGIN := 16.0
const _RADIUS_WORLD := 700.0 # world units visible across the minimap's radius

const _COLOR_MINE := Color(0.95, 0.85, 0.30)
const _COLOR_TAKEN := Color(0.85, 0.30, 0.25)
const _COLOR_FREE := Color(0.30, 0.85, 0.40)
const _COLOR_PLAYER := Color(0.85, 0.90, 1.0)
const _COLOR_DISTRICT_EDGE := Color(0.6, 0.6, 0.65, 0.8)

var _view: _View

func _ready() -> void:
	layer = 5 # same priority as Hud — an ambient readout, not a modal panel
	_view = _View.new()
	# Size must be set *before* the anchor preset: PRESET_MODE_KEEP_SIZE bakes
	# in whatever `size` already is when the preset is applied, and a brand
	# new Control's size is (0,0) — setting `size` afterward just grows the
	# box past the screen edge instead of moving it, which is exactly what
	# put the widget off-screen.
	_view.custom_minimum_size = Vector2(_SIZE, _SIZE)
	_view.size = Vector2(_SIZE, _SIZE)
	_view.set_anchors_and_offsets_preset(Control.PRESET_TOP_RIGHT, Control.PRESET_MODE_KEEP_SIZE, int(_MARGIN))
	_view.clip_contents = true # so anything projected outside the widget is cropped for free
	_view.mouse_filter = Control.MOUSE_FILTER_IGNORE
	add_child(_view)

func set_player(wx: float, wy: float, facing: Vector2) -> void:
	_view.player_pos = Vector2(wx, wy)
	if facing != Vector2.ZERO:
		_view.player_facing = facing
	_view.queue_redraw()

func set_home(wx: float, wy: float) -> void:
	_view.has_home = true
	_view.home_pos = Vector2(wx, wy)
	_view.queue_redraw()

func set_plots(plots: Array, my_plot_id: String) -> void:
	_view.plots = plots
	_view.my_plot_id = my_plot_id
	_view.queue_redraw()

func set_district_bounds(rect: Dictionary) -> void:
	_view.district_bounds = rect
	_view.queue_redraw()

## The actual drawing surface — a nested class so `Minimap` itself can stay a
## plain state-holding `CanvasLayer` (matching this codebase's UI pattern)
## while this piece does the `CanvasItem`-only `_draw()` work.
class _View extends Control:
	var player_pos := Vector2(3200, 3200)
	var player_facing := Vector2(0, -1)
	var plots: Array = []
	var my_plot_id := ""
	var has_home := false
	var home_pos := Vector2.ZERO
	var district_bounds: Dictionary = {}

	## World point -> local widget point: centred on the player, scaled so
	## `_RADIUS_WORLD` world units span the widget's half-size.
	func _project(wp: Vector2) -> Vector2:
		var scale := (_SIZE * 0.5) / _RADIUS_WORLD
		return size * 0.5 + (wp - player_pos) * scale

	func _draw() -> void:
		draw_rect(Rect2(Vector2.ZERO, size), Color(0.05, 0.05, 0.06, 0.55))

		if not district_bounds.is_empty():
			var x0 := float(district_bounds.get("x0", 0))
			var y0 := float(district_bounds.get("y0", 0))
			var x1 := float(district_bounds.get("x1", 0))
			var y1 := float(district_bounds.get("y1", 0))
			var a := _project(Vector2(x0, y0))
			var b := _project(Vector2(x1, y0))
			var c := _project(Vector2(x1, y1))
			var d := _project(Vector2(x0, y1))
			draw_polyline(PackedVector2Array([a, b, c, d, a]), _COLOR_DISTRICT_EDGE, 1.5)

		for entry_v in plots:
			var p: Dictionary = entry_v
			var bounds: Dictionary = p.get("bounds", {})
			var w := float(bounds.get("w", 0))
			var h := float(bounds.get("h", 0))
			if w <= 0.0 or h <= 0.0:
				continue
			var center_w := Vector2(float(bounds.get("x", 0)) + w * 0.5, float(bounds.get("y", 0)) + h * 0.5)
			var mine := String(p.get("plot_id", "")) == my_plot_id
			var owner_name = p.get("owner_name")
			var taken := owner_name != null and String(owner_name) != ""
			var color := _COLOR_MINE if mine else (_COLOR_TAKEN if taken else _COLOR_FREE)
			var pt := _project(center_w)
			draw_rect(Rect2(pt - Vector2(3, 3), Vector2(6, 6)), color)

		# Home arrow at the edge, only when the player's own plot has scrolled
		# out of the visible radius (otherwise it's already a gold plot square).
		if has_home:
			var delta := home_pos - player_pos
			if delta.length() > _RADIUS_WORLD:
				var dir := delta.normalized()
				var edge := size * 0.5 + dir * (_SIZE * 0.5 - 10.0)
				_draw_marker(edge, dir, _COLOR_MINE)

		_draw_marker(size * 0.5, player_facing, _COLOR_PLAYER)

	## A small triangle at `pos` pointing along `dir` (already in the same
	## world-aligned (x,y) space as everything else this widget projects, so
	## no extra rotation math is needed — just vector arithmetic).
	func _draw_marker(pos: Vector2, dir: Vector2, color: Color) -> void:
		var d := dir.normalized()
		if d == Vector2.ZERO:
			d = Vector2(0, -1)
		var perp := Vector2(-d.y, d.x)
		var tip := pos + d * 7.0
		var left := pos - d * 5.0 + perp * 4.0
		var right := pos - d * 5.0 - perp * 4.0
		draw_colored_polygon(PackedVector2Array([tip, left, right]), color)
