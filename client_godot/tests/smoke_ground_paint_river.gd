## Headless smoke test: ground cells below the river-height threshold paint
## river silt-brown, overriding whatever safety tint that point would
## otherwise get (World._ground_color_at / _RIVER_HEIGHT_THRESHOLD_M).
## Run: Godot --headless --path client_godot -s res://tests/smoke_ground_paint_river.gd
extends SceneTree

func _initialize() -> void:
	# A simple west-high/east-low ramp: west half is well above the river
	# threshold, east half dips below it -- so the river-colored strip should
	# land on the east side regardless of the (uniformly "safe") zone tint.
	var resolution := 8
	var world_size := 6400.0
	var stride := resolution + 1
	var heights := PackedFloat32Array()
	heights.resize(stride * stride)
	for gy in range(stride):
		for gx in range(stride):
			heights[gy * stride + gx] = 50.0 if gx < stride / 2 else 0.0
	Protocol.apply_terrain_data(resolution, world_size, heights)

	var world = load("res://world/World.gd").new()
	root.add_child(world)
	world.apply_partition({"world": world_size, "zones": [
		{"x0": 0, "y0": 0, "x1": 6400, "y1": 6400, "district": "civic", "safety": "safe"},
	]})
	world.on_terrain_data()

	var high_color: Color = world._ground_color_at(500.0, 500.0)   # west, height 50m
	var low_color: Color = world._ground_color_at(5500.0, 500.0)   # east, height 0m
	print("high_color=%s low_color=%s river_const=%s" % [high_color, low_color, World._RIVER_COLOR])

	if high_color.is_equal_approx(low_color):
		print("SMOKE_FAIL: high ground and low (river) ground painted the same color")
		quit(1)
		return
	if not low_color.is_equal_approx(World._RIVER_COLOR):
		print("SMOKE_FAIL: low-lying ground didn't paint the river color (got %s)" % low_color)
		quit(1)
		return

	print("SMOKE_OK: low-lying ground paints the river color, distinct from high ground")
	quit(0)
