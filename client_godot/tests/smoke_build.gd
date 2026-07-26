## Headless end-to-end build-order test against a running gateway + zone
## (rewritten in #127 — bare-hands gathering was retired in #125). Registers
## a character, reads the district's build board (`build.list`), walks to the
## logging camp foreman, talks (grants an axe), equips it, chops wood at a
## real camp tree, walks to the town build board, contributes the wood to
## the Town Well, and asserts the order's progress reflects the contribution.
## Order completion + XP + unlock are covered by the Rust proxy integration
## test; this proves the real Godot client can drive the swing -> contribute
## loop end to end.
## Run: Godot --headless --path client_godot -s res://tests/smoke_build.gd
extends SceneTree

var _net
var _t := 0.0
var _phase := "auth" # auth -> to_elke -> talk_wait -> talk -> equip -> to_tree -> chop_wait -> chop -> to_board -> contribute -> wait_progress
var _wood_qty := 0
var _well_id := ""
var _well_wood_progress := -1
var _saw_board := false
var _chop_cooldown_at := 0.0
var _axe_instance_id := ""

func _initialize() -> void:
	randomize()
	_net = load("res://net/NetworkClient.gd").new()
	root.add_child(_net)
	_net.auth_required.connect(func(_v):
		var email := "build_%d_%d@t.test" % [Time.get_ticks_msec(), randi()]
		_net.register(email, "pw12", "Builder"))
	_net.welcome.connect(func(d):
		print("SMOKE: welcome ", d.get("player_id"))
		_phase = "to_elke")
	# The build board must actually be pushed to the (registered) client, so the
	# player can find it in the world.
	_net.status_update.connect(func(_id, _zone, state):
		if String(state.get("type", "")) == "build_board":
			_saw_board = true)
	_net.build_list.connect(func(orders):
		for o in orders:
			if String(o.get("kind", "")) == "town_well":
				_well_id = String(o.get("order_id", ""))
				var prog: Dictionary = o.get("progress", {})
				_well_wood_progress = int(prog.get("wood", 0)))
	_net.build_progress.connect(func(order_id, _required, progress):
		if order_id == _well_id:
			_well_wood_progress = int(progress.get("wood", 0)))
	_net.npc_dialogue.connect(func(_npc_id, npc_name, _lines, granted):
		if _phase == "talk":
			print("SMOKE: talked to ", npc_name, " granted=", granted)
			_phase = "equip")
	_net.equip_update.connect(func(tool, _durability, _max_durability, _abilities):
		if _phase == "equip" and tool == "axe":
			print("SMOKE: axe equipped")
			_phase = "to_tree")
	_net.inv_update.connect(func(items, _used, _capacity):
		for it in items:
			var item_id := String(it.get("item_id", ""))
			if item_id == "wood":
				_wood_qty = int(it.get("qty", 0))
			elif item_id == "axe" and _axe_instance_id == "":
				_axe_instance_id = String(it.get("id", "")))
	_net.connect_to("ws://127.0.0.1:8766")

func _process(delta: float) -> bool:
	_t += delta
	if _phase == "wait_progress" and _well_wood_progress >= 1:
		if not _saw_board:
			push_error("SMOKE_BUILD_NO_BOARD logged-in player never received the build board entity")
			quit(1)
			return true
		print("SMOKE_BUILD_OK town_well holds wood x%d, board rendered" % _well_wood_progress)
		return true
	if _t > 45.0:
		push_error("SMOKE_BUILD_TIMEOUT phase=%s well=%s wood=%d progress=%d" % [
			_phase, _well_id, _wood_qty, _well_wood_progress])
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
			if _axe_instance_id != "":
				_net.send_equip(_axe_instance_id)
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
				_phase = "to_board"
			elif _t >= _chop_cooldown_at:
				_net.send_ability_use("chop", "node_logging_tree_0")
				_chop_cooldown_at = _t + 2.1 # base cooldown at woodcutting Lv0 + slack
		"to_board":
			# Tree (14280,11380) -> the town build board (12770,12810).
			_net.send_move(12770 - 14280, 12810 - 11380)
			_phase = "board_wait"
		"board_wait":
			if _t > 4.5:
				_phase = "contribute"
		"contribute":
			if _well_id == "":
				push_error("SMOKE_BUILD_NO_ORDER never received the town_well order")
				quit(1)
				return true
			_net.send_build_contribute(_well_id, "wood", _wood_qty)
			_phase = "wait_progress"
		_:
			pass
	return false
