## Crafting station panel: shown while the player stands near a crafting
## structure. Lists the static recipe registry (`craft.list`/`craft.recipes`)
## with a Craft button per recipe the player can currently afford. Built in
## code; `Main` toggles visibility by proximity and feeds it the registry and
## inventory. The server re-validates ownership of a crafting station and the
## ingredients — this is UX, not trust (#12).
class_name CraftPanel
extends CanvasLayer

signal do_craft(recipe_id: String)

var _list: VBoxContainer
var _recipes: Array = []
var _inventory: Array = []

func _ready() -> void:
	layer = 8
	var panel := PanelContainer.new()
	panel.position = Vector2(360, 360)
	panel.custom_minimum_size = Vector2(320, 0)
	add_child(panel)

	var col := VBoxContainer.new()
	col.add_theme_constant_override("separation", 8)
	panel.add_child(col)

	var head := Label.new()
	head.text = "Crafting Station"
	head.add_theme_font_size_override("font_size", 14)
	col.add_child(head)

	_list = VBoxContainer.new()
	_list.add_theme_constant_override("separation", 10)
	col.add_child(_list)
	_rebuild()

func set_recipes(recipes: Array) -> void:
	_recipes = recipes
	_rebuild()

func set_inventory(items: Array) -> void:
	_inventory = items
	_rebuild()

func show_panel(show: bool) -> void:
	visible = show

func _carried(item_id: String) -> int:
	for it_v in _inventory:
		var it: Dictionary = it_v
		if String(it.get("item_id", "")) == item_id:
			return int(it.get("qty", 0))
	return 0

func _rebuild() -> void:
	if not _list:
		return
	for c in _list.get_children():
		c.queue_free()
	if _recipes.is_empty():
		var empty := Label.new()
		empty.text = "(no recipes)"
		empty.modulate = Color(0.6, 0.6, 0.6)
		_list.add_child(empty)
		return
	for r_v in _recipes:
		var r: Dictionary = r_v
		var recipe_id := String(r.get("id", ""))
		var inputs: Array = r.get("inputs", [])
		var can_afford := true
		var cost_parts := PackedStringArray()
		for input_v in inputs:
			var input: Dictionary = input_v
			var item := String(input.get("item_id", ""))
			var need := int(input.get("qty", 0))
			var have := _carried(item)
			if have < need:
				can_afford = false
			cost_parts.append("%s %d/%d" % [item, have, need])

		var row := HBoxContainer.new()
		var lbl := Label.new()
		lbl.text = "%s  (%s)" % [String(r.get("name", recipe_id)), ", ".join(cost_parts)]
		lbl.custom_minimum_size = Vector2(220, 0)
		if not can_afford:
			lbl.modulate = Color(0.55, 0.55, 0.6)
		row.add_child(lbl)
		if can_afford:
			var btn := Button.new()
			btn.text = "Craft"
			btn.pressed.connect(func(): do_craft.emit(recipe_id))
			row.add_child(btn)
		_list.add_child(row)
