## The 3D capital: a ground plane, the authored district tiles, and the main
## roads, all rebuilt from the gateway's `partition` message.
##
## The capital starts empty — this draws the ground and districts (named, tinted
## by safety) plus a couple of authored roads and a town-centre marker. Structures
## (homes, the build-order board models) arrive with later milestones; the data to
## place them comes over the wire then.
class_name World
extends Node3D

const _GROUND_Y := 0.0
const _TILE_Y := 0.02   # district tiles sit just above the ground to avoid z-fighting
const _ROAD_Y := 0.05

var world_size := 1200.0

var _ground: MeshInstance3D
var _tiles_root := Node3D.new()
var _roads_root := Node3D.new()
var _built_static := false

func _ready() -> void:
	add_child(_tiles_root)
	add_child(_roads_root)

## Rebuild the district tiles from a `partition` message; lazily build the static
## ground/roads once the world size is known.
func apply_partition(msg: Dictionary) -> void:
	world_size = float(msg.get("world", world_size))
	if not _built_static:
		_build_ground()
		_build_roads()
		_built_static = true

	for child in _tiles_root.get_children():
		child.queue_free()

	for entry_v in msg.get("zones", []):
		var z: Dictionary = entry_v
		_add_district_tile(z)

func _build_ground() -> void:
	_ground = MeshInstance3D.new()
	var plane := PlaneMesh.new()
	plane.size = Vector2(world_size, world_size) * Protocol.WORLD_SCALE
	_ground.mesh = plane
	# PlaneMesh is centred on its origin; shift so world (0,0) is a corner.
	_ground.position = Protocol.w2v(world_size * 0.5, world_size * 0.5, _GROUND_Y)
	var mat := StandardMaterial3D.new()
	mat.albedo_color = Color(0.10, 0.14, 0.10)
	_ground.material_override = mat
	add_child(_ground)

func _build_roads() -> void:
	# Mirrors mmo::world::capital(): a main avenue across the mid-latitude and a
	# civic cross-street through the town centre at the world's centre.
	var mid := world_size * 0.5
	var road := Color(0.20, 0.20, 0.22)
	_add_strip(Vector2(0, mid), Vector2(world_size, mid), 24.0, road)        # avenue
	_add_strip(Vector2(mid, 0), Vector2(mid, world_size), 24.0, road)        # cross-street

	# Town-centre marker (the spawn anchor / first build-order board).
	var marker := MeshInstance3D.new()
	var cyl := CylinderMesh.new()
	cyl.top_radius = 6.0 * Protocol.WORLD_SCALE
	cyl.bottom_radius = 6.0 * Protocol.WORLD_SCALE
	cyl.height = 3.0
	marker.mesh = cyl
	marker.position = Protocol.w2v(mid, mid, 1.5)
	var mm := StandardMaterial3D.new()
	mm.albedo_color = Color(0.95, 0.85, 0.30)
	marker.material_override = mm
	_roads_root.add_child(marker)

func _add_strip(a: Vector2, b: Vector2, width: float, color: Color) -> void:
	var strip := MeshInstance3D.new()
	var box := BoxMesh.new()
	var length := a.distance_to(b)
	# Horizontal strip if a.y == b.y, else vertical — both axis-aligned here.
	if absf(a.y - b.y) < 0.5:
		box.size = Vector3(length, 0.04, width) * Protocol.WORLD_SCALE
	else:
		box.size = Vector3(width, 0.04, length) * Protocol.WORLD_SCALE
	strip.mesh = box
	var mid := (a + b) * 0.5
	strip.position = Protocol.w2v(mid.x, mid.y, _ROAD_Y)
	var mat := StandardMaterial3D.new()
	mat.albedo_color = color
	strip.material_override = mat
	_roads_root.add_child(strip)

func _add_district_tile(z: Dictionary) -> void:
	var x0 := float(z.get("x0", 0))
	var y0 := float(z.get("y0", 0))
	var x1 := float(z.get("x1", 0))
	var y1 := float(z.get("y1", 0))
	var w := x1 - x0
	var h := y1 - y0
	if w <= 0.0 or h <= 0.0:
		return

	var tile := MeshInstance3D.new()
	var plane := PlaneMesh.new()
	plane.size = Vector2(w, h) * Protocol.WORLD_SCALE
	tile.mesh = plane
	tile.position = Protocol.w2v(x0 + w * 0.5, y0 + h * 0.5, _TILE_Y)

	var safe := String(z.get("safety", "wilds")) == "safe"
	var mat := StandardMaterial3D.new()
	mat.albedo_color = (Color(0.12, 0.30, 0.18, 0.5) if safe
		else Color(0.30, 0.12, 0.12, 0.5))
	mat.transparency = BaseMaterial3D.TRANSPARENCY_ALPHA
	tile.material_override = mat
	_tiles_root.add_child(tile)

	var district_name: String = String(z.get("district", z.get("zone_id", "")))
	if district_name != "":
		var label := Label3D.new()
		label.text = district_name
		label.billboard = BaseMaterial3D.BILLBOARD_ENABLED
		label.modulate = Color(0.7, 1.0, 0.8) if safe else Color(1.0, 0.7, 0.7)
		label.pixel_size = 0.05
		label.position = Protocol.w2v(x0 + w * 0.5, y0 + h * 0.5, 6.0)
		_tiles_root.add_child(label)
