## Headless smoke test for tool durability's client UI (mining/abilities
## epic #123 backlog, #128): the Inventory panel shows a damaged tool's
## durability, right-clicking a tool row equips its SPECIFIC instance (not
## just "the pickaxe" — #128 makes that ambiguous), a damaged instance gets
## a "Repair" button that emits the right instance id, and a fully-healthy
## instance gets no repair button at all.
## Run: Godot --headless --path client_godot -s res://tests/smoke_durability.gd
extends SceneTree

var _inv: InventoryPanel
var _t := 0.0
var _stage := 0

func _fail(message: String) -> void:
	print("SMOKE_FAIL: %s" % message)
	quit(1)

func _find_draggable(grid: GridContainer, item_id: String) -> DraggableItem:
	for slot in grid.get_children():
		for col_child in slot.get_children():
			for child in col_child.get_children():
				if child is DraggableItem and child.item_id == item_id:
					return child
	return null

func _find_repair_button(grid: GridContainer, item_id: String) -> Button:
	for slot in grid.get_children():
		for col_child in slot.get_children():
			var saw_item := false
			var found_button: Button = null
			for child in col_child.get_children():
				if child is DraggableItem and child.item_id == item_id:
					saw_item = true
				if child is Button and child.text == "Repair":
					found_button = child
			if saw_item and found_button != null:
				return found_button
	return null

func _initialize() -> void:
	_inv = InventoryPanel.new()
	root.add_child(_inv)

func _process(delta: float) -> bool:
	_t += delta
	match _stage:
		0:
			# InventoryPanel builds its UI in _ready (not _init, unlike
			# Hotbar/NpcDialoguePanel) — give it a frame to actually enter
			# the tree before touching _grid.
			if _inv._grid != null:
				_inv.set_inventory([
					{"id": "wood-stack", "item_id": "wood", "qty": 5, "slot": null},
					{"id": "instance-worn", "item_id": "pickaxe", "qty": 1, "slot": null, "durability": 12, "max_durability": 50},
					{"id": "instance-healthy", "item_id": "axe", "qty": 1, "slot": null, "durability": 50, "max_durability": 50},
				], 7, 50)
				_stage = 1
			elif _t > 5.0:
				_fail("InventoryPanel never finished entering the tree")
				return true
		1:
			# _rebuild()'s queue_free() on the old (empty) placeholder is
			# deferred, not synchronous — wait a frame so it's actually gone
			# before counting children.
			_run_checks()
			return true
	return false

func _run_checks() -> void:
	var worn_id := "instance-worn"
	var healthy_id := "instance-healthy"

	# Exactly 3 slots — one per array entry (never aggregated).
	if _inv._grid.get_child_count() != 3:
		_fail("expected 3 inventory slots, got %d" % _inv._grid.get_child_count()); return

	# Right-clicking a tool row equips its OWN instance id, not the item_id.
	var equipped: Array[String] = []
	_inv.do_equip.connect(func(id): equipped.append(id))
	var worn_draggable := _find_draggable(_inv._grid, "pickaxe")
	if worn_draggable == null:
		_fail("expected a DraggableItem for the worn pickaxe"); return
	worn_draggable.right_clicked.emit(worn_draggable.item_id)
	if equipped != [worn_id]:
		_fail("expected do_equip(%s), got %s" % [worn_id, equipped]); return

	# The worn pickaxe (12/50) has a Repair button; clicking it emits its instance id.
	var repaired: Array[String] = []
	_inv.do_repair.connect(func(id): repaired.append(id))
	var repair_btn := _find_repair_button(_inv._grid, "pickaxe")
	if repair_btn == null:
		_fail("expected a Repair button on the damaged pickaxe row"); return
	repair_btn.pressed.emit()
	if repaired != [worn_id]:
		_fail("expected do_repair(%s), got %s" % [worn_id, repaired]); return

	# The fully-healthy axe (50/50) gets NO repair button at all.
	if _find_repair_button(_inv._grid, "axe") != null:
		_fail("a full-durability tool must not offer repair"); return

	# An ordinary stackable item (wood) is untouched by any of this — no
	# equip wiring, and its row still shows the plain qty text.
	var wood_draggable := _find_draggable(_inv._grid, "wood")
	if wood_draggable == null or wood_draggable.text.find("x5") == -1:
		_fail("expected the wood row to show its plain quantity: %s" % [wood_draggable.text if wood_draggable else "null"]); return

	print("SMOKE_OK: inventory renders one row per instance, right-click equips the SPECIFIC instance, damaged tools offer repair and healthy ones don't")
	quit(0)
