## Live visual check for water rendering (`World._build_water` +
## `world/water.gdshader`): connects to a REAL running proxy (not
## `--headless` -- needs a window/GPU), waits for the coarse backdrop, then
## SCANS it for the water nearest the map centre (the CBD river reach on the
## v3 bake -- no hardcoded coords to go stale with the next bake), stands a
## simulated player on the nearest shore so `TerrainStreamer` pulls the
## surrounding native-resolution tiles, and screenshots across the waterline
## from a low oblique vantage. The shot should show translucent muddy water
## fading out at the bank (no hard polygon waterline), darkening over the
## channel, with sun glints off the animated waves.
##
## Uses Main._build_environment's real sky/fog/sun values so the shader's
## fresnel-to-sky and fog behavior are judged against the in-game look.
##
## Requires a live server on ws://127.0.0.1:8766 (see rust_server/README.md).
## Run WITHOUT --headless, from the repo root:
##   Godot --path client_godot -s res://tests/smoke_water_visual.gd -- --out=C:/some/path/shot.png
extends SceneTree

var _net
var _world
var _streamer
var _t := 0.0
var _partition_ok := false
var _terrain_ok := false
var _tiles_seen := 0
var _ready_at_frame := -1
var _frame := 0
var _out_path := "user://water_visual.png"
## Camera height above the bank (m). 60 = survey view of the whole reach;
## pass --eye=2 for what a player standing on the bank actually sees.
var _eye_height := 60.0

func _initialize() -> void:
	for arg in OS.get_cmdline_user_args():
		if arg.begins_with("--out="):
			_out_path = arg.substr(len("--out="))
		elif arg.begins_with("--eye="):
			_eye_height = float(arg.substr(len("--eye=")))

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
		_terrain_ok = _frame_shoreline(resolution, world_size))
	_net.terrain_tile_data.connect(func(tx, ty, heights):
		_streamer.on_tile_data(tx, ty, heights)
		_tiles_seen += 1)
	_net.terrain_delta_data.connect(func(tx, ty, has_delta, offsets):
		_streamer.on_delta_data(tx, ty, has_delta, offsets))
	_streamer.tile_requested.connect(func(tx, ty): _net.send_terrain_tile_request(tx, ty))
	_streamer.delta_requested.connect(func(tx, ty): _net.send_terrain_delta_request(tx, ty))
	_net.connect_to("ws://127.0.0.1:8766")

	# The real in-game environment (Main._build_environment) -- the water
	# fresnel blends toward the sky color and must be judged against it.
	var env := Environment.new()
	env.background_mode = Environment.BG_COLOR
	env.background_color = Color(0.55, 0.63, 0.72)
	env.ambient_light_source = Environment.AMBIENT_SOURCE_COLOR
	env.ambient_light_color = Color(0.5, 0.5, 0.55)
	env.ambient_light_energy = 0.6
	env.fog_enabled = true
	env.fog_light_color = Color(0.55, 0.63, 0.72)
	env.fog_density = 0.0001
	env.fog_sun_scatter = 0.05
	var we := WorldEnvironment.new()
	we.environment = env
	root.add_child(we)

	var sun := DirectionalLight3D.new()
	sun.rotation_degrees = Vector3(-55, -40, 0)
	sun.light_energy = 1.1
	root.add_child(sun)

## Find the sea-level-or-below backdrop cell nearest the map centre, then the
## nearest genuinely-high cell to it (the bank), stand the streamer's player
## on the bank, and aim the camera from behind/above the bank across the
## waterline. Returns false (test will time out and fail) if the received
## heightmap contains no water at all.
func _frame_shoreline(resolution: int, world_size: float) -> bool:
	var step := world_size / float(resolution)
	var centre := Vector2(world_size * 0.5, world_size * 0.5)
	var water := Vector2(-1, -1)
	var best := INF
	for gy in range(resolution):
		for gx in range(resolution):
			var p := Vector2((gx + 0.5) * step, (gy + 0.5) * step)
			if Protocol.terrain_height(p.x, p.y) <= 0.0:
				var d2 := p.distance_squared_to(centre)
				if d2 < best:
					best = d2
					water = p
	if water.x < 0.0:
		push_error("SMOKE_FAIL: no water (height <= 0m) anywhere in the coarse backdrop")
		quit(1)
		return false
	# Nearest solidly-dry cell to that water: the bank we stand on.
	var bank := water
	best = INF
	for gy in range(resolution):
		for gx in range(resolution):
			var p := Vector2((gx + 0.5) * step, (gy + 0.5) * step)
			if Protocol.terrain_height(p.x, p.y) >= 2.0:
				var d2 := p.distance_squared_to(water)
				if d2 < best:
					best = d2
					bank = p
	print("water cell at %s (h=%.1fm), bank at %s (h=%.1fm)" % [
		water, Protocol.terrain_height(water.x, water.y),
		bank, Protocol.terrain_height(bank.x, bank.y)])
	_streamer.on_player_position(bank.x, bank.y)

	# Camera near the waterline (not at the bank cell's centre — a tall
	# levee there hides the whole river at low eye heights), looking out
	# across the channel.
	var dir := (water - bank).normalized()
	var cam_from := bank.lerp(water, 0.6)
	var look_at := water + dir * 300.0 # well out over the water
	var cam := Camera3D.new()
	root.add_child(cam)
	var ground_h := Protocol.terrain_height(cam_from.x, cam_from.y) * Protocol.HEIGHT_SCALE
	var from_h := maxf(ground_h, World._WATER_LEVEL_M * Protocol.HEIGHT_SCALE + World._WATER_Y)
	cam.look_at_from_position(
		Vector3(cam_from.x * Protocol.WORLD_SCALE, from_h + _eye_height, cam_from.y * Protocol.WORLD_SCALE),
		Protocol.w2v(look_at.x, look_at.y, 0.0), Vector3.UP)
	cam.current = true
	return true

func _process(delta: float) -> bool:
	_frame += 1
	_t += delta
	if _t > 20.0 and _ready_at_frame < 0:
		push_error("SMOKE_TIMEOUT: partition=%s terrain=%s tiles=%d -- is a server running on ws://127.0.0.1:8766?" % [_partition_ok, _terrain_ok, _tiles_seen])
		quit(1)
		return true
	# Wait for a good chunk of the ring, then a beat more so the wave
	# animation is mid-motion and late tiles have landed.
	if _partition_ok and _terrain_ok and _tiles_seen >= 9 and _ready_at_frame < 0:
		_ready_at_frame = _frame
	if _ready_at_frame >= 0 and _frame >= _ready_at_frame + 30:
		var img := root.get_texture().get_image()
		var err := img.save_png(_out_path)
		if err != OK:
			push_error("SMOKE_FAIL: could not save screenshot to %s (error %d)" % [_out_path, err])
			quit(1)
			return true
		print("SMOKE_OK: %d fine tiles streamed; wrote screenshot to %s" % [_tiles_seen, _out_path])
		quit(0)
		return true
	return false
