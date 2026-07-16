## Headless smoke test for placed world props (#86): WorldObjects mirrors
## roster/broadcast state (replace-not-merge list, duplicate-broadcast guard,
## removal), ground-snaps through the terrain and re-snaps on refresh, and
## the ObjectTool's mode cycle + click routing emit the right requests
## without ever mutating WorldObjects locally.
## Run: Godot --headless --path client_godot -s res://tests/smoke_objects.gd
extends SceneTree

func _fail(message: String) -> void:
	print("SMOKE_FAIL: %s" % message)
	quit(1)

func _initialize() -> void:
	# Flat terrain at 5m so the snap height is a known, non-zero value.
	Protocol.apply_terrain_data(1, 6400.0, PackedFloat32Array([5.0, 5.0, 5.0, 5.0]))

	var objects := WorldObjects.new()
	root.add_child(objects)

	# --- roster + broadcast bookkeeping --------------------------------------
	objects.apply_list([
		{"id": "a", "kind": "poison_tree", "x": 100, "y": 200},
		{"id": "b", "kind": "poison_tree", "x": 110, "y": 200},
	])
	if objects.count() != 2:
		_fail("roster of 2 should render 2 (got %d)" % objects.count()); return
	objects.on_placed("a", "poison_tree", 100, 200) # duplicate broadcast
	if objects.count() != 2:
		_fail("a duplicate object.placed must not double-render"); return
	objects.on_placed("c", "poison_tree", 120, 200)
	if objects.count() != 3 or not objects.has_object("c"):
		_fail("a broadcast placement should render"); return
	objects.on_removed("b")
	if objects.count() != 2 or objects.has_object("b"):
		_fail("object.removed should un-render exactly that object"); return
	objects.apply_list([{"id": "z", "kind": "poison_tree", "x": 50, "y": 50}])
	if objects.count() != 1 or not objects.has_object("z"):
		_fail("apply_list must replace, not merge (got %d)" % objects.count()); return

	# --- ground snap + refresh ------------------------------------------------
	var node: Node3D = objects._objects["z"]["node"]
	var want_y := 5.0 * Protocol.HEIGHT_SCALE
	if absf(node.position.y - want_y) > 0.001:
		_fail("object should snap to terrain (y=%f, want %f)" % [node.position.y, want_y]); return
	Protocol.apply_terrain_data(1, 6400.0, PackedFloat32Array([9.0, 9.0, 9.0, 9.0]))
	objects.refresh_heights()
	if absf(node.position.y - 9.0 * Protocol.HEIGHT_SCALE) > 0.001:
		_fail("refresh_heights should re-snap onto the new surface"); return

	# --- delete picking --------------------------------------------------------
	objects.apply_list([
		{"id": "near", "kind": "poison_tree", "x": 100, "y": 100},
		{"id": "far", "kind": "poison_tree", "x": 130, "y": 100},
	])
	if objects.object_at(Vector2(102, 101), 6.0) != "near":
		_fail("object_at should pick the nearest object in radius"); return
	if objects.object_at(Vector2(115, 100), 6.0) != "":
		_fail("object_at should pick nothing outside the radius"); return

	# --- unknown kinds degrade loudly, not invisibly ---------------------------
	objects.on_placed("mystery", "not_a_kind_yet", 10, 10)
	var mystery: Node3D = objects._objects["mystery"]["node"]
	if mystery.get_child_count() == 0:
		_fail("an unknown kind must still render a visible placeholder"); return

	# --- the ObjectTool emits requests, never touches WorldObjects -------------
	var tool := ObjectTool.new()
	tool.objects = objects
	root.add_child(tool)
	var placed: Array = []
	var deleted: Array = []
	var modes: Array = []
	tool.place_requested.connect(func(kind, x, y): placed.append([kind, x, y]))
	tool.delete_requested.connect(func(id): deleted.append(id))
	tool.mode_changed.connect(func(m): modes.append(m))

	tool.set_mode("place")
	if tool._ghost == null:
		_fail("place mode should show a ghost preview"); return
	var before := objects.count()
	tool._click(Vector2(200.4, 300.6))
	if placed != [["poison_tree", 200, 301]]:
		_fail("place click should request the rounded cursor cell (got %s)" % str(placed)); return
	if objects.count() != before:
		_fail("a place click must not render locally — the broadcast does that"); return

	tool.set_mode("delete")
	if tool._ghost != null:
		_fail("delete mode should drop the ghost"); return
	tool._click(Vector2(101, 100))
	if deleted != ["near"]:
		_fail("delete click should request the picked object (got %s)" % str(deleted)); return
	if objects.has_object("near") == false:
		_fail("a delete click must not un-render locally — the broadcast does that"); return
	tool._click(Vector2(5000, 5000))
	if deleted.size() != 1:
		_fail("a delete click on empty ground must request nothing"); return

	tool.set_mode("off")
	if modes != ["place", "delete", "off"]:
		_fail("mode_changed should have fired per transition (got %s)" % str(modes)); return

	print("SMOKE_OK: world objects render/replace/remove, ground-snap + re-snap, delete picking, unknown-kind fallback, and the object tool routes requests without local mutation")
	quit(0)
