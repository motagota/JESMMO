## Dedicated carried-inventory panel: a slot grid of the items the player carries,
## with the carry usage in its header. Toggled with the **I** key, and auto-shown
## while standing near a storage point (so items can be dragged straight into the
## storage panel). Each slot is a `DraggableItem` (source `inventory`); the panel is
## itself an `ItemDropZone` that accepts `storage` items — dropping one withdraws it.
class_name InventoryPanel
extends CanvasLayer

signal do_withdraw(item_id: String, qty: int)

const COLS := 5

var _zone: ItemDropZone
var _grid: GridContainer
var _title: Label
var _items: Array = []
var _used := 0
var _capacity := 0

var _pinned := false      # toggled by the I key
var _forced := false      # forced open while near storage

func _ready() -> void:
	layer = 8
	_zone = ItemDropZone.new("storage")
	_zone.custom_minimum_size = Vector2(300, 0)
	_zone.position = Vector2(40, 360)
	_zone.item_dropped.connect(func(item_id, qty): do_withdraw.emit(item_id, qty))
	add_child(_zone)

	var col := VBoxContainer.new()
	col.add_theme_constant_override("separation", 6)
	_zone.add_child(col)

	_title = Label.new()
	_title.add_theme_font_size_override("font_size", 14)
	col.add_child(_title)

	_grid = GridContainer.new()
	_grid.columns = COLS
	_grid.add_theme_constant_override("h_separation", 6)
	_grid.add_theme_constant_override("v_separation", 6)
	col.add_child(_grid)
	_rebuild()

func set_inventory(items: Array, used: int, capacity: int) -> void:
	_items = items
	_used = used
	_capacity = capacity
	_rebuild()

## Force the panel visible (e.g. while near storage), independent of the I toggle.
func set_forced_open(v: bool) -> void:
	if _forced != v:
		_forced = v
		_apply_visibility()

func _unhandled_key_input(event: InputEvent) -> void:
	if event is InputEventKey and event.pressed and not event.echo and event.keycode == KEY_I:
		_pinned = not _pinned
		_apply_visibility()
		get_viewport().set_input_as_handled()

func _apply_visibility() -> void:
	visible = _pinned or _forced

func _rebuild() -> void:
	if not _grid:
		return
	_title.text = "Inventory  [%d/%d]   (I)" % [_used, _capacity]
	for c in _grid.get_children():
		c.queue_free()
	if _items.is_empty():
		var empty := Label.new()
		empty.text = "(empty)"
		empty.modulate = Color(0.6, 0.6, 0.6)
		_grid.add_child(empty)
		return
	for it_v in _items:
		var it: Dictionary = it_v
		var item_id := String(it.get("item_id", "?"))
		var qty := int(it.get("qty", 0))
		var slot := PanelContainer.new()
		slot.custom_minimum_size = Vector2(54, 54)
		var di := DraggableItem.new(item_id, qty, "inventory")
		di.text = "%s\nx%d" % [item_id, qty]
		di.autowrap_mode = TextServer.AUTOWRAP_WORD_SMART
		slot.add_child(di)
		_grid.add_child(slot)
