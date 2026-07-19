## The editor's toolbar (#103): visible, clickable tool selection for
## `--editor-mode`, and the SINGLE owner of tool exclusivity — it replaces
## Main's pairwise `set_enabled` cross-wiring (O(n²) in tools, already
## awkward at three, unmanageable at the five #102 ends with).
##
## The tools keep their existing contracts untouched: ObjectTool still
## cycles off/place/delete on [O], RoadTool still toggles on [R], the brush
## still paints whenever it's enabled. The toolbar listens to their
## `mode_changed` signals (so hotkey-driven switches light the right
## button) and, on a button click, drives the same `set_mode`/`set_active`
## methods the hotkeys use — clicks and keys converge on one `_apply_state`
## that computes the whole enabled matrix:
##   - the brush is enabled exactly while no pointed tool is active;
##   - each pointed tool's hotkey is live only while the other is off
##     (existing behaviour — buttons, unlike hotkeys, can always switch).
##
## A one-line contextual hint under the buttons shows the active tool's
## controls, and the tools' `status_changed` streams replace their old
## `flash_announce` toasts here — persistent, not scrolled-away.
##
## Built in `_init`, not `_ready` — headless tests drive it before tree
## entry (the #79 rule).
class_name EditorToolbar
extends CanvasLayer

## Fired after every state change with the active tool id
## ("brush" | "objects" | "road") — tests and future consumers.
signal tool_changed(active: String)

var active := "brush"

var _brush: BrushController
var _object_tool: ObjectTool
var _road_tool: RoadTool
var _demolish_tool: DemolishTool
var _history: HistoryPanel

var _buttons: Dictionary = {} # id -> Button
var _hint: Label
var _applying := false

func _init() -> void:
	layer = 7 # above the HUD (5) and vitals (6)
	var root_box := VBoxContainer.new()
	root_box.set_anchors_preset(Control.PRESET_CENTER_TOP)
	root_box.offset_top = 8
	root_box.offset_left = -320
	root_box.offset_right = 320
	add_child(root_box)

	var bar := PanelContainer.new()
	var style := StyleBoxFlat.new()
	style.bg_color = Color(0.10, 0.10, 0.13, 0.85)
	style.set_corner_radius_all(6)
	style.content_margin_left = 8
	style.content_margin_right = 8
	style.content_margin_top = 4
	style.content_margin_bottom = 4
	bar.add_theme_stylebox_override("panel", style)
	root_box.add_child(bar)
	var row := HBoxContainer.new()
	row.add_theme_constant_override("separation", 6)
	bar.add_child(row)

	for id_label in [["brush", "🖌 Brush"], ["objects", "🌳 Objects  [O]"], ["road", "🛣 Road  [R]"], ["road_move", "🔀 Move  [M]"], ["demolish", "🔨 Demolish  [X]"]]:
		var b := Button.new()
		b.text = id_label[1]
		b.toggle_mode = true
		b.focus_mode = Control.FOCUS_NONE
		var id: String = id_label[0]
		b.pressed.connect(func(): select(id))
		row.add_child(b)
		_buttons[id] = b

	# History is a panel toggle, not a mouse tool — outside the exclusivity.
	var h := Button.new()
	h.text = "🕑 History  [H]"
	h.focus_mode = Control.FOCUS_NONE
	h.pressed.connect(func():
		if _history != null:
			_history.visible = not _history.visible)
	row.add_child(h)

	_hint = Label.new()
	_hint.horizontal_alignment = HORIZONTAL_ALIGNMENT_CENTER
	_hint.add_theme_font_size_override("font_size", 13)
	_hint.add_theme_color_override("font_color", Color(0.9, 0.9, 0.8))
	_hint.add_theme_color_override("font_outline_color", Color(0, 0, 0, 0.8))
	_hint.add_theme_constant_override("outline_size", 4)
	root_box.add_child(_hint)

## Wire the tools this toolbar governs. Their `status_changed` streams feed
## the hint line from here on (Main routes them), and their mode signals
## keep the buttons honest when hotkeys drive the switch.
func setup(brush: BrushController, object_tool: ObjectTool, road_tool: RoadTool, demolish_tool: DemolishTool, history: HistoryPanel) -> void:
	_brush = brush
	_object_tool = object_tool
	_road_tool = road_tool
	_demolish_tool = demolish_tool
	_history = history
	_object_tool.mode_changed.connect(func(_m):
		if not _applying:
			_apply_state())
	_road_tool.mode_changed.connect(func(_a):
		if not _applying:
			_apply_state())
	_demolish_tool.mode_changed.connect(func(_a):
		if not _applying:
			_apply_state())
	_apply_state()

## A button click: drive the tools to the requested state through their own
## methods (the hotkey code paths), then recompute the matrix.
func select(id: String) -> void:
	if _brush == null:
		return
	_applying = true
	match id:
		"brush":
			_object_tool.set_mode("off")
			_road_tool.set_active(false)
			_demolish_tool.set_active(false)
		"objects":
			_road_tool.set_active(false)
			_demolish_tool.set_active(false)
			if _object_tool.mode == "off":
				_object_tool.set_mode("place")
		"road":
			_object_tool.set_mode("off")
			_demolish_tool.set_active(false)
			_road_tool.set_active(true) # also switches move mode -> lay
		"road_move":
			_object_tool.set_mode("off")
			_demolish_tool.set_active(false)
			_road_tool.set_move_active(true)
		"demolish":
			_object_tool.set_mode("off")
			_road_tool.set_active(false)
			_demolish_tool.set_active(true)
	_applying = false
	_apply_state()

## Show a tool's live status in the hint line (Main routes the tools'
## `status_changed` here — persistent, unlike the old announce toasts).
func set_hint(text: String) -> void:
	if _hint != null:
		_hint.text = text

## The one place the enabled matrix is computed. Idempotent — safe to call
## after any tool state change, from clicks and hotkey signals alike.
func _apply_state() -> void:
	var road_on := _road_tool.active
	var objects_on := _object_tool.mode != "off"
	var demo_on := _demolish_tool.active
	_brush.set_enabled(not road_on and not objects_on and not demo_on)
	# A pointed tool's hotkey stays dead while another pointed tool owns
	# the mouse (pre-toolbar behaviour); buttons can always switch.
	_object_tool.enabled = not road_on and not demo_on
	_road_tool.enabled = not objects_on and not demo_on
	_demolish_tool.enabled = not objects_on and not road_on
	if demo_on:
		active = "demolish"
	elif road_on:
		active = "road_move" if _road_tool.move_mode else "road"
	else:
		active = "objects" if objects_on else "brush"
	for id in _buttons:
		_buttons[id].set_pressed_no_signal(id == active)
	match active:
		"brush":
			set_hint("Brush — LMB raise, Shift+LMB lower, [ ] radius, -/= strength, Ctrl+Z undo")
		"objects":
			set_hint("Objects (%s) — [O] cycles place/delete/off" % _object_tool.mode)
		"road":
			set_hint("Road — click to anchor/corner, [Enter] commit, [Esc] cancel, [Backspace] undo corner")
		"road_move":
			set_hint("Move — click a staked plan to pick it up, edit, [Enter] commit, [Esc] drop")
		"demolish":
			set_hint("Demolish — click a plan or built road, click again to confirm (stone refunds via the salvage job)")
	tool_changed.emit(active)
