## Live end-to-end vitals test (#89) against a running gateway: a guest
## session wired exactly like Main (status_update -> set_vitals, you_died ->
## show_death) jumps into the river; the breath meter must appear and drain
## with the SERVER's values, the death overlay must fire, and the respawn's
## clean vitals must restore the bars. Screenshots at the draining and death
## moments.
## Run: Godot --path client_godot -s res://tests/smoke_vitals_live.gd
## Screenshots: user://vitals_live_drain.png, user://vitals_live_death.png
extends SceneTree

const RIVER := Vector2i(13300, 14700) # nearest solid water to spawn (CBD S-bend)

var _net
var _v: VitalsHud
var _pid := ""
var _t := 0.0
var _phase := "auth"
var _jumped := false
var _drain_shot := false
var _death_shot_at := -1.0
var _death_shot := false
var _last_breath := -1

func _fail(message: String) -> void:
	print("SMOKE_FAIL: %s" % message)
	quit(1)

func _initialize() -> void:
	var env := Environment.new()
	env.background_mode = Environment.BG_COLOR
	env.background_color = Color(0.35, 0.45, 0.38)
	var we := WorldEnvironment.new()
	we.environment = env
	root.add_child(we)
	var cam := Camera3D.new()
	root.add_child(cam)
	cam.make_current()

	_v = VitalsHud.new()
	root.add_child(_v)

	_net = load("res://net/NetworkClient.gd").new()
	root.add_child(_net)
	_net.auth_required.connect(func(_ver): _net.guest())
	_net.welcome.connect(func(d): _pid = String(d.get("player_id", "")))
	_net.you_died.connect(func():
		_v.show_death()
		if _phase == "drowning":
			_phase = "dead"
			_death_shot_at = _t + 0.2) # give the overlay a couple of frames
	_net.status_update.connect(func(id, _zone, state):
		if id != _pid:
			return
		_v.set_vitals(
			int(state.get("hp", 100)), int(state.get("max_hp", 100)),
			int(state.get("breath", 0)), int(state.get("max_breath", 1)),
			bool(state.get("submerged", false)),
			int(state.get("poison_buildup", 0)), int(state.get("max_poison", 1)),
			bool(state.get("poisoned", false)))
		if not _jumped:
			_jumped = true
			_phase = "swimming"
			_net.send_move(RIVER.x - int(state.get("x", 0)), RIVER.y - int(state.get("y", 0)))
			return
		var breath := int(state.get("breath", -1))
		if _phase == "swimming" and bool(state.get("submerged", false)):
			if _last_breath > 0 and breath < _last_breath and not _drain_shot and breath < 160:
				_drain_shot = true
				root.get_viewport().get_texture().get_image().save_png("user://vitals_live_drain.png")
				if not _v._breath_row.visible:
					_fail("breath meter should be out while submerged"); return
				print("SMOKE: breath draining on the real wire (%d), meter visible — screenshot taken" % breath)
				_phase = "drowning"
			_last_breath = breath
		elif _phase == "dead" and int(state.get("hp", 0)) == int(state.get("max_hp", -1)):
			# The respawn stream restored full vitals under the fading overlay.
			if _v._breath_row.visible and _last_breath < 0:
				_fail("respawn should eventually tuck the breath meter away"); return
			print("SMOKE: respawn restored full vitals (hp back to max)")
			_phase = "respawned")
	_net.connect_to("ws://127.0.0.1:8766")

func _process(delta: float) -> bool:
	_t += delta
	if _phase == "drowning":
		_last_breath = -1 # stop tracking; wait for you_died
	if _death_shot_at > 0 and _t >= _death_shot_at:
		_death_shot_at = -1
		_death_shot = true
		root.get_viewport().get_texture().get_image().save_png("user://vitals_live_death.png")
		if not _v._death_overlay.visible:
			_fail("death overlay should be up right after you_died")
			return true
		print("SMOKE: death overlay up — screenshot taken")
	if _phase == "respawned" and _death_shot:
		print("SMOKE_OK: live vitals — breath meter drained with server values, death overlay fired, respawn restored the bars")
		quit(0)
	if _t > 45.0:
		_fail("SMOKE_TIMEOUT phase=%s" % _phase)
		return true
	return false
