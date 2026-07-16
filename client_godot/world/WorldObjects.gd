## Placed world props (player-attributes epic #83, #86): renders the
## `object.list` roster plus the `object.placed` / `object.removed`
## broadcasts every client receives. Purely a mirror of server state — the
## editor's ObjectTool never adds nodes here directly, it waits for its own
## placement to come back as a broadcast (the terrain-brush reconcile
## philosophy: the server owns the truth, the client renders acks).
##
## Objects ground-snap through `Protocol.w2v`, which already prefers the
## composited (base + hand-edit delta) heights wherever a streamed fine tile
## is resident — so trees sit correctly on edited terrain, and
## `refresh_heights` (wired to `TerrainStreamer.terrain_changed`) re-snaps
## them whenever the displayed surface changes, same as plot markers.
class_name WorldObjects
extends Node3D

## id -> {"kind": String, "x": float, "y": float, "node": Node3D}
var _objects: Dictionary = {}

## Replace the whole roster (the `object.list` answer). Replace-not-merge:
## the answer is authoritative, and anything rendered that it doesn't name
## (e.g. from a broadcast that raced ahead of a reconnect's fresh list) goes.
func apply_list(objects: Array) -> void:
	for id in _objects.keys().duplicate():
		on_removed(id)
	for o in objects:
		on_placed(
			String(o.get("id", "")),
			String(o.get("kind", "")),
			float(o.get("x", 0)),
			float(o.get("y", 0)))

func on_placed(id: String, kind: String, x: float, y: float) -> void:
	if id == "" or _objects.has(id):
		return # duplicate broadcast/list overlap — the first render stands
	var node := make_object_node(kind)
	add_child(node)
	node.position = Protocol.w2v(x, y)
	_objects[id] = {"kind": kind, "x": x, "y": y, "node": node}

func on_removed(id: String) -> void:
	if not _objects.has(id):
		return
	_objects[id]["node"].queue_free()
	_objects.erase(id)

## Re-snap every object onto the currently displayed terrain — streamed
## tiles arriving/leaving and accepted edits both move the ground under a
## statically-placed prop.
func refresh_heights() -> void:
	for rec in _objects.values():
		rec["node"].position = Protocol.w2v(rec["x"], rec["y"])

## The placed object nearest to world point `g` within `max_dist`, or ""
## — the ObjectTool's delete-mode picker.
func object_at(g: Vector2, max_dist: float) -> String:
	var best_id := ""
	var best_d := max_dist
	for id in _objects:
		var rec: Dictionary = _objects[id]
		var d := Vector2(rec["x"], rec["y"]).distance_to(g)
		if d <= best_d:
			best_d = d
			best_id = id
	return best_id

func count() -> int:
	return _objects.size()

func has_object(id: String) -> bool:
	return _objects.has(id)

## World position of a rendered object (tests / tooling).
func object_pos(id: String) -> Vector2:
	if not _objects.has(id):
		return Vector2.INF
	return Vector2(_objects[id]["x"], _objects[id]["y"])

## Build one object's visual, origin at ground level. Static so the
## ObjectTool can build its ghost preview from the exact same mesh.
## `ghost` renders it translucent and shadowless.
static func make_object_node(kind: String, ghost := false) -> Node3D:
	var root := Node3D.new()
	match kind:
		"poison_tree":
			# Must read as "do not touch" at a glance among the bright-green
			# gatherable wood trees: a near-black gnarled trunk under a murky
			# toxic-purple canopy with a faint sick glow.
			var trunk := MeshInstance3D.new()
			var trunk_mesh := CylinderMesh.new()
			trunk_mesh.top_radius = 0.18
			trunk_mesh.bottom_radius = 0.45
			trunk_mesh.height = 3.0
			trunk.mesh = trunk_mesh
			trunk.position = Vector3(0, 1.5, 0)
			trunk.material_override = _object_material(Color(0.14, 0.11, 0.10), Color(), ghost)
			root.add_child(trunk)
			var canopy := MeshInstance3D.new()
			var canopy_mesh := CylinderMesh.new()
			canopy_mesh.top_radius = 0.0
			canopy_mesh.bottom_radius = 2.4
			canopy_mesh.height = 3.6
			canopy.mesh = canopy_mesh
			canopy.position = Vector3(0, 4.2, 0)
			canopy.material_override = _object_material(
				Color(0.32, 0.13, 0.40), Color(0.25, 0.05, 0.35), ghost)
			root.add_child(canopy)
		_:
			# An unknown kind still renders *something* visible (a warning
			# magenta block) rather than an invisible gameplay hazard — old
			# clients meeting a future object kind degrade loudly, not silently.
			var block := MeshInstance3D.new()
			var mesh := BoxMesh.new()
			mesh.size = Vector3(1.5, 1.5, 1.5)
			block.mesh = mesh
			block.position = Vector3(0, 0.75, 0)
			block.material_override = _object_material(Color(0.9, 0.1, 0.9), Color(), ghost)
			root.add_child(block)
	return root

static func _object_material(albedo: Color, emission: Color, ghost: bool) -> StandardMaterial3D:
	var m := StandardMaterial3D.new()
	m.albedo_color = albedo
	if emission != Color():
		m.emission_enabled = true
		m.emission = emission
		m.emission_energy_multiplier = 0.25
	if ghost:
		m.transparency = BaseMaterial3D.TRANSPARENCY_ALPHA
		m.albedo_color.a = 0.45
	return m
