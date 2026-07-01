## A drop target for `DraggableItem`s. Accepts only items whose `source` matches
## `accept_source` (so a storage column accepts inventory items = deposit, and an
## inventory area accepts storage items = withdraw), and re-emits the drop as a
## typed signal the panel wires to `do_deposit` / `do_withdraw`.
class_name ItemDropZone
extends PanelContainer

signal item_dropped(item_id: String, qty: int)

## The `DraggableItem.source` this zone accepts (the *other* container's items).
var accept_source := ""

func _init(p_accept_source: String = "") -> void:
	accept_source = p_accept_source
	mouse_filter = Control.MOUSE_FILTER_STOP

func _can_drop_data(_at_position: Vector2, data: Variant) -> bool:
	return data is Dictionary \
		and data.get("kind", "") == "item" \
		and String(data.get("source", "")) == accept_source \
		and int(data.get("qty", 0)) > 0

func _drop_data(_at_position: Vector2, data: Variant) -> void:
	item_dropped.emit(String(data.get("item_id", "")), int(data.get("qty", 0)))
