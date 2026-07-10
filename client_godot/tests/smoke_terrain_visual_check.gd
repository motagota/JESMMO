## Live visual check (terrain pipeline epic #56, issue #64): connects to a
## REAL running proxy+zone_server (not `--headless` -- this needs an actual
## window/GPU to render into), logs in as a guest, waits for `partition` +
## `terrain.data`, builds the ground mesh exactly like the real client does,
## frames a camera over it, and saves a screenshot -- confirms the ground
## actually renders the baked artifact's real relief with no floating/
## fall-through, the client-side half of #63's server integration (mirrors
## the manual check already done there over a raw websocket, now through the
## actual render path).
##
## Requires a live server on ws://127.0.0.1:8766 (see rust_server/README.md).
## Run WITHOUT --headless (it needs to render), from the repo root:
##   Godot --path client_godot -s res://tests/smoke_terrain_visual_check.gd -- --out=C:/some/path/shot.png
extends SceneTree

var _net
var _world
var _t := 0.0
var _partition_ok := false
var _terrain_ok := false
var _ready_at_frame := -1
var _frame := 0
var _out_path := "user://terrain_visual_check.png"

func _initialize() -> void:
	for arg in OS.get_cmdline_user_args():
		if arg.begins_with("--out="):
			_out_path = arg.substr(len("--out="))

	_world = load("res://world/World.gd").new()
	root.add_child(_world)

	_net = load("res://net/NetworkClient.gd").new()
	root.add_child(_net)
	_net.auth_required.connect(func(_v): _net.guest())
	_net.welcome.connect(func(_d): _net.send_terrain_list())
	_net.partition.connect(func(msg):
		_world.apply_partition(msg)
		_partition_ok = true)
	_net.terrain_data.connect(func(resolution, world_size, heights):
		Protocol.apply_terrain_data(resolution, world_size, heights)
		_world.on_terrain_data()
		_terrain_ok = true)
	_net.connect_to("ws://127.0.0.1:8766")

	# Look down at the town centre (world centre) from an angle high enough
	# to frame a good chunk of the districts around it.
	var mid: float = 6400.0 * 0.5 * Protocol.WORLD_SCALE
	var cam := Camera3D.new()
	root.add_child(cam)
	cam.look_at_from_position(Vector3(mid - 260, 260, mid + 260), Vector3(mid, 0, mid), Vector3.UP)
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
	if _t > 15.0 and _ready_at_frame < 0:
		push_error("SMOKE_TIMEOUT: no partition/terrain.data within 15s -- is a server running on ws://127.0.0.1:8766?")
		quit(1)
		return true
	if _partition_ok and _terrain_ok and _ready_at_frame < 0:
		_ready_at_frame = _frame
	# A few extra frames so the mesh/lighting actually renders before capture.
	if _ready_at_frame >= 0 and _frame >= _ready_at_frame + 10:
		var img := root.get_texture().get_image()
		var err := img.save_png(_out_path)
		if err != OK:
			push_error("SMOKE_FAIL: could not save screenshot to %s (error %d)" % [_out_path, err])
			quit(1)
			return true
		print("SMOKE_OK: wrote screenshot to ", _out_path)
		quit(0)
		return true
	return false
