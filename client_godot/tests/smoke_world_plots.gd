## Headless smoke test: World.apply_plot_roster's per-plot marker shape --
## a free plot gets just the tile+border (no signpost at all), a taken one
## also gets a small signpost (post + name-plank + label) naming the owner --
## and the fill conforms to the terrain surface (per-vertex heights, not a
## flat plane buried in the first slope).
## Run: Godot --headless --path client_godot -s res://tests/smoke_world_plots.gd
extends SceneTree

func _initialize() -> void:
	var world = load("res://world/World.gd").new()
	root.add_child(world)
	world.apply_partition({"world": 6400, "zones": []})

	# A sloped backdrop under the plots (heights rise with x), so a flat
	# marker would provably disagree with the surface.
	var resolution := 64
	var heights := PackedFloat32Array()
	heights.resize((resolution + 1) * (resolution + 1))
	for gy in range(resolution + 1):
		for gx in range(resolution + 1):
			heights[gy * (resolution + 1) + gx] = gx * 2.0
	Protocol.apply_terrain_data(resolution, 6400.0, heights)

	var plots := [
		{"plot_id": "free1", "bounds": {"x": 4800, "y": 0, "w": 80, "h": 80}, "owner_name": null},
		{"plot_id": "taken1", "bounds": {"x": 4880, "y": 0, "w": 80, "h": 80}, "owner_name": "Alice"},
	]
	world.apply_plot_roster(plots, "not_mine")

	# Each marker is 1 fill + 4 border strips = 5 nodes; a taken plot adds a
	# signpost (post + plank + label) = 3 more, so 8 total.
	var count: int = world._plots_root.get_child_count()
	print("total _plots_root children for 1 free + 1 taken plot:", count)
	if count != 13:
		print("SMOKE_FAIL: expected 5 (free) + 8 (taken) = 13 children, got %d" % count)
		quit(1)
		return

	# Confirm exactly one Label3D exists (the taken plot's signpost label,
	# reading the owner's name) -- the free plot must not have added one.
	var labels := 0
	var label_texts := []
	for child in world._plots_root.get_children():
		if child is Label3D:
			labels += 1
			label_texts.append(child.text)
	print("Label3D count:", labels, "texts:", label_texts)
	if labels != 1 or label_texts[0] != "Alice":
		print("SMOKE_FAIL: expected exactly one Label3D reading 'Alice', got %s" % [label_texts])
		quit(1)
		return

	# The free plot's fill (its first child) must drape over the slope: every
	# vertex sits a small fixed lift above the terrain height at its own
	# (x, z), so the whole tile follows the ground instead of one flat plane
	# at the centre's height.
	var fill: MeshInstance3D = world._plots_root.get_child(0)
	var worst := 0.0
	var y_min := INF
	var y_max := -INF
	for v in fill.mesh.get_faces():
		var ground: float = Protocol.terrain_height(v.x / Protocol.WORLD_SCALE, v.z / Protocol.WORLD_SCALE) * Protocol.HEIGHT_SCALE
		worst = maxf(worst, absf(v.y - ground - (world._TILE_Y + 0.005)))
		y_min = minf(y_min, v.y)
		y_max = maxf(y_max, v.y)
	print("fill-vs-terrain worst error:", worst, " fill y span:", y_max - y_min)
	if worst > 0.01:
		print("SMOKE_FAIL: fill vertex strays %f from the terrain surface" % worst)
		quit(1)
		return
	# The authored slope rises 1.6m across the 80-unit plot; the fill must
	# span most of that (in world metres, so the check is independent of
	# Protocol.HEIGHT_SCALE's stylistic exaggeration).
	if (y_max - y_min) / Protocol.HEIGHT_SCALE < 1.2:
		print("SMOKE_FAIL: fill is flat (y span %fm world) over sloped terrain" % ((y_max - y_min) / Protocol.HEIGHT_SCALE))
		quit(1)
		return

	print("SMOKE_OK: free plots get no signpost, taken plots get exactly one naming the owner, fills follow the terrain")
	quit(0)
