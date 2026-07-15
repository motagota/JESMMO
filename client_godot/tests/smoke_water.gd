## Headless smoke test: the water plane (`World._build_water`) exists once
## (and only once) both `partition` and `terrain.data` have arrived, spans
## the whole world, and sits at sea level plus the z-fight lift — above the
## streamed-tile bias (so the flat 0m bay fill can't poke through it) but
## still inside GroundPaint's silt-brown band (so the waterline lands on
## wet-mud ground, not clean safety-tinted grass).
## Run: Godot --headless --path client_godot -s res://tests/smoke_water.gd
extends SceneTree

func _initialize() -> void:
	var world_size := 6400.0
	var world = load("res://world/World.gd").new()
	root.add_child(world)

	# Same both-flags gating as the ground: partition alone must not build
	# water (it would bake a plane before world_size/terrain are trustworthy).
	world.apply_partition({"world": world_size, "zones": [
		{"x0": 0, "y0": 0, "x1": 6400, "y1": 6400, "district": "civic", "safety": "safe"},
	]})
	if world._water != null:
		print("SMOKE_FAIL: water built before terrain.data arrived")
		quit(1)
		return

	# Flat 0m terrain — the bay/river NoData fill convention.
	Protocol.apply_terrain_data(1, world_size, PackedFloat32Array([0.0, 0.0, 0.0, 0.0]))
	world.on_terrain_data()

	if world._water == null:
		print("SMOKE_FAIL: water plane missing after partition + terrain.data")
		quit(1)
		return

	var plane: PlaneMesh = world._water.mesh
	var expected_extent: float = world_size * Protocol.WORLD_SCALE
	print("water size=%s position=%s" % [plane.size, world._water.position])
	if not is_equal_approx(plane.size.x, expected_extent) or not is_equal_approx(plane.size.y, expected_extent):
		print("SMOKE_FAIL: water plane doesn't span the world (got %s, want %s)" % [plane.size, expected_extent])
		quit(1)
		return

	var y: float = world._water.position.y
	var sea_scene: float = World._WATER_LEVEL_M * Protocol.HEIGHT_SCALE
	var stream_bias := 0.3  # TerrainStreamer._STREAM_Y_BIAS — streamed 0m ground renders here
	var brown_band_top: float = GroundPaint._RIVER_FADE_M * Protocol.HEIGHT_SCALE
	if y <= sea_scene + stream_bias:
		print("SMOKE_FAIL: water at y=%s would z-fight/underlap streamed 0m ground (bias %s)" % [y, stream_bias])
		quit(1)
		return
	if y >= brown_band_top:
		print("SMOKE_FAIL: water at y=%s rises past GroundPaint's silt band (top %s) — waterline would cross untinted ground" % [y, brown_band_top])
		quit(1)
		return

	# Centered on the map, translucent shader material attached.
	var mid: float = world_size * 0.5 * Protocol.WORLD_SCALE
	if not is_equal_approx(world._water.position.x, mid) or not is_equal_approx(world._water.position.z, mid):
		print("SMOKE_FAIL: water plane not centered (got %s)" % world._water.position)
		quit(1)
		return
	if not (world._water.material_override is ShaderMaterial) \
			or world._water.material_override.shader == null:
		print("SMOKE_FAIL: water has no shader material")
		quit(1)
		return

	print("SMOKE_OK: water plane spans the world at sea level, above streamed-ground bias, inside the silt band")
	quit(0)
