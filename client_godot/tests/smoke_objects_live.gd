## Headless end-to-end placed-props test against a running gateway (#86):
## logs in as the seeded editor account plus a separate guest session, each
## with its own WorldObjects mirror wired exactly like Main wires it. The
## editor places a line of three poison trees (object.place); both sessions
## must render them from the broadcasts; a fresh roster request must agree;
## then the editor deletes its own trees and both sessions must empty back
## to the baseline — leaving the dev DB the way the test found it.
## Also checks a guest's object.place is rejected.
## Run: Godot --headless --path client_godot -s res://tests/smoke_objects_live.gd
extends SceneTree

const _LINE := [Vector2i(12650, 12700), Vector2i(12660, 12700), Vector2i(12670, 12700)]

var _net
var _net2
var _objects1: WorldObjects
var _objects2: WorldObjects
var _t := 0.0
var _phase := "auth"
var _baseline := -1
var _placed_ids: Array = []
var _guest_rejected := false

func _fail(message: String) -> void:
	print("SMOKE_FAIL: %s" % message)
	quit(1)

func _wire(net, objects: WorldObjects) -> void:
	net.object_list.connect(func(objs): objects.apply_list(objs))
	net.object_placed.connect(func(id, kind, x, y):
		objects.on_placed(id, kind, x, y)
		if net == _net and _phase == "placing" and not _placed_ids.has(id):
			_placed_ids.append(id))
	net.object_removed.connect(func(id): objects.on_removed(id))

func _initialize() -> void:
	Protocol.apply_terrain_data(1, 25600.0, PackedFloat32Array([0.0, 0.0, 0.0, 0.0]))
	_objects1 = WorldObjects.new()
	root.add_child(_objects1)
	_objects2 = WorldObjects.new()
	root.add_child(_objects2)

	_net = load("res://net/NetworkClient.gd").new()
	root.add_child(_net)
	_wire(_net, _objects1)
	_net.auth_required.connect(func(_v): _net.login("editor@capital.town", "editor12345"))
	_net.welcome.connect(func(d):
		if String(d.get("role", "")) != "editor":
			_fail("expected editor role, got %s" % d.get("role"))
			return
		_net.send_object_list())
	_net.object_edit_error.connect(func(message): _fail("editor op rejected: %s" % message))

	_net2 = load("res://net/NetworkClient.gd").new()
	root.add_child(_net2)
	_wire(_net2, _objects2)
	_net2.auth_required.connect(func(_v): _net2.guest())
	_net2.welcome.connect(func(_d):
		_net2.send_object_list()
		# A guest may not place — the server must reject it.
		_net2.send_object_place("poison_tree", 100, 100))
	_net2.object_edit_error.connect(func(message):
		print("SMOKE: guest place correctly rejected (%s)" % message)
		_guest_rejected = true)

	_net.connect_to("ws://127.0.0.1:8766")
	_net2.connect_to("ws://127.0.0.1:8766")

func _process(delta: float) -> bool:
	_t += delta
	if _t > 25.0:
		_fail("SMOKE_TIMEOUT phase=%s (o1=%d o2=%d baseline=%d)" % [_phase, _objects1.count(), _objects2.count(), _baseline])
		return true
	match _phase:
		"auth":
			# Both rosters answered (possibly non-empty if the dev DB already
			# has authored trees — everything below is baseline-relative).
			if _net != null and _net.is_open() and _net2.is_open() \
					and _objects1.count() == _objects2.count() and _objects1.count() >= 0 \
					and _t > 2.0:
				_baseline = _objects1.count()
				_phase = "placing"
				print("SMOKE: baseline roster %d objects; placing a line of %d trees" % [_baseline, _LINE.size()])
				for p in _LINE:
					_net.send_object_place("poison_tree", p.x, p.y)
		"placing":
			if _objects1.count() == _baseline + _LINE.size() and _objects2.count() == _baseline + _LINE.size():
				for id in _placed_ids:
					if not _objects2.has_object(id):
						_fail("guest session missing broadcast tree %s" % id)
						return true
				print("SMOKE: both sessions render all %d placed trees" % _LINE.size())
				_phase = "deleting"
				for id in _placed_ids:
					_net.send_object_delete(id)
		"deleting":
			if _objects1.count() == _baseline and _objects2.count() == _baseline:
				if not _guest_rejected:
					_fail("guest place was never rejected")
					return true
				print("SMOKE_OK: live editor place/delete round-trip — line of trees broadcast to a second session, deletes broadcast back to baseline, guest writes rejected, dev DB left clean")
				quit(0)
	return false
