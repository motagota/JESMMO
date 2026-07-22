## The ability hotbar (mining/abilities epic #123, #119/#120): 5 fixed slots
## on keys 1-5, filled from `equip.update`'s abilities list (Pick lands in
## slot 1 today; the rest sit empty until more abilities exist). Each slot
## shows a cooldown sweep — a `ProgressBar` draining from "just used" to
## "ready" — driven by `ability.result`'s level-scaled `cooldown_ms`, which
## is the same number the gateway will actually enforce.
##
## Auto-fire (#120): right-click a slot (or Shift+its number) arms a small
## ▶ marker — Main polls `ready_auto_ids()` every frame and re-presses an
## armed slot the instant it's off cooldown (and a target exists; that part
## is Main's call, this class knows nothing about the world). No wire
## changes — auto-fire is purely the client pressing the button for you;
## the server's cooldown ledger is the only real guard either way.
##
## Built in `_init`, not `_ready` — headless tests drive it before tree
## entry (the #79 rule, same as `EditorToolbar`).
class_name HotbarPanel
extends CanvasLayer

const SLOT_COUNT := 5
const _KEYS := [KEY_1, KEY_2, KEY_3, KEY_4, KEY_5]

## Fired when a slot with an armed ability is pressed (key or click) and
## isn't visibly on cooldown or already awaiting a result. Main resolves
## the target node and sends `ability.use`.
signal use_pressed(ability_id: String)

## One slot's live state: id/name/cooldown_ms from the last `set_abilities`;
## `ready_at_msec` (0 = ready) driven by `on_ability_result`; `pending`
## (set only via `mark_sent`, so a local no-target miss can never wedge a
## slot shut) blocks a second send before the first's result lands.
var _slots: Array[Dictionary] = []
var _auto: Array[bool] = []
var _buttons: Array[Button] = []
var _bars: Array[ProgressBar] = []
var _auto_labels: Array[Label] = []

func _init() -> void:
	layer = 6 # above the HUD (5), same band as vitals
	for i in range(SLOT_COUNT):
		_slots.append({"id": "", "name": "", "cooldown_ms": 0, "ready_at_msec": 0, "pending": false})
		_auto.append(false)

	var root := HBoxContainer.new()
	root.set_anchors_preset(Control.PRESET_CENTER_BOTTOM)
	root.offset_top = -84
	root.offset_bottom = -12
	root.offset_left = -170
	root.offset_right = 170
	root.add_theme_constant_override("separation", 6)
	add_child(root)

	for i in range(SLOT_COUNT):
		var slot := VBoxContainer.new()
		slot.custom_minimum_size = Vector2(56, 0)
		root.add_child(slot)

		var b := Button.new()
		b.custom_minimum_size = Vector2(56, 56)
		b.focus_mode = Control.FOCUS_NONE
		b.text = "%d" % (i + 1)
		var idx := i
		b.pressed.connect(func(): _try_use(idx))
		b.gui_input.connect(func(event): _on_slot_gui_input(idx, event))
		slot.add_child(b)
		_buttons.append(b)

		var auto_label := Label.new()
		auto_label.add_theme_font_size_override("font_size", 12)
		auto_label.add_theme_color_override("font_color", Color(0.5, 1.0, 0.6))
		auto_label.horizontal_alignment = HORIZONTAL_ALIGNMENT_CENTER
		auto_label.text = ""
		b.add_child(auto_label)
		auto_label.set_anchors_preset(Control.PRESET_TOP_RIGHT)
		auto_label.position = Vector2(38, 2)
		_auto_labels.append(auto_label)

		var bar := ProgressBar.new()
		bar.custom_minimum_size = Vector2(56, 6)
		bar.max_value = 100.0
		bar.value = 0.0
		bar.show_percentage = false
		slot.add_child(bar)
		_bars.append(bar)

	_redraw()

