## Headless end-to-end gather + deposit test against a running gateway + zone.
## Registers a character (spawns at the town centre), walks to the authored civic
## tree, gathers wood, walks to the town storehouse, deposits, and asserts the
## storehouse holds the wood.
## Run: Godot --headless --path client_godot -s res://tests/smoke_gather.gd
extends SceneTree

var _net
var _t := 0.0
var _phase := "auth" # auth -> to_tree -> gather -> wait_wood -> to_store -> deposit -> wait_store
var _moves := 0
var _wood_qty := 0
var _stored_wood := 0
var _resources := {} # id -> true, resource nodes the client actually received

func _initialize() -> void:
	randomize()
	_net = load("res://net/NetworkClient.gd").new()
	root.add_child(_net)
	_net.auth_required.connect(func(_v):
		var email := "gather_%d_%d@t.test" % [Time.get_ticks_msec(), randi()]
		_net.register(email, "pw12", "Gatherer"))
	_net.welcome.connect(func(d):
		print("SMOKE: welcome ", d.get("player_id"))
		_phase = "to_tree")
	# Track resource nodes the server actually pushes to us — a registered player
	# spawns via spawn_entity, so this verifies that path sends the nodes too.
	_net.status_update.connect(func(id, _zone, state):
		if String(state.get("type", "")) == "resource":
			_resources[id] = true)
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
		"to_tree":
			# Town centre (600,600) -> civic tree at (540,540): step NW into range.
			if _moves < 8:
				_net.send_move(-10, -10)
				_moves += 1
			else:
				_moves = 0
				_phase = "gather"
		"gather":
			# The node we walked to must have been pushed to us; if a logged-in
			# player receives no resources, fail loudly rather than time out.
			if not _resources.has("node_civic_tree_0"):
				push_error("SMOKE_NO_RESOURCES logged-in player received no resource nodes (seen=%d)" % _resources.size())
				quit(1)
				return true
			_net.send_gather_start("node_civic_tree_0")
			_phase = "wait_wood"
		"wait_wood":
			if _wood_qty >= 2:
				print("SMOKE: gathered wood x", _wood_qty)
				_phase = "to_store"
		"to_store":
			# Tree (520,520) -> storehouse at (630,610): step SE into range.
			if _moves < 14:
				_net.send_move(10, 8)
				_moves += 1
			else:
				_phase = "deposit"
		"deposit":
			_net.send_store_deposit("wood", _wood_qty)
			_phase = "wait_store"
		_:
			pass
	return false
