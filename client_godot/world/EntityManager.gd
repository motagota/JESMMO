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
	var best := ""
	var best_d := max_dist
	for id in _entities:
		var rec: Dictionary = _entities[id]
		if rec.get("kind", "") != "resource" or int(rec.get("qty", 0)) <= 0:
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
