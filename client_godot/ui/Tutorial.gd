## The tutorial track (mine epic #164, issue #169).
##
## A CHECKLIST, NOT A QUEST LOG. Every step here completes by doing the thing —
## none of them completes by talking to anyone, and none of them gates anything.
## A player who ignores this panel entirely can mine, smelt, throw pots and sell
## them exactly as well as one who follows it; the only thing they miss is the
## handful of clay at the end.
##
## The whole state is server-computed. This panel never decides that a step is
## done, because the server has been counting since the character's first login
## and the client has not — which is exactly what lets a step the player
## finished before ever meeting Marlow arrive already ticked.
##
## It hides itself once the track is complete. A finished checklist is clutter,
## and leaving it on screen would make a thing that was never mandatory look
## like furniture the player is stuck with.
class_name TutorialPanel
extends CanvasLayer

var _title: Label
var _body: VBoxContainer
var _steps: Array = []
var _done := 0
var _total := 0
## Set once the reward has landed, so the panel can say so before retiring.
var _finished_note := ""


func _ready() -> void:
	layer = 5
	var panel := PanelContainer.new()
	panel.set_anchors_preset(Control.PRESET_TOP_RIGHT)
	panel.position = Vector2(-330, 16)
	panel.custom_minimum_size = Vector2(310, 0)
	add_child(panel)

	var root := VBoxContainer.new()
	root.add_theme_constant_override("separation", 4)
	panel.add_child(root)

	_title = Label.new()
	_title.add_theme_font_size_override("font_size", 15)
	root.add_child(_title)

	_body = VBoxContainer.new()
	_body.add_theme_constant_override("separation", 2)
	root.add_child(_body)

	visible = false


## The server's whole view of the track. Replaces everything.
func set_track(steps: Array, done: int, total: int) -> void:
	_steps = steps
	_done = done
	_total = total
	_redraw()


## The reward landed. Shown briefly, then the panel retires itself.
func note_complete(item: String, qty: int) -> void:
	_finished_note = "Marlow leaves you %d %s." % [qty, item]
	_redraw()


func is_complete() -> bool:
	return _total > 0 and _done >= _total


func _redraw() -> void:
	if _title == null:
		return
	if _steps.is_empty():
		visible = false
		return

	# Retire once finished. The reward line gets one redraw to be seen; after
	# that a completed checklist is just clutter.
	if is_complete() and _finished_note == "":
		visible = false
		return

	visible = true
	_title.text = "Kedron Cut  (%d/%d)" % [_done, _total]

	# `queue_free` is deferred, so detach first or a second push in the same
	# frame draws the list twice over.
	for c in _body.get_children():
		_body.remove_child(c)
		c.queue_free()

	# Only the next unfinished step is shown in full, with the finished ones
	# collapsed to ticks. A seven-line wall of instructions is something a
	# player dismisses; one line is something they read.
	var shown_next := false
	for s in _steps:
		var row := Label.new()
		var done := bool(s.get("done", false))
		if done:
			row.text = "  ✓ %s" % String(s.get("text", ""))
			row.modulate = Color(1, 1, 1, 0.45)
		elif not shown_next:
			row.text = "  ▸ %s" % String(s.get("text", ""))
			shown_next = true
		else:
			# Later steps stay listed but unelaborated, so the shape of what is
			# coming is visible without competing with what to do now.
			row.text = "  ·"
			row.modulate = Color(1, 1, 1, 0.3)
		row.autowrap_mode = TextServer.AUTOWRAP_WORD_SMART
		row.custom_minimum_size = Vector2(290, 0)
		_body.add_child(row)

	if _finished_note != "":
		var note := Label.new()
		note.text = _finished_note
		note.autowrap_mode = TextServer.AUTOWRAP_WORD_SMART
		note.custom_minimum_size = Vector2(290, 0)
		_body.add_child(note)
