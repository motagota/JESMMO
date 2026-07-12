## Headless end-to-end undo test against a running gateway (terrain editing
## #79): paints a stroke as the editor, records the edit_ack in a real
## HistoryPanel, undoes it via undo_last_target -> terrain.revert_op, and
## verifies the patch restores the streamed terrain to its pre-stroke height
## and the panel state flips on revert_ack.
## Run: Godot --headless --path client_godot -s res://tests/smoke_editor_undo_live.gd
extends SceneTree

const _CENTER_CORNER := Vector2i(660, 660) # world (3300, 3300), chunk (5,5)

var _net
var _streamer: TerrainStreamer
var _history: HistoryPanel
var _t := 0.0
var _phase := "auth"
var _h_base := 0.0
var _tile_in := false
var _delta_in := false
var _patches_seen := 0

func _fail(message: String) -> void:
	print("SMOKE_FAIL: %s" % message)
	quit(1)

func _initialize() -> void:
	randomize()
	_streamer = TerrainStreamer.new()
	root.add_child(_streamer)
	_streamer.set_context([], 6400.0)
	_history = HistoryPanel.new()
	root.add_child(_history)
	_net = load("res://net/NetworkClient.gd").new()
	root.add_child(_net)
	_history.do_revert.connect(func(op_id): _net.send_terrain_revert_op(op_id))
	_net.terrain_edit_ack.connect(func(op_id, brush): _history.record_op(op_id, brush))
	_net.terrain_revert_ack.connect(func(op_id):
		_history.mark_reverted(op_id)
		_check_reverted())
	_net.auth_required.connect(func(_v): _net.login("editor@capital.town", "editor12345"))
	_net.welcome.connect(func(d):
		if String(d.get("role", "")) != "editor":
			_fail("expected editor role")
			return
		_net.send_terrain_list())
	_net.terrain_data.connect(func(resolution, world_size, heights):
		Protocol.apply_terrain_data(resolution, world_size, heights)
		_phase = "streaming"
		_streamer.on_player_position(3300.0, 3300.0))
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
	_net.terrain_delta_patch.connect(func(tx, ty, _rev, offsets):
		_streamer.on_delta_patch(tx, ty, offsets)
		_patches_seen += 1
		if _phase == "awaiting_edit_patch" and Vector2i(tx, ty) == Vector2i(5, 5):
			_phase = "undoing"
			var target := _history.undo_last_target()
			if target == "":
				_fail("edit_ack should have armed the history before the patch")
				return
			print("SMOKE: stroke applied (%.2fm), undoing op %s" % [Protocol.terrain_height(3300.0, 3300.0), target.substr(0, 8)])
			_history.do_revert.emit(target))
	_net.terrain_edit_error.connect(func(message): _fail("unexpected edit_error: %s" % message))
	_net.connect_to("ws://127.0.0.1:8766")

func _maybe_paint() -> void:
	if not (_tile_in and _delta_in) or _phase != "streaming":
		return
	_phase = "awaiting_edit_patch"
	_h_base = Protocol.terrain_height(3300.0, 3300.0)
	_net.send_terrain_edit_op("raise", [[_CENTER_CORNER.x, _CENTER_CORNER.y, 500]])

## revert_ack arrived: the patch (which precedes it) must have restored the
## exact pre-stroke height, and the history must have no undo target left.
func _check_reverted() -> void:
	var h := Protocol.terrain_height(3300.0, 3300.0)
	if absf(h - _h_base) > 0.001:
		_fail("undo must restore the exact pre-stroke height (base=%f, got=%f)" % [_h_base, h])
		return
	if _history.undo_last_target() != "":
		_fail("history should be fully reverted")
		return
	print("SMOKE_OK: live undo round-trip (edit_ack -> history -> revert_op -> patch restores %.2fm -> revert_ack) works" % h)
	quit(0)

func _process(delta: float) -> bool:
	_t += delta
	if _t > 25.0:
		_fail("SMOKE_TIMEOUT phase=%s" % _phase)
		return true
	return false
