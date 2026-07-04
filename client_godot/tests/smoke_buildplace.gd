## Headless smoke test: instantiate BuildPlace + EntityManager in a real scene
## tree and drive the validity check directly (the part most likely to hide a
## rectangle-math bug), matching the server's own bounds/overlap semantics in
## `apply_build_place` (top-left-corner rects).
## Run: Godot --headless --path client_godot -s res://tests/smoke_buildplace.gd
extends SceneTree

func _initialize() -> void:
	var bp = load("res://ui/BuildPlace.gd").new()
	var em = load("res://world/EntityManager.gd").new()
	var cam := Camera3D.new()
	bp.ready.connect(_drive.bind(bp, em, cam))
	root.add_child(em)
	root.add_child(cam)
	root.add_child(bp)

func _drive(bp, em, cam: Camera3D) -> void:
	# Straight-down raycast: camera high above a known world point, looking
	# along -Y, screen point at the viewport centre (the ray through the
	# camera's own forward direction) -- the ground hit should land back at
	# that same point (Y=0, matching Protocol.w2v's ground height). Passed
	# explicitly rather than via Input.warp_mouse, which doesn't reliably
	# take effect synchronously in headless mode.
	var world_pt := Vector2(5040.0, 140.0)
	var scene_pt := Protocol.w2v(world_pt.x, world_pt.y, 0.0)
	cam.position = Vector3(scene_pt.x, 50.0, scene_pt.z)
	cam.rotation_degrees = Vector3(-90, 0, 0)
	cam.current = true
	var vp_center := root.get_visible_rect().size * 0.5
	bp.camera = cam
	var hit: Vector2 = bp._raycast_ground(vp_center)
	assert(hit.distance_to(world_pt) < 1.0, "straight-down raycast should hit back at %s, got %s" % [world_pt, hit])

	bp.entities = em
	bp.plot_bounds = {"x": 5000.0, "y": 100.0, "w": 80.0, "h": 80.0}

	# A bed (20x20) fits comfortably inside the plot with no neighbours.
	assert(bp._is_valid_placement(Vector2(5010, 110), Vector2(20, 20)), \
		"a footprint fully inside an empty plot should be valid")

	# Escaping the plot on every edge should be invalid.
	assert(not bp._is_valid_placement(Vector2(4990, 110), Vector2(20, 20)), "off the left edge")
	assert(not bp._is_valid_placement(Vector2(5010, 90), Vector2(20, 20)), "off the top edge")
	assert(not bp._is_valid_placement(Vector2(5065, 110), Vector2(20, 20)), "off the right edge (5065+20=5085 > 5080)")
	assert(not bp._is_valid_placement(Vector2(5010, 165), Vector2(20, 20)), "off the bottom edge (165+20=185 > 180)")

	# No plot known yet (e.g. before plot.assigned arrives) -> always invalid.
	bp.plot_bounds = {}
	assert(not bp._is_valid_placement(Vector2(5010, 110), Vector2(20, 20)), "no known plot -> invalid")
	bp.plot_bounds = {"x": 5000.0, "y": 100.0, "w": 80.0, "h": 80.0}

	# Seed an existing structure (a storage chest at 5030,110, 16x16) and
	# confirm overlap is caught, while a non-overlapping spot in the same
	# plot still reads as valid.
	em.upsert("s1", "zone_a", {"x": 5030, "y": 110, "type": "storage"})
	assert(em.overlaps_home_structure(Vector2(5025, 105), Vector2(20, 20)), \
		"a footprint overlapping the existing chest should be flagged")
	assert(not em.overlaps_home_structure(Vector2(5005, 105), Vector2(15, 15)), \
		"a footprint clear of the existing chest should not be flagged")
	assert(not bp._is_valid_placement(Vector2(5030, 110), Vector2(20, 20)), \
		"in-bounds but overlapping the chest should be invalid via the full check")

	print("SMOKE_OK build-place validity check matches expected rectangle math")
	quit(0)
