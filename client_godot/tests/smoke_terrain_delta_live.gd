## Headless end-to-end delta-compositing test against a running gateway
## (terrain editing #72/#76): registers, gets the manifest shape via
## `terrain.list`, then requests chunk (5,5)'s tile AND delta and asserts
## the composited height = base + offset once both arrive, plus an
## unedited chunk answering `has_delta: false`. Expects the dev DB to hold
## a delta for chunk (5,5) — the #76 verification seeds a +8m plateau on
## block 0's first 4x4 corners (see the issue log); adjust `_EXPECT_LIFT`
## if reusing with different seed data.
## Run: Godot --headless --path client_godot -s res://tests/smoke_terrain_delta_live.gd
extends SceneTree

const _CHUNK := Vector2i(5, 5)
const _EXPECT_LIFT := 8.0

var _net
var _t := 0.0
var _phase := "auth"
var _base_h := 0.0
var _tile_in := false
var _delta_in := false
var _offsets := PackedFloat32Array()
var _heights := PackedFloat32Array()

func _initialize() -> void:
	randomize()
	_net = load("res://net/NetworkClient.gd").new()
	root.add_child(_net)
	_net.auth_required.connect(func(_v):
		var email := "deltas_%d_%d@t.test" % [Time.get_ticks_msec(), randi()]
		_net.register(email, "pw12", "Deltas"))
	_net.welcome.connect(func(_d):
		_phase = "want_terrain"
		_net.send_terrain_list())
	_net.terrain_data.connect(func(resolution, world_size, heights):
		Protocol.apply_terrain_data(resolution, world_size, heights)
		_phase = "want_chunk"
		_net.send_terrain_tile_request(_CHUNK.x, _CHUNK.y)
		_net.send_terrain_delta_request(_CHUNK.x, _CHUNK.y)
		# An unedited neighbor must answer has_delta=false, not hang.
		_net.send_terrain_delta_request(0, 0))
	_net.terrain_tile_data.connect(func(tx, ty, heights):
		if Vector2i(tx, ty) != _CHUNK:
			return
		_heights = heights
		_tile_in = true
		_check_done())
	_net.terrain_delta_data.connect(func(tx, ty, has_delta, offsets):
		if Vector2i(tx, ty) == Vector2i(0, 0):
			if has_delta:
				push_error("SMOKE_FAIL: unedited chunk (0,0) claimed has_delta=true")
				quit(1)
			else:
				print("SMOKE: unedited chunk (0,0) answered has_delta=false")
			return
		if Vector2i(tx, ty) != _CHUNK:
			return
		if not has_delta:
			push_error("SMOKE_FAIL: chunk (5,5) should carry the seeded delta")
			quit(1)
			return
		_offsets = offsets
		_delta_in = true
		_check_done())
	_net.connect_to("ws://127.0.0.1:8766")

func _check_done() -> void:
	if not (_tile_in and _delta_in):
		return
	var side := Protocol._tile_size + 1
	# Corner (2,2) sits inside the seeded 4x4 plateau; (20,20) is far outside.
	var edited_i := 2 * side + 2
	var plain_i := 20 * side + 20
	if absf(_offsets[edited_i] - _EXPECT_LIFT) > 0.001 or absf(_offsets[plain_i]) > 0.001:
		push_error("SMOKE_FAIL: decoded offsets wrong (edited=%f plain=%f)" % [_offsets[edited_i], _offsets[plain_i]])
		quit(1)
		return
	# Composite the way TerrainStreamer._composited does, then verify via the
	# real height-query path.
	Protocol.apply_terrain_tile(_CHUNK.x, _CHUNK.y, TerrainStreamer._composited(_heights, _offsets))
	var extent := Protocol.terrain_tile_extent_m()
	var edited_h := Protocol.terrain_height(_CHUNK.x * extent + 2 * Protocol._tile_cell_m, _CHUNK.y * extent + 2 * Protocol._tile_cell_m)
	var expect := _heights[edited_i] + _EXPECT_LIFT
	print("SMOKE: chunk (5,5) corner(2,2): base=%.2f composited=%.2f (expect %.2f)" % [_heights[edited_i], edited_h, expect])
	if absf(edited_h - expect) > 0.01:
		push_error("SMOKE_FAIL: composited height %.2f, expected %.2f" % [edited_h, expect])
		quit(1)
		return
	print("SMOKE_OK: live delta round-trip (request, decode, composite, height query) works")
	quit(0)

func _process(delta: float) -> bool:
	_t += delta
	if _t > 20.0:
		push_error("SMOKE_TIMEOUT phase=%s tile_in=%s delta_in=%s" % [_phase, _tile_in, _delta_in])
		quit(1)
		return true
	return false
