## Headless smoke test: terrain streaming (native-resolution tiles near the
## player, coarse backdrop everywhere else).
##
## Covers: Protocol.decode_height_tile against a hand-built binary payload
## (terrain-common's HeightTile::encode format), terrain_height preferring a
## loaded fine tile and falling back to the backdrop outside it,
## TerrainStreamer.wanted_tiles_for's clamped ring, and the streamer's full
## request -> arrive -> build -> move-away -> free lifecycle.
## Run: Godot --headless --path client_godot -s res://tests/smoke_terrain_streaming.gd
extends SceneTree

func _fail(message: String) -> void:
	print("SMOKE_FAIL: %s" % message)
	quit(1)

func _initialize() -> void:
	var world_size := 6400.0

	# Coarse backdrop: flat 10m everywhere (resolution 8).
	var resolution := 8
	var stride := resolution + 1
	var backdrop := PackedFloat32Array()
	backdrop.resize(stride * stride)
	backdrop.fill(10.0)
	Protocol.apply_terrain_data(resolution, world_size, backdrop)

	# Streamable grid: 2x2 tiles of 4 cells x 800m = 3200m per tile.
	var tile_size := 4
	var side := tile_size + 1
	Protocol.apply_terrain_meta(tile_size, 800.0, 2, 2, 0.0, 100.0)

	# --- decode_height_tile against a hand-built binary payload -----------
	# Tile (0,0), every sample encoding exactly 50m (u16 32768 ~= 0.5 * 100).
	var bytes := PackedByteArray()
	bytes.resize(16 + side * side * 2)
	bytes[0] = 0x54; bytes[1] = 0x52; bytes[2] = 0x48; bytes[3] = 0x54 # "TRHT"
	bytes.encode_u16(4, 1)  # format_version
	bytes.encode_u16(6, 0)  # reserved
	bytes.encode_s32(8, 0)  # tile_x
	bytes.encode_s32(12, 0) # tile_y
	for i in range(side * side):
		bytes.encode_u16(16 + i * 2, 32768)
	var decoded := Protocol.decode_height_tile(bytes)
	if decoded.is_empty():
		_fail("decode_height_tile rejected a well-formed payload")
		return
	if decoded["tx"] != 0 or decoded["ty"] != 0:
		_fail("decoded tile coords wrong: %s" % [decoded])
		return
	var h0: float = decoded["heights"][0]
	if absf(h0 - 50.0) > 0.01:
		_fail("decoded height %f, expected ~50.0" % h0)
		return
	var garbage := bytes.duplicate()
	garbage[0] = 0x58 # break the magic
	if not Protocol.decode_height_tile(garbage).is_empty():
		_fail("decode_height_tile accepted a bad magic")
		return

	# --- terrain_height: fine tile preferred, backdrop outside ------------
	Protocol.apply_terrain_tile(0, 0, decoded["heights"])
	var inside := Protocol.terrain_height(1000.0, 1000.0)   # tile (0,0)
	var outside := Protocol.terrain_height(5000.0, 5000.0)  # tile (1,1), not loaded
	print("inside=%f (want ~50, fine) outside=%f (want ~10, backdrop)" % [inside, outside])
	if absf(inside - 50.0) > 0.01:
		_fail("point inside a loaded tile didn't use the fine data (got %f)" % inside)
		return
	if absf(outside - 10.0) > 0.01:
		_fail("point outside loaded tiles didn't fall back to the backdrop (got %f)" % outside)
		return
	Protocol.remove_terrain_tile(0, 0)
	if absf(Protocol.terrain_height(1000.0, 1000.0) - 10.0) > 0.01:
		_fail("removing a tile didn't restore the backdrop fallback")
		return

	# --- wanted_tiles_for: clamped ring ------------------------------------
	var corner := TerrainStreamer.wanted_tiles_for(Vector2i(0, 0), 1, 2, 2)
	if corner.size() != 4: # 3x3 clamped to the 2x2 grid
		_fail("corner ring should clamp to 4 tiles, got %d" % corner.size())
		return
	var mid := TerrainStreamer.wanted_tiles_for(Vector2i(5, 5), 1, 10, 10)
	if mid.size() != 9 or not mid.has(Vector2i(4, 4)) or not mid.has(Vector2i(6, 6)):
		_fail("interior ring should be a full 3x3 window")
		return

	# --- full streamer lifecycle -------------------------------------------
	var streamer := TerrainStreamer.new()
	root.add_child(streamer)
	streamer.set_context([], world_size)
	var requested: Array = []
	streamer.tile_requested.connect(func(tx, ty): requested.append(Vector2i(tx, ty)))

	streamer.on_player_position(1000.0, 1000.0) # tile (0,0) -> wants the clamped 2x2
	if requested.size() != 4:
		_fail("expected 4 tile requests from the corner position, got %d (%s)" % [requested.size(), requested])
		return

	streamer.on_tile_data(0, 0, decoded["heights"])
	if not Protocol.has_terrain_tile(0, 0):
		_fail("an in-ring tile arrival wasn't applied to the registry")
		return
	if streamer.get_child_count() != 1:
		_fail("an in-ring tile arrival didn't build exactly one mesh (got %d)" % streamer.get_child_count())
		return

	# Move far away (tile (1,1) corner) -- (0,0) leaves the ring... except a
	# 2x2 grid is fully inside any 3x3 ring, so use the wider grid instead:
	Protocol.apply_terrain_meta(tile_size, 800.0, 10, 10, 0.0, 100.0)
	streamer.on_tile_data(0, 0, decoded["heights"]) # re-apply under the 10x10 grid
	streamer.on_player_position(7600.0, 7600.0) # tile (9,9) -- far from (0,0)
	if Protocol.has_terrain_tile(0, 0):
		_fail("a tile far outside the ring wasn't freed from the registry")
		return

	# A stale arrival (tile the ring no longer wants) must be dropped.
	streamer.on_tile_data(0, 0, decoded["heights"])
	if Protocol.has_terrain_tile(0, 0):
		_fail("a stale (out-of-ring) tile arrival was wrongly applied")
		return

	print("SMOKE_OK: tile decode, fine-over-backdrop height, clamped ring, and load/unload lifecycle all behave")
	quit(0)
