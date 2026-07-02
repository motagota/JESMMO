## Build-order board panel: shown while the player stands near a build board. Lists
## the district's city build orders with their per-item cost as `progress/required`
## and a Contribute button per outstanding item. Built in code; `Main` toggles
## visibility by proximity and feeds it the latest orders (`build.list`), inventory,
## and the player's skill levels. Contribute moves the player's whole carried stack of
## that item toward the order; the server bounds it by the remaining need and what's
## carried. Orders the player can't yet build (skill gate) render greyed with a
## "requires <skill> <level>" note and no Contribute buttons — the server enforces the
## same gate, so this is UX, not trust.
class_name BuildPanel
extends CanvasLayer

signal do_contribute(order_id: String, item_id: String, qty: int)

var _list: VBoxContainer
var _orders: Array = []
var _inventory: Array = []
## skill_id -> level, for greying orders above the player's current skill.
var _skill_levels: Dictionary = {}

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
	head.text = "Build Orders  (contribute nearby)"
	head.add_theme_font_size_override("font_size", 14)
	col.add_child(head)

	_list = VBoxContainer.new()
	_list.add_theme_constant_override("separation", 10)
	col.add_child(_list)
	_rebuild()

func set_orders(orders: Array) -> void:
	_orders = orders
	_rebuild()

func set_inventory(items: Array) -> void:
	_inventory = items
	_rebuild()

## Feed the player's current skill levels (from `skill.update`) so gated orders grey
## out until the threshold is reached.
func set_skill_levels(levels: Dictionary) -> void:
	_skill_levels = levels
	_rebuild()

## Update one order's cost/progress in place (from a `build.progress` push).
func update_progress(order_id: String, required: Dictionary, progress: Dictionary) -> void:
	for o_v in _orders:
		var o: Dictionary = o_v
		if String(o.get("order_id", "")) == order_id:
			o["required"] = required
			o["progress"] = progress
			break
	_rebuild()

## Mark an order completed (from a `build.completed` push); a fresh `build.list`
## from the server will follow, but this keeps the panel responsive meanwhile.
func mark_completed(order_id: String) -> void:
	for o_v in _orders:
		var o: Dictionary = o_v
		if String(o.get("order_id", "")) == order_id:
			o["state"] = "completed"
			break
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
	if _orders.is_empty():
		var empty := Label.new()
		empty.text = "(no active orders)"
		empty.modulate = Color(0.6, 0.6, 0.6)
		_list.add_child(empty)
		return
	for o_v in _orders:
		var o: Dictionary = o_v
		var kind := String(o.get("kind", "?"))
		var state := String(o.get("state", "open"))
		var title := Label.new()
		title.add_theme_font_size_override("font_size", 13)
		if state == "completed":
			title.text = "✓ %s" % kind
			title.modulate = Color(0.6, 0.9, 0.6)
			_list.add_child(title)
			continue

		# Skill gate: an order requiring a skill level the player hasn't reached shows
		# greyed with a "requires …" note and offers no Contribute buttons.
		var rs: Variant = o.get("required_skill")
		var req_skill := String(rs) if rs != null else ""
		var req_level := int(o.get("required_level", 0))
		var locked := req_level > 0 and int(_skill_levels.get(req_skill, 0)) < req_level

		title.text = kind
		if locked:
			title.modulate = Color(0.55, 0.55, 0.6)
		_list.add_child(title)
		if locked:
			var req := Label.new()
			req.add_theme_font_size_override("font_size", 11)
			req.modulate = Color(0.95, 0.7, 0.4)
			req.text = "  requires %s %d" % [req_skill.capitalize(), req_level]
			_list.add_child(req)

		var order_id := String(o.get("order_id", ""))
		var required: Dictionary = o.get("required", {})
		var progress: Dictionary = o.get("progress", {})
		for item_v in required.keys():
			var item := String(item_v)
			var need := int(required.get(item, 0))
			var have := int(progress.get(item, 0))
			var row := HBoxContainer.new()
			var lbl := Label.new()
			lbl.text = "  %s  %d/%d" % [item, have, need]
			lbl.custom_minimum_size = Vector2(150, 0)
			if locked:
				lbl.modulate = Color(0.55, 0.55, 0.6)
			row.add_child(lbl)
			var carried := _carried(item)
			if not locked and have < need and carried > 0:
				var btn := Button.new()
				btn.text = "Contribute %d" % carried
				btn.pressed.connect(func(): do_contribute.emit(order_id, item, carried))
				row.add_child(btn)
			_list.add_child(row)
