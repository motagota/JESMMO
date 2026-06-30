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

# id -> { node: MeshInstance3D, target: Vector3, kind: String }
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
			"node": _make_node(kind),
			"target": target,
			"kind": kind,
		}
		add_child(_entities[id]["node"])
		_entities[id]["node"].position = target # spawn at first known position
	else:
		_entities[id]["target"] = target

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
	return 1.0 if kind == "mob" else 1.2

func _make_node(kind: String) -> MeshInstance3D:
	var mi := MeshInstance3D.new()
	if kind == "mob":
		var box := BoxMesh.new()
		box.size = Vector3(1.4, 2.0, 1.4)
		mi.mesh = box
		mi.material_override = _solid(Color(0.85, 0.25, 0.25))
	else:
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
