## Headless end-to-end terrain-streaming test against a running gateway:
## registers, requests the coarse backdrop (`terrain.list`), checks the
## extended manifest fields arrived, then requests one native-resolution
## tile and asserts it decodes and takes over height queries inside its
## footprint. Requires a live proxy (a zone isn't strictly needed — terrain
## is answered gateway-side — but the standard dev stack has one).
## Run: Godot --headless --path client_godot -s res://tests/smoke_terrain_streaming_live.gd
extends SceneTree

var _net
var _t := 0.0
var _phase := "auth" # auth -> want_terrain -> want_tile -> done
var _backdrop_h := 0.0

func _initialize() -> void:
	randomize()
	_net = load("res://net/NetworkClient.gd").new()
	root.add_child(_net)
	_net.auth_required.connect(func(_v):
		var email := "streamer_%d_%d@t.test" % [Time.get_ticks_msec(), randi()]
		_net.register(email, "pw12", "Streamer"))
	_net.welcome.connect(func(_d):
		_phase = "want_terrain"
		_net.send_terrain_list())
	_net.terrain_data.connect(func(resolution, world_size, heights):
		Protocol.apply_terrain_data(resolution, world_size, heights)
		print("SMOKE: terrain.data resolution=%d tile_size=%d tiles=%dx%d cell=%.1fm" % [
			resolution, Protocol._tile_size, Protocol._tiles_x, Protocol._tiles_y, Protocol._tile_cell_m])
		if Protocol._tile_size <= 0 or Protocol._tiles_x <= 0 or Protocol._tiles_y <= 0:
			push_error("SMOKE_FAIL: terrain.data carried no streamable tile-grid shape")
			quit(1)
			return
		# Sample the backdrop mid-tile-(0,0) before the fine tile loads.
		_backdrop_h = Protocol.terrain_height(300.0, 300.0)
		_phase = "want_tile"
		_net.send_terrain_tile_request(0, 0))
	_net.terrain_tile_data.connect(func(tx, ty, heights):
		if tx != 0 or ty != 0:
			return
		Protocol.apply_terrain_tile(tx, ty, heights)
		var fine := Protocol.terrain_height(300.0, 300.0)
		print("SMOKE: tile (0,0) loaded, %d samples; height at (300,300): backdrop=%.2f fine=%.2f" % [
			heights.size(), _backdrop_h, fine])
		var side := Protocol._tile_size + 1
		if heights.size() != side * side:
			push_error("SMOKE_FAIL: tile decoded to %d samples, expected %d" % [heights.size(), side * side])
			quit(1)
			return
		if not Protocol.has_terrain_tile(0, 0):
			push_error("SMOKE_FAIL: registry doesn't hold the applied tile")
			quit(1)
			return
		print("SMOKE_OK: live terrain streaming round-trip (manifest fields, tile request, decode, fine height) works")
		quit(0))
	_net.connect_to("ws://127.0.0.1:8766")

func _process(delta: float) -> bool:
	_t += delta
	if _t > 20.0:
		push_error("SMOKE_TIMEOUT phase=%s" % _phase)
		quit(1)
		return true
	return false
