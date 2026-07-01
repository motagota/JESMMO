## Headless end-to-end build-order test against a running gateway + zone.
## Registers a character (spawns at the town centre), reads the district's build
## board (`build.list`), gathers wood from the authored civic tree, walks to the
## town build board, contributes the wood to the Town Well, and asserts the order's
## progress reflects the contribution (`build.progress`). Order completion + XP +
## unlock are covered by the Rust proxy integration test; this proves the real
## Godot client can drive the gather -> contribute loop end to end.
## Run: Godot --headless --path client_godot -s res://tests/smoke_build.gd
extends SceneTree

var _net
var _t := 0.0
var _phase := "auth" # auth -> to_tree -> gather -> wait_wood -> to_board -> contribute -> wait_progress
var _moves := 0
var _wood_qty := 0
var _well_id := ""
var _well_wood_progress := -1

func _initialize() -> void:
	randomize()
	_net = load("res://net/NetworkClient.gd").new()
	root.add_child(_net)
	_net.auth_required.connect(func(_v):
		var email := "build_%d_%d@t.test" % [Time.get_ticks_msec(), randi()]
		_net.register(email, "pw12", "Builder"))
	_net.welcome.connect(func(d):
		print("SMOKE: welcome ", d.get("player_id"))
		_phase = "to_tree")
	_net.build_list.connect(func(orders):
		for o in orders:
			if String(o.get("kind", "")) == "town_well":
				_well_id = String(o.get("order_id", ""))
				var prog: Dictionary = o.get("progress", {})
				_well_wood_progress = int(prog.get("wood", 0)))
	_net.build_progress.connect(func(order_id, _required, progress):
		if order_id == _well_id:
			_well_wood_progress = int(progress.get("wood", 0)))
	_net.inv_update.connect(func(items, _used, _capacity):
		for it in items:
			if String(it.get("item_id", "")) == "wood":
				_wood_qty = int(it.get("qty", 0)))
	_net.connect_to("ws://127.0.0.1:8766")

func _process(delta: float) -> bool:
	_t += delta
	if _phase == "wait_progress" and _well_wood_progress >= 1:
		print("SMOKE_BUILD_OK town_well holds wood x", _well_wood_progress)
		return true
	if _t > 40.0:
		push_error("SMOKE_BUILD_TIMEOUT phase=%s well=%s wood=%d progress=%d" % [
			_phase, _well_id, _wood_qty, _well_wood_progress])
		quit(1)
		return true
	match _phase:
		"to_tree":
			# Town centre (600,600) -> civic tree at (540,540): step NW into range.
			if _moves < 8:
				_net.send_move(-10, -10)
				_moves += 1
			else:
				_moves = 0
				_phase = "gather"
		"gather":
			_net.send_gather_start("node_civic_tree_0")
			_phase = "wait_wood"
		"wait_wood":
			if _wood_qty >= 2:
				print("SMOKE: gathered wood x", _wood_qty)
				_phase = "to_board"
		"to_board":
			# Tree (~520,520) -> build board at (570,610): step SE into range.
			if _moves < 10:
				_net.send_move(10, 10)
				_moves += 1
			else:
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
