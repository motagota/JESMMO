## A single item entry that can be picked up and dragged (Godot native drag/drop).
## Used by the inventory and storage panels: dragging one produces a payload the
## drop zones interpret as a deposit (from inventory) or withdraw (from storage).
## Rendered as a flat, text-like button so it reads as a label but is draggable;
## `mouse_filter = PASS` so a *drop* over it falls through to the enclosing
## `ItemDropZone`, while a *drag* still starts here.
class_name DraggableItem
extends Button

## Right-clicked (mining/abilities epic #123, #119): the inventory panel
## turns this into "arm this in the tool slot" for equippable items. Uses
## the Control base's `gui_input` SIGNAL (not the `_gui_input` virtual), so
## it coexists with Button's own native left-click/drag handling untouched.
signal right_clicked(item_id: String)

var item_id: String
var qty: int
var source: String # "inventory" | "storage"

func _init(p_item_id: String = "", p_qty: int = 0, p_source: String = "") -> void:
	item_id = p_item_id
	qty = p_qty
	source = p_source
	flat = true
	focus_mode = Control.FOCUS_NONE
	mouse_filter = Control.MOUSE_FILTER_PASS
	alignment = HORIZONTAL_ALIGNMENT_LEFT
	text = "≡ %s x%d" % [item_id, qty]
	tooltip_text = "Drag to move"
	gui_input.connect(_on_gui_input)

func _on_gui_input(event: InputEvent) -> void:
	if event is InputEventMouseButton and event.button_index == MOUSE_BUTTON_RIGHT and event.pressed:
		right_clicked.emit(item_id)

func _get_drag_data(_at_position: Vector2) -> Variant:
	if qty <= 0:
		return null
	var preview := Label.new()
	preview.text = "%s x%d" % [item_id, qty]
	preview.add_theme_color_override("font_color", Color(1, 1, 0.8))
	set_drag_preview(preview)
	return {"kind": "item", "item_id": item_id, "qty": qty, "source": source}
