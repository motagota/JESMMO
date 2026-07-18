## Headless smoke test for the road tool + staked plans (#95): the snapping
## and cost math, the anchor/corner/commit state machine (driven through the
## same methods the mouse poll calls), the tool-exclusivity contract, and
## World.apply_road_plans' replace-not-merge staking.
## Run: Godot --headless --path client_godot -s res://tests/smoke_road_tool.gd
extends SceneTree

func _fail(message: String) -> void:
	print("SMOKE_FAIL: %s" % message)
	quit(1)

func _initialize() -> void:
	# --- snapping / cost math ---------------------------------------------------
	if RoadTool.snap_lattice(Vector2(100.4, 99.6)) != Vector2i(100, 100):
		_fail("lattice snap should round to the nearest metre"); return
	# Dominant axis east; y locked to the last corner.
	if RoadTool.snap_next_point(Vector2i(100, 100), Vector2(130.3, 111.0)) != Vector2i(130, 100):
		_fail("pending leg should run along the dominant axis (east)"); return
	# Dominant axis south.
	if RoadTool.snap_next_point(Vector2i(100, 100), Vector2(108.0, 140.7)) != Vector2i(100, 141):
		_fail("pending leg should run along the dominant axis (south)"); return
	if RoadTool.path_length([Vector2i(0, 0), Vector2i(100, 0), Vector2i(100, 200)]) != 300:
		_fail("path length should sum the runs"); return
	if RoadTool.stone_cost(300) != 75 or RoadTool.stone_cost(4) != Protocol.ROAD_MIN_STONE:
		_fail("cost mirror should be length/4 with the floor"); return

	# --- the laying state machine ------------------------------------------------
	var tool := RoadTool.new()
	root.add_child(tool)
	var committed: Array = [] # box: lambdas can't reassign captured locals
	tool.plan_committed.connect(func(points): committed.append(points))
	var modes: Array = []
	tool.mode_changed.connect(func(active): modes.append(active))

	tool.set_active(true)
	tool.commit()
	if not committed.is_empty():
		_fail("committing with no runs must be refused"); return
	tool.anchor(Vector2(12800.2, 12799.8))
	tool.add_corner(Vector2(12900.4, 12805.0)) # east-dominant -> (12900, 12800)
	tool.add_corner(Vector2(12902.0, 12950.6)) # south-dominant -> (12900, 12951)
	tool.add_corner(Vector2(12900.3, 12951.2)) # cursor hasn't left the corner: no-op
	if tool.points.size() != 3:
		_fail("expected anchor + two corners (got %d)" % tool.points.size()); return
	tool.commit()
	if committed.size() != 1 or committed[0] != [[12800, 12800], [12900, 12800], [12900, 12951]]:
		_fail("commit should emit the exact lattice polyline (got %s)" % str(committed)); return
	if not tool.points.is_empty():
		_fail("commit should clear the tool for the next road"); return

	# Cancel drops everything without emitting.
	committed.clear()
	tool.anchor(Vector2(10, 10))
	tool.add_corner(Vector2(60, 10))
	tool.cancel()
	tool.commit()
	if not committed.is_empty() or not tool.points.is_empty():
		_fail("cancel should drop the pending plan"); return

	tool.set_active(false)
	if modes != [true, false]:
		_fail("mode_changed should fire per transition (got %s)" % str(modes)); return

	# --- staked-plan rendering ----------------------------------------------------
	Protocol.apply_terrain_data(1, 6400.0, PackedFloat32Array([0.0, 0.0, 0.0, 0.0]))
	var world = load("res://world/World.gd").new()
	root.add_child(world)
	var orders := [
		{"order_id": "r1", "state": "open", "path": [[100, 100], [200, 100], [200, 300]], "kind": "road_x"},
		{"order_id": "r2", "state": "completed", "path": [[10, 10], [90, 10]], "kind": "road_y"},
		{"order_id": "w1", "state": "open", "kind": "town_well"},
	]
	world.apply_road_plans(orders)
	if world._road_plans.size() != 1 or not world._road_plans.has("r1"):
		_fail("only OPEN orders with a path get staked (got %s)" % str(world._road_plans.keys())); return
	# A two-run plan renders 2 strip ribbons + marching survey stakes + the
	# floating "planned road" label — assert the full surveyor kit is out.
	if world._road_plans["r1"]["nodes"].size() < 10:
		_fail("a 300m plan should stake strips + stakes + label (got %d nodes)" % world._road_plans["r1"]["nodes"].size()); return
	world.remove_road_plan("r1")
	if not world._road_plans.is_empty():
		_fail("completion should drop the stakes"); return
	# Replace-not-merge: a fresh push with no roads clears everything.
	world.apply_road_plans(orders)
	world.apply_road_plans([])
	if not world._road_plans.is_empty():
		_fail("a board push without the order should un-stake it"); return

	# --- move mode (#105) --------------------------------------------------------
	world.apply_road_plans(orders) # re-stake r1 for picking
	tool.world_ref = world
	var replans: Array = []
	tool.replan_committed.connect(func(oid, pts): replans.append([oid, pts]))

	# Pick math: nearest run within radius; nothing outside it.
	if RoadTool.pick_plan(world._road_plans, Vector2(150, 104), 10.0) != "r1":
		_fail("pick_plan should find r1 4m from its first run"); return
	if RoadTool.pick_plan(world._road_plans, Vector2(150, 140), 10.0) != "":
		_fail("pick_plan should find nothing 40m out"); return

	tool.set_move_active(true)
	if not tool.active or not tool.move_mode:
		_fail("set_move_active should enter move mode"); return
	tool.pick_at(Vector2(150, 103))
	if tool.editing_order_id != "r1" or tool.points != [Vector2i(100, 100), Vector2i(200, 100), Vector2i(200, 300)]:
		_fail("picking should load the plan's exact polyline (got %s / %s)" % [tool.editing_order_id, str(tool.points)]); return
	# Edit: trim the last corner, extend east instead.
	tool.points.pop_back()
	tool.add_corner(Vector2(260.4, 102.0))
	tool.commit()
	if replans.size() != 1 or replans[0][0] != "r1" or replans[0][1] != [[100, 100], [200, 100], [260, 100]]:
		_fail("move commit should emit road.replan with the edited polyline (got %s)" % str(replans)); return
	if tool.editing_order_id != "" or not tool.points.is_empty():
		_fail("move commit should clear the editing state"); return
	# A fresh pick then Esc drops the selection without emitting.
	tool.pick_at(Vector2(150, 103))
	tool.cancel()
	if replans.size() != 1 or tool.editing_order_id != "":
		_fail("cancel should drop the picked plan silently"); return
	tool.set_active(false)

	# --- demolish tool (#107) ----------------------------------------------------
	# Board with: a pristine plan, a progressed plan, a demolition order (not
	# a target), and a built road.
	world.apply_road_plans([
		{"order_id": "pristine", "state": "open", "kind": "road_a", "path": [[300, 100], [400, 100]], "progress": {}},
		{"order_id": "progressed", "state": "open", "kind": "road_b", "path": [[300, 200], [400, 200]], "progress": {"stone": 7}},
		{"order_id": "demo1", "state": "open", "kind": "demo_x", "path": [[300, 300], [400, 300]], "progress": {}},
		{"order_id": "built", "state": "completed", "kind": "road_c", "path": [[300, 400], [400, 400]]},
	])
	if not world._completed_road_orders.has("built"):
		_fail("completed road orders should be tracked for demolition picking"); return
	# Demolition orders stake RED with the salvage label.
	if not world._road_plans.has("demo1"):
		_fail("demolition orders stake like plans"); return

	var demo_tool := DemolishTool.new()
	demo_tool.world_ref = world
	root.add_child(demo_tool)
	var cancels: Array = []
	var demolishes: Array = []
	demo_tool.cancel_requested.connect(func(id): cancels.append(id))
	demo_tool.demolish_requested.connect(func(id): demolishes.append(id))
	demo_tool.set_active(true)

	# Picking: nearest target; demo orders excluded; built roads included.
	if demo_tool.pick_target(Vector2(350, 302)).get("order_id", "") != "":
		_fail("a demolition order must not be a demolish target"); return
	if demo_tool.pick_target(Vector2(350, 402)).get("order_id", "") != "built":
		_fail("built roads are pickable from the board data"); return

	# One click targets, the second confirms; routing by progress/built.
	demo_tool.click(Vector2(350, 101))
	if not cancels.is_empty() or not demolishes.is_empty():
		_fail("first click must never fire"); return
	demo_tool.click(Vector2(352, 100))
	if cancels != ["pristine"] or not demolishes.is_empty():
		_fail("pristine plan confirm should route to road.cancel (got %s/%s)" % [str(cancels), str(demolishes)]); return
	demo_tool.click(Vector2(350, 201))
	demo_tool.click(Vector2(350, 199))
	if demolishes != ["progressed"]:
		_fail("progressed plan confirm should route to road.demolish (got %s)" % str(demolishes)); return
	demo_tool.click(Vector2(350, 401))
	demo_tool.click(Vector2(350, 402))
	if demolishes != ["progressed", "built"]:
		_fail("built road confirm should route to road.demolish (got %s)" % str(demolishes)); return
	# Clicking a DIFFERENT target between clicks re-targets, never fires.
	cancels.clear()
	demo_tool.click(Vector2(350, 101))
	demo_tool.click(Vector2(350, 201))
	demo_tool.click(Vector2(350, 101))
	if not cancels.is_empty():
		_fail("switching targets must reset the confirmation"); return

	# A demolished built road's despawn un-renders the ribbon.
	world.upsert_dirt_road("structure_road_c", {"path": [[300, 400], [400, 400]]})
	if not world._dirt_roads.has("structure_road_c"):
		_fail("built road should render"); return
	world.remove_dirt_road("structure_road_c")
	if world._dirt_roads.has("structure_road_c"):
		_fail("despawn should un-render the demolished road"); return

	print("SMOKE_OK: road snapping/cost math, laying, staking, move-mode, and demolish pick/route/confirm all behave")
	quit(0)
