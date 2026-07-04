## Spawns, updates, interpolates, and despawns *remote* entities (other players
## and mobs) from 20 Hz `status_update` snapshots.
##
## The local player is owned by `LocalPlayer` (which predicts its own movement);
## this manager skips the local id. Remote entities render at display rate by
## easing each toward the last authoritative position, mirroring the 2D client.
class_name EntityManager
extends Node3D

## Smoothing rate for remote interpolation (higher = snappier). Frame-rate aware.
const _INTERP_RATE := 12.0

var local_id := ""

# id -> { node, target: Vector3, kind, wpos: Vector2, item_id, qty }
var _entities: Dictionary = {}

func set_local_id(id: String) -> void:
	local_id = id
	# If the local player had been shown as a remote entity before `welcome`
	# resolved our id, drop that duplicate.
	if _entities.has(id):
		_remove(id)

func upsert(id: String, _zone: String, state: Dictionary) -> void:
	if id == local_id or id == "":
		return
	var wx := float(state.get("x", 0))
	var wy := float(state.get("y", 0))
	var kind := String(state.get("type", "player"))
	var target := Protocol.w2v(wx, wy, _height_for(kind))

	if not _entities.has(id):
		_entities[id] = {
			"node": _make_node(kind, state),
			"target": target,
			"kind": kind,
		}
		add_child(_entities[id]["node"])
		_entities[id]["node"].position = target # spawn at first known position
	else:
		_entities[id]["target"] = target
	_entities[id]["wpos"] = Vector2(wx, wy)
	_entities[id]["item_id"] = String(state.get("item_id", ""))
	_entities[id]["qty"] = int(state.get("qty", 0))

## The id of the nearest live resource node within `max_dist` world units of
## `from` (world coords), or "" if none. Used by the gather interaction.
func nearest_resource(from: Vector2, max_dist: float) -> String:
	return _nearest(from, max_dist, "resource", true)

## The id of the nearest storage point within `max_dist`, or "" if none.
func nearest_storage(from: Vector2, max_dist: float) -> String:
	return _nearest(from, max_dist, "storage", false)

## The id of the nearest build board within `max_dist`, or "" if none.
func nearest_build_board(from: Vector2, max_dist: float) -> String:
	return _nearest(from, max_dist, "build_board", false)

## The id of the nearest bed within `max_dist`, or "" if none (for the
## sleep/set-respawn interaction, #12).
func nearest_bed(from: Vector2, max_dist: float) -> String:
	return _nearest(from, max_dist, "bed", false)

## The id of the nearest crafting station within `max_dist`, or "" if none (#12).
func nearest_crafting(from: Vector2, max_dist: float) -> String:
	return _nearest(from, max_dist, "crafting", false)

func _nearest(from: Vector2, max_dist: float, kind: String, need_stock: bool) -> String:
	var best := ""
	var best_d := max_dist
	for id in _entities:
		var rec: Dictionary = _entities[id]
		if rec.get("kind", "") != kind:
			continue
		if need_stock and int(rec.get("qty", 0)) <= 0:
			continue
		var d := from.distance_to(rec.get("wpos", Vector2.ZERO))
		if d <= best_d:
			best_d = d
			best = id
	return best

func remove(id: String) -> void:
	_remove(id)

func _process(delta: float) -> void:
	var t := clampf(_INTERP_RATE * delta, 0.0, 1.0)
	for rec in _entities.values():
		var node: Node3D = rec["node"]
		node.position = node.position.lerp(rec["target"], t)

func _remove(id: String) -> void:
	if _entities.has(id):
		_entities[id]["node"].queue_free()
		_entities.erase(id)

func _height_for(kind: String) -> float:
	match kind:
		"mob": return 1.0
		"resource": return 1.5
		"storage": return 0.6
		"build_board": return 0.9
		"structure": return 1.0
		"bed": return 0.5
		"crafting": return 0.9
		_: return 1.2

func _make_node(kind: String, state: Dictionary) -> MeshInstance3D:
	var mi := MeshInstance3D.new()
	match kind:
		"mob":
			var box := BoxMesh.new()
			box.size = Vector3(1.4, 2.0, 1.4)
			mi.mesh = box
			mi.material_override = _solid(Color(0.85, 0.25, 0.25))
		"resource":
			# Trees (wood) as green cones, rocks (stone) as grey boxes.
			if String(state.get("item_id", "")) == "stone":
				var rock := BoxMesh.new()
				rock.size = Vector3(2.0, 2.0, 2.0)
				mi.mesh = rock
				mi.material_override = _solid(Color(0.55, 0.55, 0.58))
			else:
				var tree := CylinderMesh.new()
				tree.top_radius = 0.0
				tree.bottom_radius = 2.0
				tree.height = 4.0
				mi.mesh = tree
				mi.material_override = _solid(Color(0.18, 0.65, 0.30))
		"storage":
			# A storehouse chest.
			var chest := BoxMesh.new()
			chest.size = Vector3(3.0, 1.4, 2.0)
			mi.mesh = chest
			mi.material_override = _solid(Color(0.6, 0.45, 0.2))
		"build_board":
			# A notice board: a tall bright slab with a floating label so it stands
			# out among the town-centre fixtures and is easy to walk up to.
			var slab := BoxMesh.new()
			slab.size = Vector3(2.6, 2.0, 0.4)
			mi.mesh = slab
			mi.material_override = _solid(Color(0.95, 0.75, 0.15))
			_add_label(mi, "🔨 Build Orders", 2.4, Color(1.0, 0.9, 0.4))
		"structure":
			# A completed city structure (well/wall/stall). A pale stone block labelled
			# with its kind; the authored kind rides in state.kind.
			var block := BoxMesh.new()
			block.size = Vector3(3.0, 2.4, 3.0)
			mi.mesh = block
			mi.material_override = _solid(Color(0.75, 0.78, 0.8))
			var kind_name := String(state.get("kind", "")).capitalize()
			if kind_name != "":
				_add_label(mi, kind_name, 2.6, Color(0.85, 0.95, 1.0))
		"bed":
			# A home bed: a low, warm-toned slab (the respawn anchor, #12).
			var frame := BoxMesh.new()
			frame.size = Vector3(2.0, 0.6, 1.2)
			mi.mesh = frame
			mi.material_override = _solid(Color(0.55, 0.35, 0.65))
		"crafting":
			# A home crafting station: a stout workbench.
			var bench := BoxMesh.new()
			bench.size = Vector3(2.0, 1.1, 1.6)
			mi.mesh = bench
			mi.material_override = _solid(Color(0.65, 0.45, 0.2))
			_add_label(mi, "🛠 Craft", 1.8, Color(1.0, 0.85, 0.5))
		_:
			var cap := CapsuleMesh.new()
			cap.radius = 0.6
			cap.height = 2.2
			mi.mesh = cap
			mi.material_override = _solid(Color(0.30, 0.55, 1.0))
	return mi

func _solid(c: Color) -> StandardMaterial3D:
	var m := StandardMaterial3D.new()
	m.albedo_color = c
	return m

## Attach a billboarded text label floating `height` metres above an entity mesh,
## drawn on top (no depth test) so it's readable from across the district.
func _add_label(parent: Node3D, text: String, height: float, color: Color) -> void:
	var label := Label3D.new()
	label.text = text
	label.billboard = BaseMaterial3D.BILLBOARD_ENABLED
	label.no_depth_test = true
	label.fixed_size = true
	label.pixel_size = 0.004
	label.modulate = color
	label.outline_size = 8
	label.position = Vector3(0, height, 0)
	parent.add_child(label)
