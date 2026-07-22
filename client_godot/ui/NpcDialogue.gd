## The NPC dialogue panel (mining/abilities epic #123, #121): a simple
## bottom-third text box shown on `npc.dialogue` — speaker name, lines, and
## (when the talk granted something) a mention of what. Closed by [E] —
## gated in `Main`, since E is the same key that opens a talk and a
## dialogue-aware Main must swallow that press as "close" instead of firing
## another interaction — or a click anywhere on the panel.
##
## Built in `_init`, not `_ready` — headless tests drive it before tree
## entry (the #79 rule, same as `Hotbar`/`EditorToolbar`).
class_name NpcDialoguePanel
extends CanvasLayer

var _name_label: Label
var _lines_label: Label

func _init() -> void:
	layer = 9 # above the hotbar (6) and HUD (5) — never hidden mid-conversation
	visible = false

	var panel := PanelContainer.new()
	panel.set_anchors_preset(Control.PRESET_CENTER_BOTTOM)
	panel.offset_left = -320
	panel.offset_right = 320
	panel.offset_top = -220
	panel.offset_bottom = -100
	var style := StyleBoxFlat.new()
	style.bg_color = Color(0.08, 0.08, 0.11, 0.92)
	style.set_corner_radius_all(8)
	style.content_margin_left = 16
	style.content_margin_right = 16
	style.content_margin_top = 12
	style.content_margin_bottom = 12
	panel.add_theme_stylebox_override("panel", style)
	panel.mouse_filter = Control.MOUSE_FILTER_STOP
	panel.gui_input.connect(func(event):
		if event is InputEventMouseButton and event.pressed:
			close())
	add_child(panel)

	var col := VBoxContainer.new()
	col.add_theme_constant_override("separation", 6)
	panel.add_child(col)

	_name_label = Label.new()
	_name_label.add_theme_font_size_override("font_size", 18)
	_name_label.add_theme_color_override("font_color", Color(1.0, 0.9, 0.6))
	col.add_child(_name_label)

	_lines_label = Label.new()
	_lines_label.add_theme_font_size_override("font_size", 15)
	_lines_label.autowrap_mode = TextServer.AUTOWRAP_WORD_SMART
	col.add_child(_lines_label)

	var hint := Label.new()
	hint.add_theme_font_size_override("font_size", 12)
	hint.modulate = Color(0.7, 0.7, 0.75)
	hint.text = "[E] / click to close"
	col.add_child(hint)

## Show `npc_name`'s reply. `granted` adds a small "+1 pickaxe" mention —
## the actual inventory gain feedback (the HUD's flash) is a separate call
## Main makes alongside this one.
func show_dialogue(npc_name: String, lines: Array, granted: bool) -> void:
	_name_label.text = npc_name
	var parts := PackedStringArray()
	for l in lines:
		parts.append(String(l))
	var text := "\n".join(parts)
	if granted:
		text += "\n\n+1 pickaxe"
	_lines_label.text = text
	visible = true

func close() -> void:
	visible = false
