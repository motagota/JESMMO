## Live build-wages test (#145) against a running gateway: an editor plans a
## short road at the Mt Coot-tha quarry face, a real player mines real stone
## there and contributes it, and the test asserts the city pays — `gold.update`
## arrives with a positive delta and an authoritative balance, and the HUD's
## purse readout tracks it. This is the game's first gold faucet, so the thing
## being proven is that gold can now go UP at all.
## Run: Godot --headless --path client_godot -s res://tests/smoke_wages.gd
extends SceneTree

const FACE := Vector2i(8210, 13900) # node_quarry_rock_0's authored spot
const FOREMAN := Vector2i(8232, 13945) # Sten's authored spot
const ROAD_START := Vector2i(8210, 13915) # a stub beside the face, clear of the rock

var _hud: Hud
var _editor
var _player
var _t := 0.0
var _phase := "plan"
var _order_id := ""
var _pid := ""
var _pos := Vector2.ZERO
var _pickaxe_instance_id := ""
var _gold_at_login := -1
var _chop_at := 0.0

func _fail(msg: String) -> void:
	print("SMOKE_FAIL: ", msg)
	quit(1)

func _initialize() -> void:
	_hud = Hud.new()
	root.add_child(_hud)

	_editor = load("res://net/NetworkClient.gd").new()
	root.add_child(_editor)
	_editor.auth_required.connect(func(_v): _editor.login("editor@capital.town", "editor12345"))
	_editor.welcome.connect(func(d):
		if String(d.get("role", "")) != "editor":
			_fail("not editor"); return
		_editor.send_road_plan([[ROAD_START.x, ROAD_START.y], [ROAD_START.x + 10, ROAD_START.y]]))
	_editor.road_plan_error.connect(func(m): _fail("road.plan_error: %s" % m))
	_editor.road_planned.connect(func(order_id):
		_order_id = order_id
		print("SMOKE: planned ", order_id)
		_spawn_player())
	_editor.connect_to("ws://127.0.0.1:8766")

func _spawn_player() -> void:
	_player = load("res://net/NetworkClient.gd").new()
	root.add_child(_player)
	var email := "wages_%d_%d@t.test" % [Time.get_unix_time_from_system(), randi()]
	_player.auth_required.connect(func(_v): _player.register(email, "pw12345", "Wages"))
	_player.welcome.connect(func(d):
		_pid = String(d.get("player_id", ""))
		_phase = "to_sten")
	_player.status_update.connect(func(id, _zone, state):
		if id == _pid:
			_pos = Vector2(float(state.get("x", 0)), float(state.get("y", 0))))
	# Login hydration's rent.status carries the starting balance — the purse
	# readout must be correct from login, before any wage is ever earned.
	_player.rent_status.connect(func(_p, _d, _pt, _s, _a, gold):
		if _gold_at_login < 0:
			_gold_at_login = gold
			_hud.set_gold(gold)
			print("SMOKE: balance at login = ", gold))
	_player.inv_update.connect(func(items, _used, _cap):
		for it in items:
			if String(it.get("item_id", "")) == "pickaxe" and _pickaxe_instance_id == "":
				_pickaxe_instance_id = String(it.get("id", "")))
	_player.equip_update.connect(func(tool, _d, _m, _a):
		if tool == "pickaxe" and _phase == "equip":
			_phase = "to_face")
	_player.ability_result.connect(func(id, ok, _cd, _reason, item_id, _qty):
		if id == "pick" and ok and item_id == "stone" and _phase == "mining":
			_player.send_build_contribute(_order_id, "stone", 1))
	_player.gold_update.connect(func(gold, delta, reason):
		_hud.set_gold(gold)
		if _gold_at_login < 0:
			_fail("a wage landed before login hydration — can't verify the delta"); return
		if delta <= 0:
			_fail("expected a positive wage delta, got %d" % delta); return
		if reason != "build_wages":
			_fail("unexpected reason: %s" % reason); return
		if gold <= _gold_at_login:
			_fail("balance didn't rise: %d -> %d" % [_gold_at_login, gold]); return
		if _hud._gold.text != "gold: %d" % gold:
			_fail("HUD purse readout out of sync: %s vs %d" % [_hud._gold.text, gold]); return
		print("SMOKE_OK: the city paid %d gold for building (%d -> %d), HUD reads '%s'"
			% [delta, _gold_at_login, gold, _hud._gold.text])
		quit(0))
	_player.connect_to("ws://127.0.0.1:8766")

func _process(delta: float) -> bool:
	_t += delta
	match _phase:
		"to_sten":
			_player.send_move(FOREMAN.x - 12800, FOREMAN.y - 12800)
			_phase = "talk_wait"
			_t = 0.0
		"talk_wait":
			if _t > 3.0:
				_player.send_npc_talk("npc_quarry_foreman")
				_phase = "equip_wait"
				_t = 0.0
		"equip_wait":
			if _t > 1.0 and _pickaxe_instance_id != "":
				_player.send_equip(_pickaxe_instance_id)
				_phase = "equip"
				_t = 0.0
		"equip":
			pass # advances via equip_update
		"to_face":
			_player.send_move(FACE.x - FOREMAN.x, FACE.y - FOREMAN.y)
			_phase = "to_face_wait"
			_t = 0.0
		"to_face_wait":
			if _t > 2.0:
				if _pos.distance_to(Vector2(FACE)) > 8.0:
					_fail("not close enough to the face: %s" % _pos); return true
				_phase = "mining"
				_chop_at = 0.0
		"mining":
			if _t >= _chop_at:
				_player.send_ability_use("pick", "node_quarry_rock_0")
				_chop_at = _t + 2.2
	if _t > 40.0:
		_fail("timed out in phase %s (login gold=%d)" % [_phase, _gold_at_login])
		return true
	return false
