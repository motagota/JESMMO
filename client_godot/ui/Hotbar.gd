## The ability hotbar (mining/abilities epic #123, #119): 5 fixed slots on
## keys 1-5, filled from `equip.update`'s abilities list (Pick lands in slot
## 1 today; the rest sit empty until more abilities exist). Each slot shows
## a cooldown sweep — a `ProgressBar` draining from "just used" to "ready" —
## driven by `ability.result`'s level-scaled `cooldown_ms`, which is the
## same number the gateway will actually enforce.
##
## Built in `_init`, not `_ready` — headless tests drive it before tree
## entry (the #79 rule, same as `EditorToolbar`).
class_name HotbarPanel
extends CanvasLayer

const SLOT_COUNT := 5
const _KEYS := [KEY_1, KEY_2, KEY_3, KEY_4, KEY_5]

## Fired when a slot with an armed ability is pressed (key or click) and
## isn't visibly on cooldown. Main resolves the target node and sends
## `ability.use`.
signal use_pressed(ability_id: String)

## One slot's live state: id/name/cooldown_ms from the last `set_abilities`,
## plus `ready_at_msec` (0 = ready) driven by `on_ability_result`.
var _slots: Array[Dictionary] = []
var _buttons: Array[Button] = []
var _bars: Array[ProgressBar] = []

func _init() -> void:
	layer = 6 # above the HUD (5), same band as vitals
	for i in range(SLOT_COUNT):
		_slots.append({"id": "", "name": "", "cooldown_ms": 0, "ready_at_msec": 0})

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
		slot.add_child(b)
		_buttons.append(b)

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
## ability id changes drops any in-flight cooldown — arming a different
## tool starts fresh.
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
		slot["id"] = want_id
		slot["name"] = want_name
		slot["cooldown_ms"] = want_cd
	_redraw()

## A swing's outcome (mining/abilities epic #123, #117/#119): only a
## success starts the sweep — a `cooldown` rejection means one is already
## running and shouldn't be restarted; other failures never began a swing.
func on_ability_result(ability_id: String, ok: bool, cooldown_ms: int) -> void:
	if not ok:
		return
	for slot in _slots:
		if slot["id"] == ability_id:
			slot["ready_at_msec"] = Time.get_ticks_msec() + cooldown_ms
			return

func _unhandled_key_input(event: InputEvent) -> void:
	if not (event is InputEventKey and event.pressed and not event.echo):
		return
	var idx := _KEYS.find(event.keycode)
	if idx == -1:
		return
	_try_use(idx)
	get_viewport().set_input_as_handled()

func _try_use(idx: int) -> void:
	if idx < 0 or idx >= SLOT_COUNT:
		return
	var slot: Dictionary = _slots[idx]
	var id: String = slot["id"]
	if id == "" or _remaining_ms(slot) > 0:
		return # empty slot, or visibly on cooldown — nothing to send yet
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
		else:
			b.text = "%s\n[%d]" % [Protocol.ability_icon(id), i + 1]
			b.tooltip_text = "%s" % String(slot["name"])
			b.disabled = false
