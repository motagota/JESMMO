## Visual check for the poison-tree prop (#86): renders a short line of
## poison trees next to a gatherable wood tree (EntityManager's bright green
## cone) on flat ground and screenshots it — the poison tree must read as
## "do not touch" at a glance beside the friendly one. Also shows the
## ObjectTool's translucent ghost. No server needed.
## Run: Godot --path client_godot -s res://tests/smoke_objects_visual.gd
## Screenshot: user://objects_visual.png
extends SceneTree

var _frames := 0

func _initialize() -> void:
	Protocol.apply_terrain_data(1, 6400.0, PackedFloat32Array([0.0, 0.0, 0.0, 0.0]))

	var env := Environment.new()
	env.background_mode = Environment.BG_COLOR
	env.background_color = Color(0.55, 0.63, 0.72)
	env.ambient_light_source = Environment.AMBIENT_SOURCE_COLOR
	env.ambient_light_color = Color(0.5, 0.5, 0.55)
	env.ambient_light_energy = 0.6
	var we := WorldEnvironment.new()
	we.environment = env
	root.add_child(we)
	var sun := DirectionalLight3D.new()
	sun.rotation_degrees = Vector3(-55, -40, 0)
	sun.light_energy = 1.1
	root.add_child(sun)

	var ground := MeshInstance3D.new()
	var plane := PlaneMesh.new()
	plane.size = Vector2(200, 200)
	ground.mesh = plane
	ground.position = Vector3(100, 0, 100)
	var mat := StandardMaterial3D.new()
	mat.albedo_color = Color(0.38, 0.52, 0.30)
	ground.material_override = mat
	root.add_child(ground)

	var objects := WorldObjects.new()
	root.add_child(objects)
	objects.apply_list([
		{"id": "p1", "kind": "poison_tree", "x": 92, "y": 100},
		{"id": "p2", "kind": "poison_tree", "x": 100, "y": 100},
		{"id": "p3", "kind": "poison_tree", "x": 108, "y": 100},
	])

	# The friendly gatherable wood tree, for contrast (EntityManager's mesh).
	var friendly := MeshInstance3D.new()
	var cone := CylinderMesh.new()
	cone.top_radius = 0.0
	cone.bottom_radius = 2.0
	cone.height = 4.0
	friendly.mesh = cone
	var green := StandardMaterial3D.new()
	green.albedo_color = Color(0.18, 0.65, 0.30)
	friendly.material_override = green
	friendly.position = Vector3(118, 1.5, 100)
	root.add_child(friendly)

	# The placement ghost, hovering where a cursor would be.
	var ghost := WorldObjects.make_object_node("poison_tree", true)
	ghost.position = Protocol.w2v(84.0, 106.0)
	root.add_child(ghost)

	var cam := Camera3D.new()
	cam.position = Vector3(100, 6.0, 122)
	root.add_child(cam)
	cam.look_at(Vector3(100, 3.0, 100))
	cam.make_current()

func _process(_delta: float) -> bool:
	_frames += 1
	if _frames == 12:
		var img := root.get_viewport().get_texture().get_image()
		img.save_png("user://objects_visual.png")
		print("SMOKE_OK: wrote screenshot to user://objects_visual.png")
		quit(0)
	return false