## Populate the hotbar from `equip.update`'s abilities list (empty on
## unequip). Slots beyond `abilities.size()` are cleared. A slot whose
## ability id changes drops any in-flight cooldown/pending/auto-arm —
## arming a different tool starts completely fresh.
func set_abilities(abilities: Array) -> void:
	for i in range(SLOT_COUNT):
		var want_id := ""
		var want_name := ""
		var want_cd := 0
		if i < abilities.size():
			var a: Dictionary = abilities[i]
			want_id = String(a.get("id", ""))
			want_name = String(a.get("name", want_id))
			want_cd = int(a.get("cooldown_ms", 0))
		var slot: Dictionary = _slots[i]
		if slot["id"] != want_id:
			slot["ready_at_msec"] = 0
			slot["pending"] = false
			_auto[i] = false
		slot["id"] = want_id
		slot["name"] = want_name
		slot["cooldown_ms"] = want_cd
	_redraw()

## A swing's outcome (mining/abilities epic #123, #117/#119). `pending`
## clears either way (success or rejection both resolve the wait); only a
## success actually starts the cooldown sweep — a `cooldown` rejection
## means one is already running and shouldn't be restarted.
func on_ability_result(ability_id: String, ok: bool, cooldown_ms: int) -> void:
	for slot in _slots:
		if slot["id"] == ability_id:
			slot["pending"] = false
			if ok:
				slot["ready_at_msec"] = Time.get_ticks_msec() + cooldown_ms
			return

## Mark a slot as having actually sent `ability.use` — called by Main only
## when a real request went out (never on a local no-target miss), so a
## slot can never get stuck waiting for a result that will never arrive.
func mark_sent(ability_id: String) -> void:
	for slot in _slots:
		if slot["id"] == ability_id:
			slot["pending"] = true
			return

## Ability ids of every auto-armed slot that's ready to fire again right
## now (off cooldown, not awaiting a prior result). Main polls this once a
## frame and attempts each one; whether a target actually exists is Main's
## call, not this class's.
func ready_auto_ids() -> Array[String]:
	var out: Array[String] = []
	for i in range(SLOT_COUNT):
		var slot: Dictionary = _slots[i]
		if _auto[i] and slot["id"] != "" and not slot["pending"] and _remaining_ms(slot) == 0:
			out.append(slot["id"])
	return out

func _unhandled_key_input(event: InputEvent) -> void:
	if not (event is InputEventKey and event.pressed and not event.echo):
		return
	var idx := _KEYS.find(event.keycode)
	if idx == -1:
		return
	if event.shift_pressed:
		_toggle_auto(idx)
	else:
		_try_use(idx)
	get_viewport().set_input_as_handled()

func _on_slot_gui_input(idx: int, event: InputEvent) -> void:
	if event is InputEventMouseButton and event.button_index == MOUSE_BUTTON_RIGHT and event.pressed:
		_toggle_auto(idx)

func _toggle_auto(idx: int) -> void:
	if idx < 0 or idx >= SLOT_COUNT:
		return
	if String(_slots[idx]["id"]) == "":
		return # nothing to arm
	_auto[idx] = not _auto[idx]
	_redraw()

func _try_use(idx: int) -> void:
	if idx < 0 or idx >= SLOT_COUNT:
		return
	var slot: Dictionary = _slots[idx]
	var id: String = slot["id"]
	if id == "" or slot["pending"] or _remaining_ms(slot) > 0:
		return # empty, already in flight, or visibly on cooldown
	use_pressed.emit(id)

func _remaining_ms(slot: Dictionary) -> int:
	return maxi(0, int(slot["ready_at_msec"]) - Time.get_ticks_msec())

func _process(_delta: float) -> void:
	for i in range(SLOT_COUNT):
		var slot: Dictionary = _slots[i]
		var cd: int = slot["cooldown_ms"]
		var remaining := _remaining_ms(slot)
		_bars[i].value = (100.0 * remaining / cd) if cd > 0 else 0.0
		_bars[i].visible = remaining > 0

func _redraw() -> void:
	for i in range(SLOT_COUNT):
		var slot: Dictionary = _slots[i]
		var id: String = slot["id"]
		var b := _buttons[i]
		if id == "":
			b.text = "%d" % (i + 1)
			b.tooltip_text = ""
			b.disabled = true
			_auto_labels[i].text = ""
		else:
			b.text = "%s\n[%d]" % [Protocol.ability_icon(id), i + 1]
			b.tooltip_text = "%s%s" % [String(slot["name"]), "  (auto-fire armed — right-click/Shift+%d to disarm)" % (i + 1) if _auto[i] else ""]
			b.disabled = false
			_auto_labels[i].text = "▶" if _auto[i] else ""
