## Heads-up display: connection/zone/position, the gathered inventory, the
## gathering skill, a transient gather-progress line, and floating "+N item"
## feedback. Built in code; driven by `Main`.
class_name Hud
extends CanvasLayer

var _status: Label
var _inv: Label
var _skill: Label
var _gather: Label
var _gain: Label

var conn := "connecting…"
var zone := "—"
var pos := Vector2.ZERO

var _gain_tween: Tween

func _ready() -> void:
	layer = 5
	var box := VBoxContainer.new()
	box.position = Vector2(12, 12)
	add_child(box)

	_status = _line(box)
	_inv = _line(box)
	_inv.text = "inventory: (empty)"
	_skill = _line(box)
	_skill.text = "gathering: Lv 0"
	_gather = _line(box)
	_gather.modulate = Color(0.8, 1.0, 0.6)

	# Floating gain feedback, centred-ish on screen.
	_gain = Label.new()
	_gain.add_theme_font_size_override("font_size", 28)
	_gain.position = Vector2(560, 360)
	_gain.modulate = Color(0.9, 1.0, 0.6, 0.0)
	add_child(_gain)

	_refresh_status()

func _line(parent: Control) -> Label:
	var l := Label.new()
	l.add_theme_font_size_override("font_size", 14)
	parent.add_child(l)
	return l

# --- status line --------------------------------------------------------------

func set_conn(text: String) -> void:
	conn = text
	_refresh_status()

func set_zone(text: String) -> void:
	zone = text
	_refresh_status()

func set_pos(wx: float, wy: float) -> void:
	pos = Vector2(wx, wy)
	_refresh_status()

func _refresh_status() -> void:
	if _status:
		_status.text = "%s   |   zone: %s   |   pos: (%d, %d)   |   [E] gather" % [
			conn, zone, int(round(pos.x)), int(round(pos.y))]

# --- gameplay -----------------------------------------------------------------

func set_inventory(items: Array) -> void:
	if items.is_empty():
		_inv.text = "inventory: (empty)"
		return
	var parts := PackedStringArray()
	for it_v in items:
		var it: Dictionary = it_v
		parts.append("%s x%d" % [String(it.get("item_id", "?")), int(it.get("qty", 0))])
	_inv.text = "inventory: " + ", ".join(parts)

func set_skill(skill_id: String, xp: int, level: int) -> void:
	_skill.text = "%s: Lv %d  (%d xp)" % [skill_id, level, xp]

func set_gather_progress(pct: int) -> void:
	if pct <= 0 or pct >= 100:
		_gather.text = ""
	else:
		_gather.text = "gathering… %d%%" % pct

func flash_gain(item_id: String, qty: int) -> void:
	_gather.text = ""
	_gain.text = "+%d %s" % [qty, item_id]
	_gain.modulate.a = 1.0
	if _gain_tween and _gain_tween.is_valid():
		_gain_tween.kill()
	_gain_tween = create_tween()
	_gain_tween.tween_property(_gain, "modulate:a", 0.0, 1.0)
