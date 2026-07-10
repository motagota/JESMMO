## Headless conformance test (terrain pipeline epic #56, issue #64): the
## client's own `Protocol.terrain_height` must reconstruct a canonical baked
## height function correctly, at the same kind of sample points
## `terrain-common`'s own Rust golden fixture covers
## (`terrain-common/src/sampler.rs`).
##
## The fixture property mirrored here is the same one that fixture chose:
## height = world x, constant in y (a pure ramp) — deliberately linear, so
## its interpolated value is identical under *any* interpolation scheme.
## That's what makes this a valid cross-language conformance point between
## the server's canonical bilinear `terrain_common::Terrain::sample_height`
## and this client's own triangle-planar `Protocol.terrain_height`, without
## requiring the two schemes to agree in general (they don't, and don't need
## to — see `docs/protocol.md`'s `terrain.*` section on why). The wire grid
## here is a single flat square `(resolution+1)^2` array (what
## `terrain.data` actually sends), unlike `terrain-common`'s own tiled,
## duplicated-edge format, so the numbers are analogous, not byte-identical,
## to the Rust test.
##
## Run: Godot --headless --path client_godot -s res://tests/smoke_terrain_bake_conformance.gd
extends SceneTree

const _RESOLUTION := 8
const _WORLD_SIZE := 80.0

func _initialize() -> void:
	var step := _WORLD_SIZE / _RESOLUTION
	var heights := PackedFloat32Array()
	for gy in range(_RESOLUTION + 1):
		for gx in range(_RESOLUTION + 1):
			heights.append(gx * step) # height = world x, constant in y
	Protocol.apply_terrain_data(_RESOLUTION, _WORLD_SIZE, heights)

	# (x, y, expected_height) -- corners, an interior point, and the far
	# corner of the whole grid, mirroring the shape of terrain-common's own
	# golden-sample table.
	var cases := [
		[0.0, 0.0, 0.0],
		[10.0, 0.0, 10.0],
		[5.0, 5.0, 5.0],
		[35.0, 20.0, 35.0],
		[40.0, 0.0, 40.0],
		[40.0, 40.0, 40.0],
		[45.0, 15.0, 45.0],
		[80.0, 80.0, 80.0],
	]
	for c in cases:
		var got := Protocol.terrain_height(c[0], c[1])
		if absf(got - c[2]) > 0.01:
			print("SMOKE_FAIL: terrain_height(%f, %f) = %f, expected %f" % [c[0], c[1], got, c[2]])
			quit(1)
			return

	print("SMOKE_OK: client terrain_height reconstructs a canonical linear-ramp height function exactly -- the same conformance property terrain-common's golden fixture checks server-side")
	quit(0)
