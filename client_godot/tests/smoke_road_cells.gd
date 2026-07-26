## Live progressive road-cell rendering test (#131/#132/#133/#134) against a
## running gateway: an editor plans a short (10m, 2-cell) road right next to
## the Mt Coot-tha quarry face, a real player mines real stone there and
## contributes it, and the test asserts the client's rendered pavement
## grows cell by cell — a real render-layer proof, not just the wire-level
## coverage the Rust integration tests already give #133's contribute path.
## Run: Godot --headless --path client_godot -s res://tests/smoke_road_cells.gd
extends SceneTree

const FACE := Vector2i(8210, 13900) # node_quarry_rock_0's authored spot
const FOREMAN := Vector2i(8232, 13945) # Sten's authored spot
const ROAD_START := Vector2i(8210, 13910) # 10m stub, clear of the rock itself

var _world: World
var _editor
var _player
var _t := 0.0
var _phase := "plan"
var _order_id := ""
var _pid := ""
var _pos := Vector2.ZERO
var _pickaxe_instance_id := ""
var _stone := 0
var _contributed := 0
var _chop_at := 0.0
var _saw_one_cell := false

func _fail(msg: String) -> void:
	print("SMOKE_FAIL: ", msg)
	quit(1)

func _rendered_cells() -> int:
	if not _world._road_cells.has(_order_id):
		return -1
	return _world._road_cells[_order_id]["nodes"].size()

func _initialize() -> void:
	_world = World.new()
	root.add_child(_world)

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
		print("SMOKE: planned ", order_id, " (10m, 2 cells)")
		_spawn_player())
	_editor.connect_to("ws://127.0.0.1:8766")

func _spawn_player() -> void:
	_player = load("res://net/NetworkClient.gd").new()
	root.add_child(_player)
	var email := "roadcells_%d_%d@t.test" % [Time.get_unix_time_from_system(), randi()]
	_player.auth_required.connect(func(_v): _player.register(email, "pw12345", "RoadCells"))
	_player.welcome.connect(func(d):
		_pid = String(d.get("player_id", ""))
		_player.send_road_cells_request(_order_id)
		_phase = "to_sten")
	_player.status_update.connect(func(id, _zone, state):
		if id == _pid:
			_pos = Vector2(float(state.get("x", 0)), float(state.get("y", 0))))
	_player.inv_update.connect(func(items, _used, _cap):
		for it in items:
			if String(it.get("item_id", "")) == "pickaxe" and _pickaxe_instance_id == "":
				_pickaxe_instance_id = String(it.get("id", "")))
	_player.equip_update.connect(func(tool, _d, _m, _a):
		if tool == "pickaxe" and _phase == "equip":
			print("SMOKE: pickaxe equipped")
			_phase = "to_face")
	_player.ability_result.connect(func(id, ok, _cd, _reason, item_id, qty):
		if id == "pick" and ok and item_id == "stone":
			_stone += qty
			if _phase == "mining":
				_player.send_build_contribute(_order_id, "stone", 1)
				_contributed += 1)
	_player.road_cells.connect(func(order_id, cells):
		if order_id == _order_id:
			print("SMOKE: got road.cells (%d cells)" % cells.size())
			_world.set_road_cells(order_id, cells)
			if cells.size() != 2:
				_fail("expected 2 cells for a 10m road, got %d" % cells.size())
				return
			if _rendered_cells() != 0:
				_fail("a fresh plan should render zero pavement, got %d" % _rendered_cells())
				return)
	_player.road_cell_progress.connect(func(order_id, cell_index, required, progress, completed):
		if order_id != _order_id:
			return
		_world.set_road_cell_progress(order_id, cell_index, required, progress, completed)
		var rendered := _rendered_cells()
		if rendered == 1 and not _saw_one_cell:
			_saw_one_cell = true
			print("SMOKE: first cell completed -> 1 segment rendered")
		if rendered == 2:
			print("SMOKE_OK: both cells completed -> 2 segments rendered, pavement grew cell by cell")
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
		_fail("timed out in phase %s (stone=%d contributed=%d rendered=%d)" % [_phase, _stone, _contributed, _rendered_cells()])
		return true
	return false
