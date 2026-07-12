## Headless smoke test: Protocol's grid-based terrain_height (#54) and the
## ground mesh World builds from it. Terrain is purely cosmetic (the server
## has no other concept of height), but the client-side grid lookup must
## exactly match the mesh's own triangle split -- that's the fix for the
## "objects/players fall through" bug, where a continuous noise function
## disagreed with the piecewise-flat mesh between grid points.
## Run: Godot --headless --path client_godot -s res://tests/smoke_terrain.gd
extends SceneTree

## A tiny 3x3-corner (resolution 2) grid over a 100-unit world, values chosen
## distinct and increasing so no two interpolation results can coincide by
## accident: row-major/y-major, heights[gy*3+gx].
##   gy=0: 0 1 2
##   gy=1: 3 4 5
##   gy=2: 6 7 8
static var _HEIGHTS := PackedFloat32Array([0, 1, 2, 3, 4, 5, 6, 7, 8])
const _RESOLUTION := 2
const _WORLD_SIZE := 100.0

func _initialize() -> void:
	# Before any terrain.data arrives, terrain_height is a safe flat 0.0 --
	# never crashes / never fabricates a height client-side.
	if Protocol.terrain_height(1234.0, 5678.0) != 0.0:
		print("SMOKE_FAIL: terrain_height should default to 0.0 before terrain.data arrives")
		quit(1)
		return

	Protocol.apply_terrain_data(_RESOLUTION, _WORLD_SIZE, _HEIGHTS)

	# Exact grid corners return their exact authored value -- no
	# interpolation error at a cell boundary.
	var corner_cases := [
		[0.0, 0.0, 0.0],     # h00 of cell (0,0)
		[50.0, 0.0, 1.0],    # shared corner between cells (0,0)/(1,0)
		[100.0, 0.0, 2.0],   # far edge
		[0.0, 100.0, 6.0],
		[100.0, 100.0, 8.0], # far corner
		[50.0, 50.0, 4.0],   # centre corner, shared by all 4 cells
	]
	for c in corner_cases:
		var got := Protocol.terrain_height(c[0], c[1])
		if absf(got - c[2]) > 0.001:
			print("SMOKE_FAIL: terrain_height(%f, %f) = %f, expected exact corner value %f" % [c[0], c[1], got, c[2]])
			quit(1)
			return

	# Mid-cell interpolation matches the hand-computed triangle-planar
	# formula for cell (0,0) (h00=0, h10=1, h01=3, h11=4), split along the
	# (0,0)-(1,1) diagonal:
	#   fy <= fx (below/right of the diagonal): h00 + (fx-fy)*(h10-h00) + fy*(h11-h00)
	#   fy >  fx (above/left of the diagonal):  h00 + (fy-fx)*(h01-h00) + fx*(h11-h00)
	var below_diag := Protocol.terrain_height(37.5, 12.5) # fx=0.75, fy=0.25
	var expect_below := 0.0 + (0.75 - 0.25) * (1.0 - 0.0) + 0.25 * (4.0 - 0.0)
	if absf(below_diag - expect_below) > 0.001:
		print("SMOKE_FAIL: below-diagonal interpolation = %f, expected %f" % [below_diag, expect_below])
		quit(1)
		return

	var above_diag := Protocol.terrain_height(12.5, 37.5) # fx=0.25, fy=0.75
	var expect_above := 0.0 + (0.75 - 0.25) * (3.0 - 0.0) + 0.25 * (4.0 - 0.0)
	if absf(above_diag - expect_above) > 0.001:
		print("SMOKE_FAIL: above-diagonal interpolation = %f, expected %f" % [above_diag, expect_above])
		quit(1)
		return

	# Deterministic: same point, same answer.
	if Protocol.terrain_height(37.5, 12.5) != below_diag:
		print("SMOKE_FAIL: terrain_height is not deterministic")
		quit(1)
		return

	# w2v's `y` parameter stays additive over terrain height.
	var base: Vector3 = Protocol.w2v(37.5, 12.5, 0.0)
	var raised: Vector3 = Protocol.w2v(37.5, 12.5, 10.0)
	if absf((raised.y - base.y) - 10.0) > 0.001:
		print("SMOKE_FAIL: w2v's y offset isn't staying additive over terrain height")
		quit(1)
		return

	# The ground mesh World builds uses this exact grid (same resolution,
	# same triangle split) -- its vertex heights must match terrain_height
	# queried at the same points, and its generated normals must point
	# upward (verifies the mesh's winding needs no CULL_DISABLED backface
	# band-aid, which was the likely cause of the "translucent" look).
	var world = load("res://world/World.gd").new()
	root.add_child(world)
	world.apply_partition({"world": _WORLD_SIZE, "zones": []})
	world.on_terrain_data()

	var mesh: Mesh = world._ground.mesh
	var arrays := mesh.surface_get_arrays(0)
	var verts: PackedVector3Array = arrays[Mesh.ARRAY_VERTEX]
	var normals: PackedVector3Array = arrays[Mesh.ARRAY_NORMAL]

	if verts.is_empty():
		print("SMOKE_FAIL: ground mesh has no vertices -- terrain.data race not resolved")
		quit(1)
		return

	for n in normals:
		if n.y <= 0.0:
			print("SMOKE_FAIL: a ground mesh normal points downward/sideways (%s) -- winding is backwards" % n)
			quit(1)
			return

	# Every mesh vertex's height must agree with terrain_height (scaled to
	# scene Y) at that same world point -- the actual guarantee against
	# "falling through": mesh and height-lookup can never disagree because
	# they share one grid.
	var mismatch := false
	for v in verts:
		var wx := v.x / Protocol.WORLD_SCALE
		var wy := v.z / Protocol.WORLD_SCALE
		var expected_y := Protocol.terrain_height(wx, wy) * Protocol.HEIGHT_SCALE
		if absf(v.y - expected_y) > 0.01:
			print("SMOKE_FAIL: mesh vertex at world (%f, %f) has y=%f but terrain_height says %f" % [wx, wy, v.y, expected_y])
			mismatch = true
			break
	if mismatch:
		quit(1)
		return

	print("SMOKE_OK: terrain_height matches authored corners/interpolation exactly, and the ground mesh agrees with it everywhere with upward-facing normals")
	quit(0)
