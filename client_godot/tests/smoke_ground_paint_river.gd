## Headless smoke test: ground at or below GroundPaint._RIVER_FULL_M paints
## fully river silt-brown, overriding whatever safety tint that point would
## otherwise get; ground at or above GroundPaint._RIVER_FADE_M paints no
## river tint at all (GroundPaint.ground_color_at / safety_color_at).
## Run: Godot --headless --path client_godot -s res://tests/smoke_ground_paint_river.gd
extends SceneTree

func _initialize() -> void:
	# A simple west-high/east-low ramp: west half sits well above
	# _RIVER_FADE_M, east half sits well below _RIVER_FULL_M -- so the
	# river-colored strip should land fully on the east side regardless of
	# the (uniformly "safe") zone tint.
	var resolution := 8
	var world_size := 6400.0
	var stride := resolution + 1
	var heights := PackedFloat32Array()
	heights.resize(stride * stride)
	for gy in range(stride):
		for gx in range(stride):
			heights[gy * stride + gx] = 50.0 if gx < stride / 2 else -10.0
	Protocol.apply_terrain_data(resolution, world_size, heights)

	var world = load("res://world/World.gd").new()
	root.add_child(world)
	world.apply_partition({"world": world_size, "zones": [
		{"x0": 0, "y0": 0, "x1": 6400, "y1": 6400, "district": "civic", "safety": "safe"},
	]})
	world.on_terrain_data()

	var high_color: Color = GroundPaint.ground_color_at(world._zones, world_size, 500.0, 500.0)   # west, height 50m
	var low_color: Color = GroundPaint.ground_color_at(world._zones, world_size, 5500.0, 500.0)   # east, height -10m
	print("high_color=%s low_color=%s river_const=%s" % [high_color, low_color, GroundPaint._RIVER_COLOR])

	if high_color.is_equal_approx(low_color):
		print("SMOKE_FAIL: high ground and low (river) ground painted the same color")
		quit(1)
		return
	if not low_color.is_equal_approx(GroundPaint._RIVER_COLOR):
		print("SMOKE_FAIL: low-lying ground didn't paint the river color (got %s)" % low_color)
		quit(1)
		return

	# A point in the fade band (between _RIVER_FULL_M and _RIVER_FADE_M) must
	# be a genuine blend -- neither the pure safety color nor pure river
	# brown -- so a single low wire-grid corner doesn't wash a whole coarse
	# cell into flat brown; it should taper.
	var mid_h := (GroundPaint._RIVER_FULL_M + GroundPaint._RIVER_FADE_M) * 0.5
	Protocol.apply_terrain_data(1, world_size, PackedFloat32Array([mid_h, mid_h, mid_h, mid_h]))
	var mid_color: Color = GroundPaint.ground_color_at(world._zones, world_size, 100.0, 100.0)
	print("mid_color=%s (at height %s)" % [mid_color, mid_h])
	if mid_color.is_equal_approx(GroundPaint._RIVER_COLOR) or mid_color.is_equal_approx(high_color):
		print("SMOKE_FAIL: fade-band height didn't blend (got %s)" % mid_color)
		quit(1)
		return

	print("SMOKE_OK: river color fully applies when deep, fades out by _RIVER_FADE_M, and blends in between")
	quit(0)
