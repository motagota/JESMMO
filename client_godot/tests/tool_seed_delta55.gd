## Dev-DB seeding tool, not a test: recreates the #76 verification state that
## `smoke_terrain_delta_live.gd` expects — a +8m plateau on chunk (5,5)'s
## block 0 first 4x4 corners — by logging in as the seeded editor account and
## sending one `terrain.edit_op`. Run it once against a fresh dev DB (after
## `smoke_editor_undo_live`, which needs the chunk to start clean):
##   Godot --headless --path client_godot -s res://tests/tool_seed_delta55.gd
extends SceneTree

var _net
var _t := 0.0

func _initialize() -> void:
	_net = load("res://net/NetworkClient.gd").new()
	root.add_child(_net)
	_net.auth_required.connect(func(_v): _net.login("editor@capital.town", "editor12345"))
	_net.welcome.connect(func(_d):
		var cells: Array = []
		var ts := 128  # chunk (5,5)'s corner origin in world corner coords
		for gy in range(4):
			for gx in range(4):
				cells.append([5 * ts + gx, 5 * ts + gy, 800])  # +8m in cm
		_net.send_terrain_edit_op("seed-plateau", cells))
	_net.terrain_edit_ack.connect(func(op_id, _brush):
		print("SEED_OK: chunk (5,5) +8m plateau accepted (op %s)" % op_id)
		quit(0))
	_net.terrain_edit_error.connect(func(message):
		push_error("SEED_FAIL: %s" % message)
		quit(1))
	_net.connect_to("ws://127.0.0.1:8766")

func _process(delta: float) -> bool:
	_t += delta
	if _t > 15.0:
		push_error("SEED_TIMEOUT")
		quit(1)
		return true
	return false
