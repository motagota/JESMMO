## Live visual check for terrain streaming: connects to a REAL running proxy
## (not `--headless` -- needs a window/GPU), requests the coarse backdrop,
## then simulates a player standing near the river bend so `TerrainStreamer`
## pulls the surrounding native-resolution tiles, and screenshots the result
## from a low, close vantage -- the fine tiles' real 5m relief (and the
## river-brown paint hugging the actual channel) should be visibly sharper
## than the coarse backdrop around them.
##
## Requires a live server on ws://127.0.0.1:8766 (see rust_server/README.md).
## Run WITHOUT --headless, from the repo root:
##   Godot --path client_godot -s res://tests/smoke_terrain_streaming_visual.gd -- --out=C:/some/path/shot.png
extends SceneTree

## Stand on the riverbank at the CBD reach of the S-bend (upper-middle of
## the production AOI).
const _STAND_X := 2500.0
const _STAND_Y := 1400.0

var _net
var _world
var _streamer
var _t := 0.0
var _partition_ok := false
var _terrain_ok := false
var _tiles_seen := 0
var _ready_at_frame := -1
var _frame := 0
var _out_path := "user://terrain_streaming_visual.png"

func _initialize() -> void:
	for arg in OS.get_cmdline_user_args():
		if arg.begins_with("--out="):
			_out_path = arg.substr(len("--out="))

	_world = load("res://world/World.gd").new()
	root.add_child(_world)
	_streamer = load("res://world/TerrainStreamer.gd").new()
	_world.add_child(_streamer)

	_net = load("res://net/NetworkClient.gd").new()
	root.add_child(_net)
	_net.auth_required.connect(func(_v): _net.guest())
	_net.welcome.connect(func(_d): _net.send_terrain_list())
	_net.partition.connect(func(msg):
		_world.apply_partition(msg)
		_streamer.set_context(_world._zones, _world.world_size)
		_partition_ok = true)
	_net.terrain_data.connect(func(resolution, world_size, heights):
		Protocol.apply_terrain_data(resolution, world_size, heights)
		_world.on_terrain_data()
		_terrain_ok = true
		_streamer.on_player_position(_STAND_X, _STAND_Y))
	_net.terrain_tile_data.connect(func(tx, ty, heights):
		_streamer.on_tile_data(tx, ty, heights)
		_tiles_seen += 1)
	_streamer.tile_requested.connect(func(tx, ty): _net.send_terrain_tile_request(tx, ty))
	_net.connect_to("ws://127.0.0.1:8766")

	# Low-ish oblique view over the stand point so the fine tiles fill the frame.
	var sx: float = _STAND_X * Protocol.WORLD_SCALE
	var sz: float = _STAND_Y * Protocol.WORLD_SCALE
	var cam := Camera3D.new()
	root.add_child(cam)
	cam.look_at_from_position(Vector3(sx - 70, 60, sz + 70), Vector3(sx, 0, sz), Vector3.UP)
	cam.current = true

	var sun := DirectionalLight3D.new()
	sun.rotation_degrees = Vector3(-55, -40, 0)
	sun.light_energy = 1.2
	root.add_child(sun)

	var env := Environment.new()
	env.background_mode = Environment.BG_COLOR
	env.background_color = Color(0.04, 0.05, 0.07)
	env.ambient_light_source = Environment.AMBIENT_SOURCE_COLOR
	env.ambient_light_color = Color(0.5, 0.5, 0.55)
	env.ambient_light_energy = 0.6
	var we := WorldEnvironment.new()
	we.environment = env
	root.add_child(we)

func _process(delta: float) -> bool:
	_frame += 1
	_t += delta
	if _t > 20.0 and _ready_at_frame < 0:
		push_error("SMOKE_TIMEOUT: partition=%s terrain=%s tiles=%d -- is a server running on ws://127.0.0.1:8766?" % [_partition_ok, _terrain_ok, _tiles_seen])
		quit(1)
		return true
	# Wait for the full ring (the stand point is interior -> 9 tiles).
	if _partition_ok and _terrain_ok and _tiles_seen >= 9 and _ready_at_frame < 0:
		_ready_at_frame = _frame
	if _ready_at_frame >= 0 and _frame >= _ready_at_frame + 10:
		var img := root.get_texture().get_image()
		var err := img.save_png(_out_path)
		if err != OK:
			push_error("SMOKE_FAIL: could not save screenshot to %s (error %d)" % [_out_path, err])
			quit(1)
			return true
		print("SMOKE_OK: %d fine tiles streamed in; wrote screenshot to %s" % [_tiles_seen, _out_path])
		quit(0)
		return true
	return false
