## The "load curtain" for gated district transitions (#15): a brief full-screen
## fade the client shows itself the moment it detects (from its own knowledge of
## the district tiles) that the player crossed a district gate. The actual
## position/zone handoff already happened via the ordinary seamless
## migrate-request path (#4) — this is purely cosmetic pacing, so it waits for
## *both* a minimum duration (an instant round-trip shouldn't just flash) and
## the gateway's `district.ready` ack (refreshed district-scoped content, e.g.
## the build board) before dropping.
class_name DistrictTransition
extends CanvasLayer

var _curtain: ColorRect
var _label: Label

var _waiting_for_min_time := false
var _waiting_for_server := false

func _ready() -> void:
	layer = 20
	_curtain = ColorRect.new()
	_curtain.color = Color(0.02, 0.02, 0.03, 0.0)
	_curtain.set_anchors_preset(Control.PRESET_FULL_RECT)
	_curtain.mouse_filter = Control.MOUSE_FILTER_IGNORE
	add_child(_curtain)

	_label = Label.new()
	_label.add_theme_font_size_override("font_size", 22)
	_label.set_anchors_preset(Control.PRESET_CENTER)
	_label.horizontal_alignment = HORIZONTAL_ALIGNMENT_CENTER
	_label.modulate = Color(1, 1, 1, 0.0)
	add_child(_label)

## Show the curtain for entering `district_name`; drops once both the minimum
## duration has elapsed and the gateway has acked `district.ready`.
func begin(district_name: String) -> void:
	_label.text = "Entering %s…" % district_name.capitalize()
	_curtain.color.a = 1.0
	_label.modulate.a = 1.0
	_waiting_for_min_time = true
	_waiting_for_server = true
	await get_tree().create_timer(Protocol.DISTRICT_TRANSITION_MIN_SECS).timeout
	_waiting_for_min_time = false
	_maybe_finish()

## The gateway confirmed district-scoped content is refreshed.
func mark_server_ready() -> void:
	_waiting_for_server = false
	_maybe_finish()

func _maybe_finish() -> void:
	if _waiting_for_min_time or _waiting_for_server:
		return
	var tween := create_tween()
	tween.set_parallel(true)
	tween.tween_property(_curtain, "color:a", 0.0, 0.25)
	tween.tween_property(_label, "modulate:a", 0.0, 0.25)
