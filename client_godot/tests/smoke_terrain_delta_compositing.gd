## Headless smoke test: client-side height-delta compositing (terrain
## editing #72, client issue #76).
##
## Covers: Protocol.decode_height_delta against a hand-built binary payload
## (terrain-common's SparseHeightDelta::encode format, magic "TRHD"),
## streamer compositing in BOTH arrival orders (delta-before-tile composites
## at build; tile-before-delta composites + rebuilds the mesh), has_delta:
## false answering without changing heights, duplicate-answer idempotence,
## and unload dropping delta state so a re-entered chunk re-requests fresh.
## Run: Godot --headless --path client_godot -s res://tests/smoke_terrain_delta_compositing.gd
extends SceneTree

func _fail(message: String) -> void:
	print("SMOKE_FAIL: %s" % message)
	quit(1)

## SparseHeightDelta::encode(1) bytes for a 1-block grid (side <= 16):
## block 0 present, `cm_offsets` poked in at corner-grid positions.
func _delta_payload(side: int, cm_offsets: Dictionary) -> PackedByteArray:
	var bytes := PackedByteArray()
	bytes.resize(8 + 8 + 256 * 2) # header + 1 bitmap word + one block
	bytes[0] = 0x54; bytes[1] = 0x52; bytes[2] = 0x48; bytes[3] = 0x44 # "TRHD"
	bytes.encode_u16(4, 1) # format_version
	bytes.encode_u16(6, 0) # reserved
	bytes.encode_u64(8, 1) # bitmap: block 0 present
	for pos in cm_offsets:
		if pos.x >= side or pos.y >= side:
			_fail("test bug: offset outside the corner grid")
		bytes.encode_s16(16 + (pos.y * 16 + pos.x) * 2, cm_offsets[pos])
	return bytes

