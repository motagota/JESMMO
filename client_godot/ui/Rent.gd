## Rent panel: plot status (paid-through, due date, state), a Pay Rent button,
## and an auto-pay checkbox. Unlike Storage/Craft (proximity-gated — you're
## either at the fixture or not), rent is an ongoing background concern
## independent of location, so `Main` toggles this with a keypress (P) rather
## than proximity. Built in code; driven by `rent.status` pushes (login, and
## after any pay/auto-pay/ticker-driven change) (#14).
class_name RentPanel
extends CanvasLayer

signal do_pay(plot_id: String)
signal do_set_autopay(plot_id: String, enabled: bool)

var _status_label: Label
var _gold_label: Label
var _pay_button: Button
var _autopay_box: CheckBox

var _plot_id := ""
var _due_at := 0
var _paid_through := 0
var _state := "active"
var _auto_pay := false
var _gold := 0
## Suppresses the checkbox's own `toggled` signal while we set it programmatically
## from a server push, so it doesn't loop back as a spurious `do_set_autopay`.
var _applying_status := false

func _ready() -> void:
    layer = 8
    visible = false
    var panel := PanelContainer.new()
    panel.position = Vector2(360, 360)
    panel.custom_minimum_size = Vector2(320, 0)
    add_child(panel)

    var col := VBoxContainer.new()
    col.add_theme_constant_override("separation", 8)
    panel.add_child(col)

    var head := Label.new()
    head.text = "Plot & Rent  [P] close"
    head.add_theme_font_size_override("font_size", 14)
    col.add_child(head)

    _status_label = Label.new()
    _status_label.add_theme_font_size_override("font_size", 13)
    col.add_child(_status_label)

    _gold_label = Label.new()
    _gold_label.add_theme_font_size_override("font_size", 13)
    col.add_child(_gold_label)

    var row := HBoxContainer.new()
    col.add_child(row)
    _pay_button = Button.new()
    _pay_button.text = "Pay Rent"
    _pay_button.pressed.connect(func(): do_pay.emit(_plot_id))
    row.add_child(_pay_button)

    _autopay_box = CheckBox.new()
    _autopay_box.text = "Auto-pay"
    _autopay_box.toggled.connect(func(enabled: bool):
        if not _applying_status:
            do_set_autopay.emit(_plot_id, enabled))
    row.add_child(_autopay_box)

    _refresh()

func show_panel(p_show: bool) -> void:
    visible = p_show

func set_status(plot_id: String, due_at: int, paid_through: int, state: String, auto_pay: bool, gold: int) -> void:
    _plot_id = plot_id
    _due_at = due_at
    _paid_through = paid_through
    _state = state
    _gold = gold
    _applying_status = true
    _auto_pay = auto_pay
    _autopay_box.button_pressed = auto_pay
    _applying_status = false
    _refresh()

func _refresh() -> void:
    if not _status_label:
        return
    if _plot_id == "":
        _status_label.text = "(no plot yet)"
        _gold_label.text = ""
        _pay_button.disabled = true
        return
    var now := int(Time.get_unix_time_from_system())
    var state_text := _state.capitalize()
    var when := ""
    if _state == "reclaimed":
        when = "— your plot was reclaimed"
    elif now < _due_at:
        when = "due in " + _format_span(_due_at - now)
    else:
        when = "overdue by " + _format_span(now - _due_at)
    _status_label.text = "Plot %s: %s (%s)" % [_plot_id.left(8), state_text, when]
    _gold_label.text = "Gold: %d   (rent costs gold each period)" % _gold
    _pay_button.disabled = _state == "reclaimed"

## A rough "Nd Nh" (or "Nh" under a day) span for a countdown/overdue readout.
func _format_span(secs: int) -> String:
    var days := secs / 86400
    var hours := (secs % 86400) / 3600
    if days > 0:
        return "%dd %dh" % [days, hours]
    return "%dh" % maxi(hours, 0)
