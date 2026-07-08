## Headless smoke test: a district tile (`_add_district_tile`) must be a
## terrain-following mesh, not a single flat plane sampled at its center --
## real DEM terrain (issue #69) carries far more relief across a district's
## footprint than the old synthetic placeholder did, so a flat plane visibly
## floats above/through the real ground almost everywhere in the tile.
## Run: Godot --headless --path client_godot -s res://tests/smoke_district_tile_follows_terrain.gd
extends SceneTree

func _initialize() -> void:
	# A steep synthetic ramp (0m at x=0 up to 1000m at x=6400) so a flat
	# single-sample plane and a terrain-following mesh would visibly disagree.
	var resolution := 8
	var world_size := 6400.0
	var stride := resolution + 1
	var heights := PackedFloat32Array()
	heights.resize(stride * stride)
	for gy in range(stride):
		for gx in range(stride):
			heights[gy * stride + gx] = float(gx) / float(resolution) * 1000.0
	Protocol.apply_terrain_data(resolution, world_size, heights)

	var world = load("res://world/World.gd").new()
	root.add_child(world)
	world.apply_partition({"world": world_size, "zones": [
		{"x0": 0, "y0": 0, "x1": 6400, "y1": 6400, "district": "wilds", "safety": "wilds"},
	]})

	# One tile mesh + one district-name label per zone.
	var tile: MeshInstance3D = null
	for c in world._tiles_root.get_children():
		if c is MeshInstance3D:
			tile = c
	if tile == null:
		print("SMOKE_FAIL: no MeshInstance3D district tile found among %d children" % world._tiles_root.get_child_count())
		quit(1)
		return
	var mesh: Mesh = tile.mesh
	if mesh is PlaneMesh:
		print("SMOKE_FAIL: district tile is still a single flat PlaneMesh")
		quit(1)
		return

	var arrays := mesh.surface_get_arrays(0)
	var verts: PackedVector3Array = arrays[Mesh.ARRAY_VERTEX]
	if verts.size() < 8:
		print("SMOKE_FAIL: expected a subdivided grid mesh, got only %d vertices" % verts.size())
		quit(1)
		return

	# Vertices at the low-x edge and the high-x edge of the ramp must differ
	# in height by close to the ramp's full 1000m range -- a flat
	# single-sample plane would have every vertex at the same Y.
	var min_y := INF
	var max_y := -INF
	for v in verts:
		min_y = minf(min_y, v.y)
		max_y = maxf(max_y, v.y)
	print("min_y=%f max_y=%f vertex_count=%d" % [min_y, max_y, verts.size()])
	if max_y - min_y < 500.0:
		print("SMOKE_FAIL: district tile doesn't follow the terrain ramp (range only %f)" % (max_y - min_y))
		quit(1)
		return

	print("SMOKE_OK: district tile is a terrain-following mesh spanning the real height range")
	quit(0)
