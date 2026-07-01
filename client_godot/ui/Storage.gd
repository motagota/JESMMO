## Storage panel: shown while the player stands near a storage point. Lists carried
## items (with a Deposit button each) and stored items (with a Withdraw button).
## Built in code; `Main` toggles visibility by proximity and feeds it the latest
## inventory + storage. Deposit/withdraw move the whole stack of that item; the
## server bounds the amount (carry capacity for withdraw).
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

	_carry_box = _column(cols, "Carried  →  Deposit")
	_store_box = _column(cols, "Stored  →  Withdraw")
	_rebuild()

func _column(parent: Control, title: String) -> VBoxContainer:
	var col := VBoxContainer.new()
	col.custom_minimum_size = Vector2(190, 0)
	var head := Label.new()
	head.text = title
	head.add_theme_font_size_override("font_size", 14)
	col.add_child(head)
	parent.add_child(col)
	return col

func set_inventory(items: Array) -> void:
	_inventory = items
	_rebuild()

func set_storage(items: Array) -> void:
	_storage = items
	_rebuild()

func show_panel(show: bool) -> void:
	visible = show

func _rebuild() -> void:
	if not _carry_box:
		return
	_fill(_carry_box, _inventory, "Deposit", do_deposit)
	_fill(_store_box, _storage, "Withdraw", do_withdraw)

func _fill(box: VBoxContainer, items: Array, verb: String, sig: Signal) -> void:
	# Clear previous rows (keep the header at index 0).
	for i in range(box.get_child_count() - 1, 0, -1):
		box.get_child(i).queue_free()
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
		var lbl := Label.new()
		lbl.text = "%s x%d" % [item_id, qty]
		lbl.custom_minimum_size = Vector2(110, 0)
		row.add_child(lbl)
		var btn := Button.new()
		btn.text = verb
		btn.pressed.connect(func(): sig.emit(item_id, qty))
		row.add_child(btn)
		box.add_child(row)
