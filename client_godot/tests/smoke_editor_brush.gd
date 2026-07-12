## Headless smoke test: editor brush math + preview/patch reconciliation
## (terrain editing #78).
##
## Covers: BrushController.falloff_factor and brush_corners (pure static
## math), TerrainStreamer.apply_edit_preview mutating displayed heights —
## including seam-corner fanout into BOTH owning chunks — the authoritative
## terrain.delta_patch replacing preview values in place, and
## discard_edit_preview rolling un-acked preview back to base+server state.
## Run: Godot --headless --path client_godot -s res://tests/smoke_editor_brush.gd
extends SceneTree

func _fail(message: String) -> void:
	print("SMOKE_FAIL: %s" % message)
	quit(1)

func _initialize() -> void:
	# --- falloff math -------------------------------------------------------
	if absf(BrushController.falloff_factor(0.0, "smooth") - 1.0) > 0.001 \
			or absf(BrushController.falloff_factor(1.0, "smooth")) > 0.001 \
			or absf(BrushController.falloff_factor(0.5, "smooth") - 0.5) > 0.001:
		_fail("smooth falloff endpoints/midpoint wrong")
		return
	if absf(BrushController.falloff_factor(0.25, "linear") - 0.75) > 0.001:
		_fail("linear falloff wrong")
		return
	if absf(BrushController.falloff_factor(0.5, "sharp") - 0.25) > 0.001:
		_fail("sharp falloff wrong")
		return
	if absf(BrushController.falloff_factor(2.0, "smooth")) > 0.001:
		_fail("out-of-range t must clamp to 0 weight")
		return

	# --- brush corner selection ---------------------------------------------
	# cell 160m: a 200m brush at (800, 800) touches corners 4..6 per axis
	# minus the circle cut; corner (5,5) = (800,800) is dead center.
	var corners := BrushController.brush_corners(Vector2(800, 800), 200.0, 160.0, 40, 40)
	if not corners.has(Vector2i(5, 5)):
		_fail("brush must include its center corner")
		return
	if corners.has(Vector2i(4, 4)):
		# (640,640) is 226m from (800,800) — outside a 200m radius.
		_fail("diagonal corner outside the radius was included")
		return
	if not corners.has(Vector2i(4, 5)) or not corners.has(Vector2i(6, 5)):
		_fail("cardinal corners 160m away must be inside a 200m radius")
		return
	# Clamped at the world edge: a brush centred at the origin keeps only
	# non-negative corners.
	for c in BrushController.brush_corners(Vector2(0, 0), 200.0, 160.0, 40, 40):
		if c.x < 0 or c.y < 0:
			_fail("brush corners must clamp to the grid, got %s" % [c])
			return

	# --- streamer: preview, seam fanout, patch reconcile, discard -----------
	var world_size := 6400.0
	var resolution := 8
	var stride := resolution + 1
	var backdrop := PackedFloat32Array()
	backdrop.resize(stride * stride)
	backdrop.fill(10.0)
	Protocol.apply_terrain_data(resolution, world_size, backdrop)
	var tile_size := 4
	var side := tile_size + 1
	Protocol.apply_terrain_meta(tile_size, 160.0, 10, 10, 0.0, 100.0)
	var base := PackedFloat32Array()
	base.resize(side * side)
	base.fill(50.0)

	var streamer := TerrainStreamer.new()
	root.add_child(streamer)
	streamer.set_context([], world_size)
	streamer.on_player_position(3520.0, 3520.0) # tile (5,5): 3x3 ring resident
	for ty in range(4, 7):
		for tx in range(4, 7):
			streamer.on_tile_data(tx, ty, base)
			streamer.on_delta_data(tx, ty, false, PackedFloat32Array())

	# Preview a +1m lift on world corner (20, 21): cx=20 is the seam between
	# chunks (4,y) and (5,y) — the preview must land on BOTH sides.
	streamer.apply_edit_preview({Vector2i(20, 21): 1.0})
	var left := Protocol.terrain_height(20 * 160.0 - 0.5, 21 * 160.0)  # chunk (4,5)
	var right := Protocol.terrain_height(20 * 160.0 + 0.5, 21 * 160.0) # chunk (5,5)
	if absf(left - 51.0) > 0.05 or absf(right - 51.0) > 0.05:
		_fail("seam preview must lift both chunks (left=%f right=%f, want ~51)" % [left, right])
		return

	# An authoritative patch for chunk (5,5) carrying +2m at local corner
	# (0,1) (= world corner (20,21)) replaces the +1m preview there...
	var patch := PackedFloat32Array()
	patch.resize(side * side)
	patch[1 * side + 0] = 2.0
	streamer.on_delta_patch(5, 5, patch)
	var patched := Protocol.terrain_height(20 * 160.0 + 0.5, 21 * 160.0)
	if absf(patched - 52.0) > 0.05:
		_fail("patch must replace preview on its chunk (got %f, want ~52)" % patched)
		return
	# ...while chunk (4,5), unpatched, still shows its (now stale) preview.
	var unpatched := Protocol.terrain_height(20 * 160.0 - 0.5, 21 * 160.0)
	if absf(unpatched - 51.0) > 0.05:
		_fail("an unpatched chunk keeps its preview until told otherwise (got %f)" % unpatched)
		return

	# discard_edit_preview: everything back to base + authoritative offsets —
	# chunk (5,5) keeps its patch (+2m), chunk (4,5) reverts to plain base.
	streamer.discard_edit_preview()
	var kept := Protocol.terrain_height(20 * 160.0 + 0.5, 21 * 160.0)
	var reverted := Protocol.terrain_height(20 * 160.0 - 0.5, 21 * 160.0)
	if absf(kept - 52.0) > 0.05:
		_fail("discard must keep authoritative patches (got %f, want ~52)" % kept)
		return
	if absf(reverted - 50.0) > 0.05:
		_fail("discard must revert un-acked preview to base (got %f, want ~50)" % reverted)
		return

	# A patch for a non-resident chunk is stored and used on later stream-in
	# (and suppresses the redundant delta re-request).
	var far_patch := PackedFloat32Array()
	far_patch.resize(side * side)
	far_patch.fill(3.0)
	streamer.on_delta_patch(0, 0, far_patch)
	streamer.on_player_position(200.0, 200.0) # move to tile (0,0)'s ring
	streamer.on_tile_data(0, 0, base)
	var streamed_in := Protocol.terrain_height(100.0, 100.0)
	if absf(streamed_in - 53.0) > 0.05:
		_fail("a stored patch must composite on later stream-in (got %f, want ~53)" % streamed_in)
		return

	print("SMOKE_OK: falloff, corner selection, seam preview fanout, patch reconcile, discard, and stored-patch stream-in all behave")
	quit(0)
