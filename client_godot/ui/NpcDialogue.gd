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
## The bounty offer (#161): where you are against it, and the button to claim.
## Hidden entirely for an NPC who pays no bounty, so an ordinary foreman's
## dialogue is exactly what it always was.
var _bounty_label: Label
var _bounty_button: Button
var _bounty_ready := false

## The player asked to hand in trophies. Main sends it; the server re-checks
## range and the count, so this is a request, not a claim.
signal do_bounty_turn_in

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

	_bounty_label = Label.new()
	_bounty_label.add_theme_font_size_override("font_size", 13)
	_bounty_label.modulate = Color(1.0, 0.9, 0.6)
	_bounty_label.visible = false
	col.add_child(_bounty_label)

	_bounty_button = Button.new()
	_bounty_button.text = "Hand over the pelts"
	_bounty_button.focus_mode = Control.FOCUS_NONE
	_bounty_button.visible = false
	_bounty_button.pressed.connect(func(): do_bounty_turn_in.emit())
	col.add_child(_bounty_button)

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

## Where the player stands against the bounty (#161). Called whenever the server
## reports it — on arrival and after every turn-in — so the count is never the
## client's own arithmetic.
##
## The button appears only when the turn-in would actually succeed: offering an
## action the server is going to refuse is worse than not offering it, and the
## label already says how many more are needed.
func set_bounty(item_id: String, required: int, gold: int, held: int) -> void:
	_bounty_ready = held >= required and required > 0
	_bounty_label.visible = true
	_bounty_button.visible = _bounty_ready
	if _bounty_ready:
		_bounty_label.text = "Bounty: %d %s for %dg — you have %d" % [
			required, item_id, gold, held]
	else:
		_bounty_label.text = "Bounty: %d %s for %dg — you have %d (%d more)" % [
			required, item_id, gold, held, required - held]

## Hide the bounty offer entirely, for an NPC who pays none.
func clear_bounty() -> void:
	_bounty_label.visible = false
	_bounty_button.visible = false

func close() -> void:
	visible = false
