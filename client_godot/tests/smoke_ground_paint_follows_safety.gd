## Headless smoke test: district safety is painted directly into the ground
## mesh's per-vertex colors (World._build_ground / _ground_color_at), not a
## separate overlay plane -- confirms the ground mesh actually carries a
## COLOR array, and that a safe-zone vertex and a wilds-zone vertex get
## visibly different colors.
## Run: Godot --headless --path client_godot -s res://tests/smoke_ground_paint_follows_safety.gd
extends SceneTree

func _initialize() -> void:
	var resolution := 8
	var world_size := 6400.0
	var stride := resolution + 1
	var heights := PackedFloat32Array()
	heights.resize(stride * stride)
	Protocol.apply_terrain_data(resolution, world_size, heights) # flat is fine -- only colors matter here

	var world = load("res://world/World.gd").new()
	root.add_child(world)
	world.apply_partition({"world": world_size, "zones": [
		{"x0": 0, "y0": 0, "x1": 3200, "y1": 6400, "district": "civic", "safety": "safe"},
		{"x0": 3200, "y0": 0, "x1": 6400, "y1": 6400, "district": "wilds", "safety": "wilds"},
	]})
	world.on_terrain_data() # both signals must arrive before the ground actually builds

	if world._ground == null:
		print("SMOKE_FAIL: ground was never built")
		quit(1)
		return

	var arrays: Array = world._ground.mesh.surface_get_arrays(0)
	var verts: PackedVector3Array = arrays[Mesh.ARRAY_VERTEX]
	var colors: PackedColorArray = arrays[Mesh.ARRAY_COLOR]
	if colors.size() != verts.size():
		print("SMOKE_FAIL: ground mesh has no per-vertex color array (size %d vs %d vertices)" % [colors.size(), verts.size()])
		quit(1)
		return

	# Find a vertex clearly in the safe half (x < 3200) and one clearly in
	# the wilds half (x > 3200), by scene-space x (WORLD_SCALE-independent
	# comparison since both sides use the same scale).
	var safe_color = null
	var wilds_color = null
	for i in range(verts.size()):
		if verts[i].x < 1000.0 * Protocol.WORLD_SCALE:
			safe_color = colors[i]
		elif verts[i].x > 5000.0 * Protocol.WORLD_SCALE:
			wilds_color = colors[i]
	if safe_color == null or wilds_color == null:
		print("SMOKE_FAIL: couldn't find both a safe-side and wilds-side vertex to compare")
		quit(1)
		return
	print("safe_color=%s wilds_color=%s" % [safe_color, wilds_color])
	if safe_color.is_equal_approx(wilds_color):
		print("SMOKE_FAIL: safe-zone and wilds-zone ground vertices have the same color")
		quit(1)
		return

	print("SMOKE_OK: ground mesh carries per-vertex safety-tint colors that actually differ safe vs wilds")
	quit(0)
