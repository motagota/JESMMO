## Live Milton Road / quarry loop test (#99) against a running gateway with
## the content seeded (scripts/seed_milton_road.py + seed_poison_pen.py):
## registers a fresh account, mines one stone at the Mt Coot-tha quarry face
## (authored contract: node_quarry_rock_0 at (8210, 13900)), self-locates
## the quarry-spur road order from the civic board (path start (8485,14250)),
## contributes the stone from the face, and asserts build.progress moves.
## Deliberately does NOT complete any order — the inaugural build belongs to
## players — so the test is rerunnable (each run advances the community
## build by one stone).
## Run: Godot --headless --path client_godot -s res://tests/smoke_milton_road.gd
extends SceneTree

const FACE := Vector2i(8210, 13900) # node_quarry_rock_0's authored spot
const SPUR_START := Vector2i(8485, 14250) # the quarry spur plan's first point

var _net
var _pid := ""
var _t := 0.0
var _phase := "auth"
var _pos := Vector2.ZERO
var _stone := 0
var _spur_id := ""
var _board_poll := 0.0

func _fail(message: String) -> void:
	print("SMOKE_FAIL: %s" % message)
	quit(1)

func _initialize() -> void:
	_net = load("res://net/NetworkClient.gd").new()
	root.add_child(_net)
	var email := "roadsmoke_%d@t.test" % (Time.get_ticks_usec() % 1000000)
	_net.auth_required.connect(func(_v): _net.register(email, "pw12", "RoadSmoke"))
	_net.welcome.connect(func(d): _pid = String(d.get("player_id", "")))
	_net.status_update.connect(func(id, _zone, state):
		if id != _pid:
			return
		_pos = Vector2(float(state.get("x", 0)), float(state.get("y", 0)))
		if _phase == "auth":
			_phase = "travel"
			_net.send_move(FACE.x - int(_pos.x), FACE.y - int(_pos.y))
		elif _phase == "travel" and _pos.distance_to(Vector2(FACE)) < 5.0:
			# Only start mining once the position has settled at the face —
			# the teleport may cross a zone split, and a gather.start sent
			# before the handoff lands in the OLD zone and is dropped.
			_phase = "mining"
			_net.send_gather_start("node_quarry_rock_0")
			print("SMOKE: at the Mt Coot-tha face — mining"))
	_net.gather_result.connect(func(item_id, qty):
		if item_id == "stone":
			_stone += qty
			if _phase == "mining" and _stone >= 1:
				_net.send_gather_stop()
				_phase = "find_spur"
				_board_poll = 0.0
				print("SMOKE: mined %d stone" % _stone))
	_net.build_list.connect(func(orders):
		if _phase != "find_spur":
			return
		for o_v in orders:
			var o: Dictionary = o_v
			var path: Array = o.get("path", [])
			if path.size() >= 2 and int(path[0][0]) == SPUR_START.x and int(path[0][1]) == SPUR_START.y:
				_spur_id = String(o.get("order_id", ""))
				if String(o.get("state", "")) != "open":
					print("SMOKE_OK: the quarry spur is already BUILT — the community finished it; nothing to contribute")
					quit(0)
					return
				_phase = "contributing"
				_net.send_build_contribute(_spur_id, "stone", 1)
				print("SMOKE: contributing 1 stone to the spur (%s) from the face" % _spur_id)
				return
		_fail("no quarry-spur road order on the civic board — run scripts/seed_milton_road.py"))
	_net.build_progress.connect(func(order_id, _required, progress):
		if _phase == "contributing" and order_id == _spur_id:
			print("SMOKE_OK: Milton Road loop live — mined at the Mt Coot-tha quarry and build.progress moved (spur stone now %s)" % str(progress.get("stone")))
			quit(0))
	_net.connect_to("ws://127.0.0.1:8766")

func _process(delta: float) -> bool:
	_t += delta
	if _phase == "find_spur":
		_board_poll -= delta
		if _board_poll <= 0.0:
			_board_poll = 1.0
			_net.send_build_list()
	if _t > 40.0:
		_fail("SMOKE_TIMEOUT phase=%s stone=%d spur=%s" % [_phase, _stone, _spur_id])
		return true
	return false
