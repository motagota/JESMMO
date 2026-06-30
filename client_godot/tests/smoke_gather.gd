## Headless end-to-end gather test against a running gateway + zone.
## Registers a character (spawns at the town centre), walks to the authored civic
## tree, gathers, and asserts the gateway pushes an inv.update containing wood.
## Run: Godot --headless --path client_godot -s res://tests/smoke_gather.gd
extends SceneTree

var _net
var _t := 0.0
var _phase := "auth" # auth -> move -> gather -> wait
var _moves := 0
var _got_wood := false

func _initialize() -> void:
	randomize()
	_net = load("res://net/NetworkClient.gd").new()
	root.add_child(_net)
	_net.auth_required.connect(func(_v):
		var email := "gather_%d_%d@t.test" % [Time.get_ticks_msec(), randi()]
		_net.register(email, "pw12", "Gatherer"))
	_net.welcome.connect(func(d):
		print("SMOKE: welcome ", d.get("player_id"))
		_phase = "move")
	_net.inv_update.connect(func(items):
		for it in items:
			if String(it.get("item_id", "")) == "wood" and int(it.get("qty", 0)) >= 1:
				_got_wood = true)
	_net.skill_update.connect(func(sid, xp, lvl):
		print("SMOKE: skill ", sid, " xp=", xp, " lv=", lvl))
	_net.connect_to("ws://127.0.0.1:8766")

func _process(delta: float) -> bool:
	_t += delta
	if _got_wood:
		print("SMOKE_GATHER_OK inventory shows wood")
		return true
	if _t > 20.0:
		push_error("SMOKE_GATHER_TIMEOUT phase=%s" % _phase)
		quit(1)
		return true
	match _phase:
		"move":
			# Town centre (600,600) -> civic tree at (540,540): step NW into range.
			if _moves < 8:
				_net.send_move(-10, -10)
				_moves += 1
			else:
				_phase = "gather"
		"gather":
			_net.send_gather_start("node_civic_tree_0")
			_phase = "wait"
		_:
			pass
	return false
