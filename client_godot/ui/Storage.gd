## Storage panel: shown while the player stands near a storage point. Two columns —
## Carried and Stored — each a drag/drop zone: drag a carried item onto the Stored
## column (or drop it there) to **deposit**, and a stored item onto the Carried
## column to **withdraw**. Each row also keeps a Deposit/Withdraw button as a
## click fallback. Built in code; `Main` toggles visibility by proximity and feeds
## it the latest inventory + storage. Moves are whole-stack; the server bounds the
## amount (carry capacity for withdraw).
class_name StoragePanel
extends CanvasLayer

signal do_deposit(item_id: String, qty: int)
signal do_withdraw(item_id: String, qty: int)

var _carry_box: VBoxContainer
var _store_box: VBoxContainer
var _inventory: Array = []
var _storage: Array = []

func _ready() -> void:
	layer = 8
	var panel := PanelContainer.new()
	panel.position = Vector2(820, 360)
	panel.custom_minimum_size = Vector2(420, 0)
	add_child(panel)

	var cols := HBoxContainer.new()
	cols.add_theme_constant_override("separation", 24)
	panel.add_child(cols)

	# Carried column accepts dropped *storage* items (= withdraw); Stored column
	# accepts dropped *inventory* items (= deposit).
	_carry_box = _column(cols, "Carried  →  Deposit", "storage", do_withdraw)
	_store_box = _column(cols, "Stored  →  Withdraw", "inventory", do_deposit)
	_rebuild()

## Build a titled column that is also an `ItemDropZone`; returns the inner row box.
func _column(parent: Control, title: String, accept_source: String, drop_sig: Signal) -> VBoxContainer:
	var zone := ItemDropZone.new(accept_source)
	zone.custom_minimum_size = Vector2(190, 0)
	zone.item_dropped.connect(func(item_id, qty): drop_sig.emit(item_id, qty))
	parent.add_child(zone)

	var col := VBoxContainer.new()
	zone.add_child(col)
	var head := Label.new()
	head.text = title
	head.add_theme_font_size_override("font_size", 14)
	col.add_child(head)

	var rows := VBoxContainer.new()
	col.add_child(rows)
	return rows

func set_inventory(items: Array) -> void:
	_inventory = items
	_rebuild()

func set_storage(items: Array) -> void:
	_storage = items
	_rebuild()

func show_panel(p_show: bool) -> void:
	visible = p_show

func _rebuild() -> void:
	if not _carry_box:
		return
	_fill(_carry_box, _inventory, "Deposit", "inventory", do_deposit)
	_fill(_store_box, _storage, "Withdraw", "storage", do_withdraw)

func _fill(box: VBoxContainer, items: Array, verb: String, source: String, sig: Signal) -> void:
	for c in box.get_children():
		c.queue_free()
	if items.is_empty():
		var empty := Label.new()
		empty.text = "(empty)"
		empty.modulate = Color(0.6, 0.6, 0.6)
		box.add_child(empty)
		return
	for it_v in items:
		var it: Dictionary = it_v
		var item_id := String(it.get("item_id", "?"))
		var qty := int(it.get("qty", 0))
		var row := HBoxContainer.new()
		var di := DraggableItem.new(item_id, qty, source)
		di.custom_minimum_size = Vector2(120, 0)
		row.add_child(di)
		var btn := Button.new()
		btn.text = verb
		btn.pressed.connect(func(): sig.emit(item_id, qty))
		row.add_child(btn)
		box.add_child(row)
