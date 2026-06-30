## Headless end-to-end smoke test of the real networking stack against a running
## gateway. Drives NetworkClient: connect -> (auth_required) guest -> welcome.
## Run: Godot --headless --path client_godot -s res://tests/smoke.gd
## Exits non-zero on timeout so it can gate CI.
extends SceneTree

var _net
var _t := 0.0
var _done := false

func _initialize() -> void:
	_net = load("res://net/NetworkClient.gd").new()
	root.add_child(_net)
	_net.auth_required.connect(func(v): print("SMOKE: auth_required v", v); _net.guest())
	_net.welcome.connect(func(d):
		print("SMOKE_OK welcome player=", d.get("player_id"), " zone=", d.get("zone"))
		_done = true)
	_net.status_update.connect(func(id, zone, state):
		print("SMOKE: status_update id=", id, " pos=(", state.get("x"), ",", state.get("y"), ")"))
	_net.connect_to("ws://127.0.0.1:8766")

func _process(delta: float) -> bool:
	_t += delta
	if _done:
		print("SMOKE: done")
		return true
	if _t > 10.0:
		push_error("SMOKE_TIMEOUT: no welcome within 10s")
		quit(1)
		return true
	return false
