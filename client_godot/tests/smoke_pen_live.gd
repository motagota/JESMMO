## Live starting-area pen test (#90) against a running gateway with the pen
## seeded (scripts/seed_poison_pen.py): the poison-forest walls and the river
## both kill. Self-locating — it reads the tree line from object.list and
## finds the river by probing southward until the server flags submerged —
## so it survives the pen being re-authored. A guest account, walk-speed
## movement, nothing persisted: rerunnable as-is.
## Run: Godot --headless --path client_godot -s res://tests/smoke_pen_live.gd
extends SceneTree

const WALK_SPEED := 8.0 # the client's real movement rate, m/s
const SPAWN := Vector2(12800, 12800)

var _net
var _pid := ""
var _t := 0.0
var _phase := "auth"
var _pos := Vector2.ZERO
var _fpos := Vector2.ZERO # float walker, integer deltas sent
var _walk_dir := Vector2.ZERO
var _walk_left := 0.0
var _trees: Array = []
var _forest_death := false
var _hop_wait := 0.0
var _hops := 0

func _fail(message: String) -> void:
	print("SMOKE_FAIL: %s" % message)
	quit(1)

func _initialize() -> void:
	_net = load("res://net/NetworkClient.gd").new()
	root.add_child(_net)
	_net.auth_required.connect(func(_v): _net.guest())
	_net.welcome.connect(func(d):
		_pid = String(d.get("player_id", ""))
		_net.send_object_list())
	_net.object_list.connect(func(objects):
		for o in objects:
			if String(o.get("kind", "")) == "poison_tree":
				_trees.append(Vector2(float(o.get("x", 0)), float(o.get("y", 0))))
		if _trees.size() < 1500:
			_fail("pen not seeded: only %d poison trees (run scripts/seed_poison_pen.py)" % _trees.size())
			return
		print("SMOKE: pen present (%d trees)" % _trees.size())
		# An idle guest gets no further own status updates — nudge one step so
		# the auth phase (which runs off our own status) gets a fresh reading.
		_net.send_move(1, 0))
	_net.you_died.connect(func():
		if _phase == "walk_forest" or _phase == "await_forest_death":
			if not _forest_death:
				_fail("died in the forest without ever being poisoned?")
				return
			print("SMOKE: forest wall kill confirmed — respawning, on to the river")
			_phase = "respawn_wait"
		elif _phase == "drowning":
			print("SMOKE_OK: the pen holds — poison-forest wall kill + river drowning kill, both self-located")
			quit(0))
	_net.status_update.connect(_on_status)
	_net.connect_to("ws://127.0.0.1:8766")

func _process(delta: float) -> bool:
	_t += delta
	if _t > 150.0:
		_fail("SMOKE_TIMEOUT phase=%s pos=%s" % [_phase, str(_pos)])
		return true
	match _phase:
		"walk_forest", "await_forest_death":
			# Keep walking through/into the band even after the proc (the DoT
			# does the killing; movement just proves a walker can't outrun it).
			if _walk_left > 0.0:
				var step := _walk_dir * WALK_SPEED * delta
				var before := Vector2i(_fpos.round())
				_fpos += step
				_walk_left -= step.length()
				var d := Vector2i(_fpos.round()) - before
				if d != Vector2i.ZERO:
					_net.send_move(d.x, d.y)
		"hop_river":
			# 200m southward hops, pausing ~2s each for the env tick's verdict.
			_hop_wait -= delta
			if _hop_wait <= 0.0:
				if _hops >= 16:
					_fail("no water found within %dm south of spawn" % (_hops * 200))
					return true
				_hops += 1
				_net.send_move(0, 200)
				_hop_wait = 2.0
	return false

func _on_status(id: String, _zone: String, state: Dictionary) -> void:
	if id != _pid:
		return
	_pos = Vector2(float(state.get("x", 0)), float(state.get("y", 0)))
	match _phase:
		"auth":
			if _trees.is_empty():
				return # roster not in yet; next update tries again
			if int(state.get("poison_buildup", 1)) != 0 or bool(state.get("submerged", true)):
				_fail("spawn area must be hazard-free (buildup=%s submerged=%s)" % [state.get("poison_buildup"), state.get("submerged")])
				return
			# Teleport to 100m short of the tree nearest spawn, then walk
			# through the wall at the real 8 m/s.
			var nearest := _trees[0] as Vector2
			for t_v in _trees:
				if (t_v as Vector2).distance_squared_to(SPAWN) < nearest.distance_squared_to(SPAWN):
					nearest = t_v
			var dir := (nearest - SPAWN).normalized()
			var start := nearest - dir * 100.0
			_net.send_move(int(round(start.x - _pos.x)), int(round(start.y - _pos.y)))
			_walk_dir = dir
			_walk_left = 320.0
			_fpos = start
			_phase = "walk_forest"
			print("SMOKE: nearest wall tree %s — walking through the line" % str(nearest))
		"walk_forest":
			if bool(state.get("poisoned", false)) and not _forest_death:
				_forest_death = true
				print("SMOKE: PROC at %s (buildup %s) — the wall caught us" % [str(_pos), state.get("poison_buildup")])
				_phase = "await_forest_death"
		"respawn_wait":
			if int(state.get("hp", 0)) == int(state.get("max_hp", -1)) and not bool(state.get("poisoned", true)):
				_phase = "hop_river"
				_hop_wait = 0.0
				print("SMOKE: respawned clean — probing south for the river")
		"hop_river":
			if bool(state.get("submerged", false)):
				_phase = "drowning"
				print("SMOKE: submerged at %s — waiting out breath + suffocation" % str(_pos))
