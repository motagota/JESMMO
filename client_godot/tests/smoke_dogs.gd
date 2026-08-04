## Wild dogs on the client (epic #157, issue #158): a dog arrives as a mob
## carrying a species, renders as its own creature rather than another anonymous
## blob, and can be picked out of a crowd — which is what a bounty (#161) will
## need to point a player at something to fight.
##
## Run: Godot --headless --path client_godot -s res://tests/smoke_dogs.gd
extends SceneTree

const HERE := Vector2(12100, 12400)

var _entities: EntityManager

func _fail(msg: String) -> void:
	print("SMOKE_FAIL: ", msg)
	quit(1)

func _mob(at: Vector2, species: String) -> Dictionary:
	var d := {"x": at.x, "y": at.y, "type": "mob", "hp": 40, "max_hp": 40, "facing": [0, 0]}
	if species != "":
		d["species"] = species
	return d

func _initialize() -> void:
	_entities = EntityManager.new()
	root.add_child(_entities)

func _process(_delta: float) -> bool:
	# --- species arrives and is stored ---------------------------------------
	_entities.upsert("mob_dog_0", "zone_a", _mob(HERE, "wild_dog"))
	_entities.upsert("mob_ambient", "zone_a", _mob(HERE + Vector2(20, 0), ""))

	if _entities.nearest_creature(HERE, 100.0, "wild_dog") != "mob_dog_0":
		_fail("a wild dog wasn't findable by species"); return true
	# The anonymous mob standing right next to it must NOT be mistaken for one.
	if _entities.nearest_creature(HERE + Vector2(20, 0), 5.0, "wild_dog") != "":
		_fail("an ambient mob was matched as a wild dog"); return true
	# Nor does an unknown species match anything.
	if _entities.nearest_creature(HERE, 100.0, "bear") != "":
		_fail("matched a species that doesn't exist"); return true

	# --- range is respected, same as every other proximity lookup ------------
	if _entities.nearest_creature(HERE + Vector2(500, 0), 100.0, "wild_dog") != "":
		_fail("found a dog from far outside the range"); return true

	# --- the nearer of two is chosen ----------------------------------------
	_entities.upsert("mob_dog_1", "zone_a", _mob(HERE + Vector2(60, 0), "wild_dog"))
	if _entities.nearest_creature(HERE, 200.0, "wild_dog") != "mob_dog_0":
		_fail("picked the further dog"); return true
	if _entities.nearest_creature(HERE + Vector2(60, 0), 200.0, "wild_dog") != "mob_dog_1":
		_fail("picked the further dog from the other side"); return true

	# --- a dog looks like a dog ---------------------------------------------
	# Distinguishable from the ambient mob it stands beside: its own mesh, its
	# own colour, and a name. A bounty target you can't pick out of a crowd
	# isn't a target.
	var dog_rec: Dictionary = _entities._entities.get("mob_dog_0", {})
	var mob_rec: Dictionary = _entities._entities.get("mob_ambient", {})
	var dog_node = dog_rec.get("node")
	var mob_node = mob_rec.get("node")
	if dog_node == null or mob_node == null:
		_fail("expected both creatures to have rendered nodes"); return true
	if dog_node.mesh.size == mob_node.mesh.size:
		_fail("a wild dog renders identically to an anonymous mob"); return true
	var labelled := false
	for child in dog_node.get_children():
		if child is Label3D and String(child.text).contains("Wild Dog"):
			labelled = true
	if not labelled:
		_fail("a wild dog should be named so it can be picked out"); return true
	for child in mob_node.get_children():
		if child is Label3D:
			_fail("an ambient mob should stay anonymous"); return true

	# --- despawn clears it ---------------------------------------------------
	_entities.remove("mob_dog_0")
	if _entities.nearest_creature(HERE, 200.0, "wild_dog") != "mob_dog_1":
		_fail("a killed dog stayed findable"); return true

	print("SMOKE_OK: wild dogs carry a species, are findable by it, are never confused with ",
		"anonymous mobs, and render as their own named creature")
	quit(0)
	return true
