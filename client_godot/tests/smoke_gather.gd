## Headless end-to-end resource-to-storage test against a running gateway +
## zone (rewritten in #127 — bare-hands gathering was retired in #125; every
## resource is ability-swing-gated now). Registers a character, walks to the
## logging camp foreman, talks (grants an axe), equips it, walks to a real
## camp tree, swings Chop until 2 wood are banked, walks to the town
## storehouse, deposits, and asserts the storehouse holds the wood.
## Run: Godot --headless --path client_godot -s res://tests/smoke_gather.gd
extends SceneTree

var _net
var _t := 0.0
var _phase := "auth" # auth -> to_elke -> talk -> equip -> to_tree -> chop -> to_store -> deposit -> wait_store
var _wood_qty := 0
var _stored_wood := 0
var _chop_cooldown_at := 0.0

func _initialize() -> void:
	randomize()
	_net = load("res://net/NetworkClient.gd").new()
	root.add_child(_net)
	_net.auth_required.connect(func(_v):
		var email := "gather_%d_%d@t.test" % [Time.get_ticks_msec(), randi()]
		_net.register(email, "pw12", "Gatherer"))
	_net.welcome.connect(func(d):
		print("SMOKE: welcome ", d.get("player_id"))
		_phase = "to_elke")
	_net.npc_dialogue.connect(func(_npc_id, npc_name, _lines, granted):
		if _phase == "talk":
			print("SMOKE: talked to ", npc_name, " granted=", granted)
			_phase = "equip")
	_net.equip_update.connect(func(tool, _abilities):
		if _phase == "equip" and tool == "axe":
			print("SMOKE: axe equipped")
			_phase = "to_tree")
	_net.inv_update.connect(func(items, _used, _capacity):
		for it in items:
			if String(it.get("item_id", "")) == "wood":
				_wood_qty = int(it.get("qty", 0)))
	_net.store_update.connect(func(items):
		for it in items:
			if String(it.get("item_id", "")) == "wood":
				_stored_wood = int(it.get("qty", 0)))
	_net.connect_to("ws://127.0.0.1:8766")

func _process(delta: float) -> bool:
	_t += delta
	if _stored_wood >= 1:
		print("SMOKE_STORE_OK storehouse holds wood x", _stored_wood)
		return true
	if _t > 30.0:
		push_error("SMOKE_STORE_TIMEOUT phase=%s wood=%d stored=%d" % [_phase, _wood_qty, _stored_wood])
		quit(1)
		return true
	match _phase:
		"to_elke":
			# Town centre (12800,12800) -> the logging foreman (14300,11400).
			_net.send_move(14300 - 12800, 11400 - 12800)
			_phase = "talk_wait"
		"talk_wait":
			if _t > 3.0:
				_net.send_npc_talk("npc_logging_foreman")
				_phase = "talk"
		# "talk" advances to "equip" via npc_dialogue above.
		"equip":
			_net.send_equip("axe")
			# advances to "to_tree" via equip_update above once tool == "axe"
		"to_tree":
			# Elke (14300,11400) -> the nearest camp tree (14280,11380).
			_net.send_move(14280 - 14300, 11380 - 11400)
			_phase = "chop_wait"
		"chop_wait":
			if _t > 4.5:
				_chop_cooldown_at = 0.0
				_phase = "chop"
		"chop":
			if _wood_qty >= 2:
				print("SMOKE: chopped wood x", _wood_qty)
				_phase = "to_store"
			elif _t >= _chop_cooldown_at:
				_net.send_ability_use("chop", "node_logging_tree_0")
				_chop_cooldown_at = _t + 2.1 # base cooldown at woodcutting Lv0 + slack
		"to_store":
			# Tree (14280,11380) -> the town storehouse (12830,12810).
			_net.send_move(12830 - 14280, 12810 - 11380)
			_phase = "store_wait"
		"store_wait":
			if _t > 4.5:
				_phase = "deposit"
		"deposit":
			_net.send_store_deposit("wood", _wood_qty)
			_phase = "wait_store"
		_:
			pass
	return false
