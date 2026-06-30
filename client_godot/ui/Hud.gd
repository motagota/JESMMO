## Minimal heads-up display: connection state, current district/zone, and the
## local player's predicted position. Built in code; updated by `Main`.
class_name Hud
extends CanvasLayer

var _label: Label

var conn := "connecting…"
var zone := "—"
var pos := Vector2.ZERO

func _ready() -> void:
	layer = 5
	var panel := PanelContainer.new()
	panel.position = Vector2(12, 12)
	add_child(panel)
	_label = Label.new()
	_label.add_theme_font_size_override("font_size", 14)
	panel.add_child(_label)
	_refresh()

func set_conn(text: String) -> void:
	conn = text
	_refresh()

func set_zone(text: String) -> void:
	zone = text
	_refresh()

func set_pos(wx: float, wy: float) -> void:
	pos = Vector2(wx, wy)
	_refresh()

func _refresh() -> void:
	if _label:
		_label.text = "%s   |   zone: %s   |   pos: (%d, %d)" % [
			conn, zone, int(round(pos.x)), int(round(pos.y))]
