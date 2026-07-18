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
	if world._road_plans["r1"]["nodes"].size() != 2:
		_fail("a two-run plan stakes two ribbons"); return
	world.remove_road_plan("r1")
	if not world._road_plans.is_empty():
		_fail("completion should drop the stakes"); return
	# Replace-not-merge: a fresh push with no roads clears everything.
	world.apply_road_plans(orders)
	world.apply_road_plans([])
	if not world._road_plans.is_empty():
		_fail("a board push without the order should un-stake it"); return

	print("SMOKE_OK: road snapping/cost math, the anchor/corner/commit machine, and staked-plan rendering all behave")
	quit(0)
