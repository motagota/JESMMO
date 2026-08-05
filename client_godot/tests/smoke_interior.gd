## Interior rendering on the client (mine epic #164, issue #165): going
## underground hides the surface and lays the authored floor; coming back out
## restores exactly what was there.
##
## The server is already authoritative about where a player can stand — this is
## presentation. But getting it wrong means a tunnel that reads as a field with a
## roof, or a surface that never comes back.
##
## Run: Godot --headless --path client_godot -s res://tests/smoke_interior.gd
extends SceneTree

var _world: World

func _fail(msg: String) -> void:
	print("SMOKE_FAIL: ", msg)
	quit(1)

func _volumes() -> Array:
	return [
		{"x0": 0, "y0": 0, "x1": 80, "y1": 60},
		{"x0": 80, "y0": 20, "x1": 280, "y1": 50},
	]

func _initialize() -> void:
	_world = World.new()
	root.add_child(_world)
	# A headless World never streams terrain, so `_ground`/`_water` would be
	# null and every assertion about hiding the surface would pass vacuously —
	# which it did, until a deliberate sabotage run caught it. Stand in real
	# nodes so the hide/restore path is actually exercised.
	_world._ground = MeshInstance3D.new()
	_world._ground.mesh = BoxMesh.new()
	_world.add_child(_world._ground)
	_world._water = MeshInstance3D.new()
	_world._water.mesh = BoxMesh.new()
	_world.add_child(_world._water)

func _process(_delta: float) -> bool:
	# --- the surface heightmap does not apply underground ---------------------
	# Interior coordinates belong to a different space; sampling the DEM at them
	# would put the floor at whatever height the surface happens to be there.
	if Protocol.in_interior():
		_fail("started in interior mode"); return true

	_world.enter_interior(_volumes())
	if not Protocol.in_interior():
		_fail("entering an interior didn't flatten the terrain sampler"); return true
	if not is_equal_approx(Protocol.terrain_height(12800.0, 13500.0), 0.0):
		_fail("terrain height should be flat underground"); return true

	# --- the floor is actually built -----------------------------------------
	if _world._interior_nodes.size() < 2:
		_fail("expected at least a slab per volume, got %d nodes"
			% _world._interior_nodes.size()); return true

	# --- the surface is hidden, not destroyed --------------------------------
	# It cost real work to stream and the player is coming back out; hiding is
	# also what makes leaving honest, since it restores what was there rather
	# than rebuilding a guess at it.
	var ground = _world._ground
	var water = _world._water
	if ground != null and ground.visible:
		_fail("the surface ground is still visible underground"); return true
	if water != null and water.visible:
		_fail("the water plane is still visible underground"); return true

	# --- and coming back out puts it all back --------------------------------
	_world.leave_interior()
	if Protocol.in_interior():
		_fail("leaving didn't clear interior mode"); return true
	if _world._interior_nodes.size() != 0:
		_fail("interior floor survived leaving (%d nodes)" % _world._interior_nodes.size())
		return true
	if ground != null and not ground.visible:
		_fail("the surface never came back"); return true

	# --- entering twice doesn't stack ----------------------------------------
	_world.enter_interior(_volumes())
	var first := _world._interior_nodes.size()
	_world.enter_interior(_volumes())
	if _world._interior_nodes.size() != first:
		_fail("re-entering an interior stacked a second floor on the first"); return true

	# --- a degenerate volume is skipped, not drawn inside out ----------------
	_world.leave_interior()
	_world.enter_interior([{"x0": 10, "y0": 10, "x1": 10, "y1": 40}])
	if _world._interior_nodes.size() != 0:
		_fail("a zero-area volume was rendered"); return true

	_world.leave_interior()
	print("SMOKE_OK: entering an interior flattens the terrain sampler, lays a slab ",
		"per volume and hides the surface; leaving restores it exactly, re-entry ",
		"doesn't stack, and a degenerate volume is skipped")
	quit(0)
	return true
