## Station panel (mine epic #164, issue #167): shown while the player stands at
## a crafting station — the furnace in the mine yard, and whatever #168 adds.
##
## Standing here is not what authorises anything. The server range-checks every
## station command against its own position cache (`station_at` in proxy.rs), so
## this panel is a view, never a permission.
##
## EVERYTHING DRAWN HERE COMES FROM `station.state`. The recipe list, the fuel
## rates, the fee, the per-recipe duration with the player's own skill already
## applied — none of it is mirrored client-side. That is the lesson #155 learned
## the hard way with market fees: a client-side copy of a server-side rule
## becomes a lie the moment the config is edited, and it lies convincingly.
##
## The one thing the client does own is the COUNTDOWN. A running job's remaining
## time is animated locally from the `ready_at` the server sent, because asking
## the server every frame for a number it can already predict would be a
## needless round trip. The server still decides when a job is actually done —
## the bar reaching the end is a picture, not an event.
class_name StationPanel
extends CanvasLayer

## Feed the fire, start a job, take what's finished.
signal do_load_fuel(item_id: String, qty: int)
signal do_start(recipe_id: String)
signal do_collect(job_id: String)

var _title: Label
var _fuel_line: Label
var _fuel_bar: ProgressBar
var _body: VBoxContainer
var _fuel_row: HBoxContainer
var _note: Label

## The last `station.state`, as sent. Kept whole rather than unpacked so a new
## server field shows up here without a client change first.
var _state: Dictionary = {}
## Carried inventory, for deciding whether a Load button can do anything.
var _inventory: Dictionary = {}
## Cleared on the next state push; holds the last error or collection message.
var _note_text := ""

## How full the fuel gauge reads at its maximum. The server has no cap on the
## buffer — you can pour in as much charcoal as you own — so this is purely how
## much the bar can show before it pins, chosen to be several jobs' worth.
const FUEL_GAUGE_MAX := 20.0


func _ready() -> void:
    layer = 6
    var panel := PanelContainer.new()
    panel.set_anchors_preset(Control.PRESET_CENTER_RIGHT)
    panel.position = Vector2(-460, -220)
    panel.custom_minimum_size = Vector2(440, 440)
    add_child(panel)

    var root := VBoxContainer.new()
    root.add_theme_constant_override("separation", 8)
    panel.add_child(root)

    _title = Label.new()
    _title.text = "Station"
    _title.add_theme_font_size_override("font_size", 20)
    root.add_child(_title)

    _fuel_line = Label.new()
    root.add_child(_fuel_line)

    _fuel_bar = ProgressBar.new()
    _fuel_bar.max_value = FUEL_GAUGE_MAX
    _fuel_bar.custom_minimum_size = Vector2(0, 14)
    _fuel_bar.show_percentage = false
    root.add_child(_fuel_bar)

    _fuel_row = HBoxContainer.new()
    root.add_child(_fuel_row)

    _note = Label.new()
    _note.autowrap_mode = TextServer.AUTOWRAP_WORD_SMART
    _note.custom_minimum_size = Vector2(420, 0)
    root.add_child(_note)

    var sep := HSeparator.new()
    root.add_child(sep)

    _body = VBoxContainer.new()
    _body.add_theme_constant_override("separation", 6)
    root.add_child(_body)


## The server's whole view of this station. Replaces everything.
func set_state(state: Dictionary) -> void:
    _state = state
    _redraw()


func set_inventory(items: Array) -> void:
    _inventory.clear()
    for it in items:
        var id := String(it.get("item_id", ""))
        _inventory[id] = int(_inventory.get(id, 0)) + int(it.get("qty", 0))
    _redraw()


## Show a one-line message until the next state push replaces it.
func note(text: String) -> void:
    _note_text = text
    if _note != null:
        _note.text = text


func station_id() -> String:
    return String(_state.get("station_id", ""))


func _held(item_id: String) -> int:
    return int(_inventory.get(item_id, 0))


func _redraw() -> void:
    if _title == null or _state.is_empty():
        return
    var fee := int(_state.get("usage_fee_gold", 0))
    _title.text = String(_state.get("name", "Station"))
    if fee > 0:
        _title.text += "  (%dg per job)" % fee

    var fuel := int(_state.get("fuel_units", 0))
    var is_heat := String(_state.get("kind", "")) == "heat"
    _fuel_bar.visible = is_heat
    _fuel_line.visible = is_heat
    _fuel_row.visible = is_heat
    if not is_heat:
        _fuel_line.text = ""
    if is_heat:
        _fuel_bar.value = min(float(fuel), FUEL_GAUGE_MAX)
        # Naming the shared-ness in the UI, because a player who loads fuel and
        # watches a stranger burn it should have been told, not surprised.
        _fuel_line.text = "🔥 Fuel: %d units  (shared — this is a public fire)" % fuel
        _rebuild_fuel_row()

    _note.text = _note_text

    # `queue_free` alone is deferred, so the old rows are still children until
    # the end of the frame — a second `set_state` in the same frame would draw
    # the panel twice over. Detaching first makes the redraw immediate.
    for c in _body.get_children():
        _body.remove_child(c)
        c.queue_free()

    var jobs: Array = _state.get("jobs", [])
    var slots := int(_state.get("job_slots", 1))
    var by_slot := {}
    for j in jobs:
        by_slot[int(j.get("slot", 0))] = j

    var slots_label := Label.new()
    slots_label.text = "Your slots (%d/%d in use)" % [jobs.size(), slots]
    _body.add_child(slots_label)
    for i in range(slots):
        _body.add_child(_slot_row(i, by_slot.get(i, {})))

    var sep := HSeparator.new()
    _body.add_child(sep)

    var recipes: Array = _state.get("recipes", [])
    if recipes.is_empty():
        var none := Label.new()
        none.text = "Nothing can be made here."
        _body.add_child(none)
        return
    for r in recipes:
        _body.add_child(_recipe_row(r, fuel, jobs.size() >= slots))


