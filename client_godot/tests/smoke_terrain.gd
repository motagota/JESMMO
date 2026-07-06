## Headless smoke test: Protocol.terrain_height and World's displaced ground
## mesh. Purely cosmetic (the server has no concept of height at all), but
## worth pinning down: deterministic, within its stated amplitude, additive
## through w2v (so "height above ground" callers keep meaning that), and the
## ground mesh itself is actually non-flat rather than silently staying flat.
## Run: Godot --headless --path client_godot -s res://tests/smoke_terrain.gd
extends SceneTree

func _initialize() -> void:
	# Deterministic: the same point always returns the same height.
	var h1 := Protocol.terrain_height(3200.0, 3200.0)
	var h2 := Protocol.terrain_height(3200.0, 3200.0)
	if h1 != h2:
		print("SMOKE_FAIL: terrain_height is not deterministic (%f vs %f)" % [h1, h2])
		quit(1)
		return

	# Within the stated amplitude across a sweep of the world.
	var min_h := INF
	var max_h := -INF
	for i in range(0, 6400, 200):
		var h := Protocol.terrain_height(float(i), float(i) * 0.7)
		min_h = minf(min_h, h)
		max_h = maxf(max_h, h)
	print("terrain_height range sampled: [%f, %f]" % [min_h, max_h])
	if min_h < -4.01 or max_h > 4.01:
		print("SMOKE_FAIL: terrain_height exceeded its +/-4.0 amplitude")
		quit(1)
		return
	if max_h - min_h < 0.5:
		print("SMOKE_FAIL: terrain barely varies at all across the sweep (%f range) -- looks flat" % (max_h - min_h))
		quit(1)
		return

	# w2v's `y` parameter stays additive: raising it by a fixed amount raises
	# the scene position by exactly that amount, regardless of terrain height.
	var base: Vector3 = Protocol.w2v(4000.0, 1500.0, 0.0)
	var raised: Vector3 = Protocol.w2v(4000.0, 1500.0, 10.0)
	if absf((raised.y - base.y) - 10.0) > 0.001:
		print("SMOKE_FAIL: w2v's y offset isn't staying additive over terrain height")
		quit(1)
		return

	# The actual ground mesh World builds is non-flat.
	var world = load("res://world/World.gd").new()
	root.add_child(world)
	world.apply_partition({"world": 6400, "zones": []})
	var mesh: Mesh = world._ground.mesh
	var arrays := mesh.surface_get_arrays(0)
	var verts: PackedVector3Array = arrays[Mesh.ARRAY_VERTEX]
	var mesh_min := INF
	var mesh_max := -INF
	for v in verts:
		mesh_min = minf(mesh_min, v.y)
		mesh_max = maxf(mesh_max, v.y)
	print("ground mesh Y range: [%f, %f] across %d vertices" % [mesh_min, mesh_max, verts.size()])
	if mesh_max - mesh_min < 0.5:
		print("SMOKE_FAIL: the ground mesh is effectively flat")
		quit(1)
		return

	print("SMOKE_OK: terrain is deterministic, in-range, additive through w2v, and the ground mesh is non-flat")
	quit(0)