func _initialize() -> void:
	var world_size := 6400.0

	# Coarse backdrop: flat 10m (so fine-tile queries are easy to spot).
	var resolution := 8
	var stride := resolution + 1
	var backdrop := PackedFloat32Array()
	backdrop.resize(stride * stride)
	backdrop.fill(10.0)
	Protocol.apply_terrain_data(resolution, world_size, backdrop)

	# Streamable grid: 10x10 tiles of 4 cells x 160m = 640m per tile
	# (10x10 so a far move genuinely evicts chunks from a 3x3 ring).
	var tile_size := 4
	var side := tile_size + 1
	Protocol.apply_terrain_meta(tile_size, 160.0, 10, 10, 0.0, 100.0)

	# Flat-50m base tile heights (what the "server" streams for any chunk).
	var base := PackedFloat32Array()
	base.resize(side * side)
	base.fill(50.0)

	# --- decode_height_delta against a hand-built payload ------------------
	# +2m at corner (1,1), -1.5m at corner (3,2).
	var payload := _delta_payload(side, {Vector2i(1, 1): 200, Vector2i(3, 2): -150})
	var offsets := Protocol.decode_height_delta(payload)
	if offsets.size() != side * side:
		_fail("decode_height_delta size %d, want %d" % [offsets.size(), side * side])
		return
	if absf(offsets[1 * side + 1] - 2.0) > 0.001 or absf(offsets[2 * side + 3] + 1.5) > 0.001:
		_fail("decoded offsets wrong: %s" % [offsets])
		return
	if absf(offsets[0]) > 0.001:
		_fail("untouched corner should offset 0, got %f" % offsets[0])
		return
	var garbage := payload.duplicate()
	garbage[0] = 0x58 # break the magic
	if not Protocol.decode_height_delta(garbage).is_empty():
		_fail("decode_height_delta accepted a bad magic")
		return
	var short := payload.duplicate()
	short.resize(payload.size() - 2)
	if not Protocol.decode_height_delta(short).is_empty():
		_fail("decode_height_delta accepted a truncated body")
		return

	# --- streamer: both arrival orders -------------------------------------
	var streamer := TerrainStreamer.new()
	root.add_child(streamer)
	streamer.set_context([], world_size)
	var tile_reqs: Array = []
	var delta_reqs: Array = []
	streamer.tile_requested.connect(func(tx, ty): tile_reqs.append(Vector2i(tx, ty)))
	streamer.delta_requested.connect(func(tx, ty): delta_reqs.append(Vector2i(tx, ty)))

	streamer.on_player_position(3520.0, 3520.0) # tile (5,5) -> full 3x3 ring
	if tile_reqs.size() != 9 or delta_reqs.size() != 9:
		_fail("expected 9 tile + 9 delta requests, got %d + %d" % [tile_reqs.size(), delta_reqs.size()])
		return
	if not delta_reqs.has(Vector2i(5, 5)):
		_fail("delta request ring missing the center chunk")
		return

	# Order 1 — delta first, then tile: heights must arrive composited.
	streamer.on_delta_data(5, 5, true, offsets)
	streamer.on_tile_data(5, 5, base)
	var h_edit := Protocol.terrain_height(5 * 640.0 + 1 * 160.0, 5 * 640.0 + 1 * 160.0) # corner (1,1)
	if absf(h_edit - 52.0) > 0.01:
		_fail("delta-first: corner (1,1) should read 52m (50 base + 2 delta), got %f" % h_edit)
		return
	var h_plain := Protocol.terrain_height(5 * 640.0, 5 * 640.0) # corner (0,0), unedited
	if absf(h_plain - 50.0) > 0.01:
		_fail("delta-first: unedited corner should stay 50m, got %f" % h_plain)
		return

	# Order 2 — tile first (mesh builds from base), then delta: composited +
	# mesh rebuilt (still exactly one mesh per loaded chunk).
	streamer.on_tile_data(4, 5, base)
	var before := Protocol.terrain_height(4 * 640.0 + 1 * 160.0, 5 * 640.0 + 1 * 160.0)
	if absf(before - 50.0) > 0.01:
		_fail("tile-first: pre-delta height should be base 50m, got %f" % before)
		return
	var meshes_before := streamer.get_child_count()
	streamer.on_delta_data(4, 5, true, offsets)
	var after := Protocol.terrain_height(4 * 640.0 + 1 * 160.0, 5 * 640.0 + 1 * 160.0)
	if absf(after - 52.0) > 0.01:
		_fail("tile-first: post-delta height should be 52m, got %f" % after)
		return
	# queue_free is deferred, so count meshes that aren't queued for deletion.
	var live := 0
	for child in streamer.get_children():
		if not child.is_queued_for_deletion():
			live += 1
	if live != meshes_before:
		_fail("tile-first rebuild should keep one live mesh per chunk (want %d, got %d)" % [meshes_before, live])
		return

	# Duplicate delta answer must not double-apply.
	streamer.on_delta_data(4, 5, true, offsets)
	if absf(Protocol.terrain_height(4 * 640.0 + 1 * 160.0, 5 * 640.0 + 1 * 160.0) - 52.0) > 0.01:
		_fail("duplicate delta answer double-applied the offsets")
		return

	# has_delta=false: answers without touching heights.
	streamer.on_tile_data(6, 5, base)
	streamer.on_delta_data(6, 5, false, PackedFloat32Array())
	if absf(Protocol.terrain_height(6 * 640.0 + 160.0, 5 * 640.0 + 160.0) - 50.0) > 0.01:
		_fail("has_delta=false changed a chunk's heights")
		return

	# --- unload drops delta state; re-entry re-requests both ---------------
	streamer.on_player_position(200.0, 200.0) # tile (0,0), far from (5,5)
	if Protocol.has_terrain_tile(5, 5):
		_fail("moving away should unload chunk (5,5)")
		return
	tile_reqs.clear()
	delta_reqs.clear()
	streamer.on_player_position(3520.0, 3520.0) # back to (5,5)
	if not tile_reqs.has(Vector2i(5, 5)) or not delta_reqs.has(Vector2i(5, 5)):
		_fail("re-entering a chunk should re-request tile AND delta (got %s / %s)" % [tile_reqs, delta_reqs])
		return

	print("SMOKE_OK: delta decode, both composite orders, rebuild, idempotence, and unload/re-request all behave")
	quit(0)