## One "Load N charcoal" button per accepted fuel.
func _rebuild_fuel_row() -> void:
    for c in _fuel_row.get_children():
        _fuel_row.remove_child(c)
        c.queue_free()
    for f in _state.get("fuels", []):
        var item := String(f.get("item", ""))
        var per := int(f.get("units", 1))
        var have := _held(item)
        var b := Button.new()
        b.text = "%s Load %s (%d)" % [Protocol.item_icon(item), item, have]
        b.tooltip_text = "Each is worth %d fuel units" % per
        b.disabled = have <= 0
        b.pressed.connect(func(): do_load_fuel.emit(item, have))
        _fuel_row.add_child(b)


func _slot_row(index: int, job: Dictionary) -> Control:
    var row := HBoxContainer.new()
    var label := Label.new()
    label.custom_minimum_size = Vector2(250, 0)
    row.add_child(label)

    if job.is_empty():
        label.text = "  Slot %d — empty" % (index + 1)
        return row

    var state := String(job.get("state", "running"))
    var out := String(job.get("output_item", ""))
    var qty := int(job.get("output_qty", 1))
    if state == "failed":
        # A refund with no explanation reads as a bug, so the reason is shown
        # and the button says what it will actually hand back.
        var reason := String(job.get("fail_reason", "")) if job.get("fail_reason") != null else ""
        label.text = "  Slot %d — %s" % [index + 1, Protocol.job_fail_text(reason)]
        var b := Button.new()
        b.text = "Take materials back"
        b.pressed.connect(func(): do_collect.emit(String(job.get("id", ""))))
        row.add_child(b)
        return row

    if state == "ready":
        label.text = "  Slot %d — %s %s x%d ready" % [index + 1, Protocol.item_icon(out), out, qty]
        var b := Button.new()
        b.text = "Collect"
        b.pressed.connect(func(): do_collect.emit(String(job.get("id", ""))))
        row.add_child(b)
        return row

    var left := int(job.get("remaining_secs", 0))
    label.text = "  Slot %d — %s %s x%d  (%ds)" % [index + 1, Protocol.item_icon(out), out, qty, left]
    var bar := ProgressBar.new()
    bar.custom_minimum_size = Vector2(120, 12)
    bar.show_percentage = false
    var started := int(job.get("started_at", 0))
    var ready_at := int(job.get("ready_at", 0))
    var span: int = max(ready_at - started, 1)
    bar.value = clampf(float(span - left) / float(span) * 100.0, 0.0, 100.0)
    row.add_child(bar)
    return row


func _recipe_row(r: Dictionary, fuel: int, slots_full: bool) -> Control:
    var row := HBoxContainer.new()
    var out := String(r.get("output_item", ""))
    var need_fuel := int(r.get("fuel_units", 0))

    var parts: Array[String] = []
    var short := false
    for i in r.get("inputs", []):
        var item := String(i.get("item", ""))
        var need := int(i.get("qty", 0))
        var have := _held(item)
        if have < need:
            short = true
        parts.append("%d %s (%d)" % [need, item, have])

    var label := Label.new()
    label.custom_minimum_size = Vector2(280, 0)
    label.text = "%s %s — %s, %.0fs" % [
        Protocol.item_icon(out),
        String(r.get("name", "")),
        ", ".join(parts),
        float(r.get("duration_ms", 0)) / 1000.0,
    ]
    if need_fuel > 0:
        label.text += ", %d fuel" % need_fuel
    row.add_child(label)

    var b := Button.new()
    b.text = "Start"
    # Disabled reasons are spelled out rather than left to a greyed button. A
    # button that does nothing and says nothing is the worst of both.
    var locked := bool(r.get("locked", false))
    if locked:
        b.disabled = true
        b.tooltip_text = "Needs %s level %d" % [String(r.get("skill", "")), int(r.get("required_level", 1))]
    elif slots_full:
        b.disabled = true
        b.tooltip_text = "All your slots are busy"
    elif short:
        b.disabled = true
        b.tooltip_text = "You don't have the materials"
    elif fuel < need_fuel:
        b.disabled = true
        b.tooltip_text = "The fire needs %d fuel and has %d" % [need_fuel, fuel]
    else:
        b.pressed.connect(func(): do_start.emit(String(r.get("id", ""))))
    row.add_child(b)
    return row


## Tick the countdown locally between server pushes, so a running job's bar
## moves smoothly instead of stepping once a second when a state arrives.
func _process(_delta: float) -> void:
    if not visible or _state.is_empty():
        return
    var now := int(Time.get_unix_time_from_system())
    var changed := false
    for j in _state.get("jobs", []):
        if String(j.get("state", "")) != "running":
            continue
        var left: int = max(int(j.get("ready_at", 0)) - now, 0)
        if left != int(j.get("remaining_secs", -1)):
            j["remaining_secs"] = left
            changed = true
    if changed:
        _redraw()
