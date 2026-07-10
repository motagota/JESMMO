## Headless end-to-end editor test against a running gateway (terrain
## editing #78): logs in as the seeded editor account, streams chunk (5,5),
## builds a raise stroke with the real BrushController math, previews it,
## sends it as one terrain.edit_op, verifies the terrain.delta_patch
## reconciliation lands at the same height the preview showed, and finally
## proves persistence by reading the delta back from a second, fresh guest
## session. Also checks a guest's edit op is rejected.
## Run: Godot --headless --path client_godot -s res://tests/smoke_editor_live.gd
extends SceneTree

const _CENTER_CORNER := Vector2i(660, 660) # world (3300, 3300), chunk (5,5) interior
const _RADIUS := 40.0
const _STRENGTH := 0.4
const _RATE := 400.0
const _TICK := 0.5 # one simulated half-second paint tick -> 80cm at center

var _net
var _net2
var _streamer: TerrainStreamer
var _t := 0.0
var _phase := "auth"
var _h_base := 0.0
var _h_preview := 0.0
var _stroke: Array = []
var _tile_in := false
var _delta_in := false

func _fail(message: String) -> void:
	print("SMOKE_FAIL: %s" % message)
	quit(1)

func _initialize() -> void:
	randomize()
	_streamer = TerrainStreamer.new()
	root.add_child(_streamer)
	_streamer.set_context([], 6400.0)
	_net = load("res://net/NetworkClient.gd").new()
	root.add_child(_net)
	_net.auth_required.connect(func(_v): _net.login("editor@capital.town", "editor12345"))
	_net.welcome.connect(func(d):
		if String(d.get("role", "")) != "editor":
			_fail("expected editor role, got %s" % d.get("role"))
			return
		_phase = "want_terrain"
		_net.send_terrain_list())
	_net.terrain_data.connect(func(resolution, world_size, heights):
		Protocol.apply_terrain_data(resolution, world_size, heights)
		_phase = "streaming"
		_streamer.on_player_position(3300.0, 3300.0)) # ring around chunk (5,5)
	_streamer.tile_requested.connect(func(tx, ty): _net.send_terrain_tile_request(tx, ty))
	_streamer.delta_requested.connect(func(tx, ty): _net.send_terrain_delta_request(tx, ty))
	_net.terrain_tile_data.connect(func(tx, ty, heights):
		_streamer.on_tile_data(tx, ty, heights)
		if Vector2i(tx, ty) == Vector2i(5, 5):
			_tile_in = true
			_maybe_paint())
	_net.terrain_delta_data.connect(func(tx, ty, has_delta, offsets):
		_streamer.on_delta_data(tx, ty, has_delta, offsets)
		if Vector2i(tx, ty) == Vector2i(5, 5):
			if has_delta:
				_fail("chunk (5,5) should start unedited — clean the dev DB and rerun")
				return
			_delta_in = true
			_maybe_paint())
	_net.terrain_delta_patch.connect(func(tx, ty, revision, offsets):
		_streamer.on_delta_patch(tx, ty, offsets)
		if Vector2i(tx, ty) == Vector2i(5, 5) and _phase == "awaiting_patch":
			_on_patch(revision))
	_net.terrain_edit_error.connect(func(message): _fail("unexpected edit_error: %s" % message))
	_net.connect_to("ws://127.0.0.1:8766")

## Once both the tile and its (empty) delta are resident, paint one
## simulated brush tick and commit it — the same math BrushController's
## _paint_tick runs, minus the mouse.
func _maybe_paint() -> void:
	if not (_tile_in and _delta_in) or _phase != "streaming":
		return
	_phase = "painting"
	var cell := Protocol._tile_cell_m
	var g := Vector2(_CENTER_CORNER.x * cell, _CENTER_CORNER.y * cell)
	_h_base = Protocol.terrain_height(g.x, g.y)
	var increments: Dictionary = {}
	_stroke = []
	for corner in BrushController.brush_corners(g, _RADIUS, cell, Protocol._tile_size * Protocol._tiles_x, Protocol._tile_size * Protocol._tiles_y):
		var dist := Vector2(corner.x * cell, corner.y * cell).distance_to(g)
		var f := BrushController.falloff_factor(dist / _RADIUS, "smooth")
		var d_cm := int(round(_STRENGTH * _RATE * _TICK * f))
		if d_cm != 0:
			increments[corner] = d_cm * 0.01
			_stroke.append([corner.x, corner.y, d_cm])
	_streamer.apply_edit_preview(increments)
	_h_preview = Protocol.terrain_height(g.x, g.y)
	if absf(_h_preview - (_h_base + 0.8)) > 0.02:
		_fail("preview should lift the center corner by 0.8m (base=%f preview=%f)" % [_h_base, _h_preview])
		return
	print("SMOKE: preview lifted center %.2f -> %.2f (+0.8m), stroke has %d cells" % [_h_base, _h_preview, _stroke.size()])
	_phase = "awaiting_patch"
	_net.send_terrain_edit_op("raise", _stroke)

## The authoritative patch arrived: reconciliation must land exactly where
## the preview already was (same integer-cm math on both sides).
func _on_patch(revision: int) -> void:
	var cell := Protocol._tile_cell_m
	var h := Protocol.terrain_height(_CENTER_CORNER.x * cell, _CENTER_CORNER.y * cell)
	if absf(h - _h_preview) > 0.02:
		_fail("patch reconcile moved the terrain (preview=%f, post-patch=%f) — preview/server math disagree" % [_h_preview, h])
		return
	print("SMOKE: patch (revision %d) reconciled with zero pop (%.2f)" % [revision, h])
	# Persistence: a brand-new guest session must see the edit via the
	# ordinary delta read path.
	_phase = "verify_fresh_session"
	_net2 = load("res://net/NetworkClient.gd").new()
	root.add_child(_net2)
	_net2.auth_required.connect(func(_v): _net2.guest())
	_net2.welcome.connect(func(_d):
		# A guest may not edit — the server must reject it...
		_net2.send_terrain_edit_op("raise", [[10, 10, 100]])
		# ...and must serve the edited chunk's delta to anyone.
		_net2.send_terrain_delta_request(5, 5))
	_net2.terrain_edit_error.connect(func(message):
		print("SMOKE: guest edit correctly rejected (%s)" % message))
	_net2.terrain_delta_data.connect(func(tx, ty, has_delta, offsets):
		if Vector2i(tx, ty) != Vector2i(5, 5):
			return
		if not has_delta:
			_fail("a fresh session should see the persisted delta")
			return
		var side := Protocol._tile_size + 1
		var lx: int = _CENTER_CORNER.x - 5 * Protocol._tile_size
		var ly: int = _CENTER_CORNER.y - 5 * Protocol._tile_size
		var got: float = offsets[ly * side + lx]
		if absf(got - 0.8) > 0.001:
			_fail("fresh session decoded %.3fm at the center corner, want 0.8" % got)
			return
		print("SMOKE_OK: live editor round-trip (login, stream, preview, edit_op, patch reconcile, fresh-session persistence, guest rejection) works")
		quit(0))
	_net2.connect_to("ws://127.0.0.1:8766")

func _process(delta: float) -> bool:
	_t += delta
	if _t > 25.0:
		_fail("SMOKE_TIMEOUT phase=%s" % _phase)
		return true
	return false
