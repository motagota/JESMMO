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

	# A placed home structure (carries `built_by`, matching
	# `home_structure_status_json`) should render offset by half its footprint
	# -- centred over the corner+size rect the server validated -- while its
	# `wpos` (used for proximity/overlap) stays the raw corner, matching the
	# server's own `near_home_structure` distance check.
	em.upsert("bed1", "zone_a", {"x": 5020.0, "y": 120.0, "type": "bed", "built_by": "alice"})
	var bed_rec: Dictionary = em._entities["bed1"]
	var bed_footprint: Vector2 = Protocol.STRUCTURE_FOOTPRINT["bed"]
	var expected_render := Protocol.w2v(5020.0 + bed_footprint.x * 0.5, 120.0 + bed_footprint.y * 0.5, em._height_for("bed"))
	assert(bed_rec["node"].position.distance_to(expected_render) < 0.001, \
		"a home structure should render offset to its footprint centre, got %s want %s" % [bed_rec["node"].position, expected_render])
	assert(bed_rec["wpos"] == Vector2(5020.0, 120.0), "wpos should stay the raw corner regardless of the render offset")

	# The authored point-fixture version (no `built_by`) should render at the
	# raw position, unshifted -- it has no footprint/corner concept at all.
	em.upsert("storehouse1", "zone_a", {"x": 5040.0, "y": 160.0, "type": "storage"})
	var store_rec: Dictionary = em._entities["storehouse1"]
	var expected_store_render := Protocol.w2v(5040.0, 160.0, em._height_for("storage"))
	assert(store_rec["node"].position.distance_to(expected_store_render) < 0.001, \
		"an authored point-fixture should render unshifted, got %s want %s" % [store_rec["node"].position, expected_store_render])

	print("SMOKE_OK build-place validity check matches expected rectangle math")
	quit(0)
