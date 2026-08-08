## The tutorial track panel (#169).
##
## What this guards is that the panel is a VIEW and never a judge. Every "done"
## comes from the server, which has been counting since the character's first
## login; the client has not, and a panel that decided for itself would disagree
## the moment a player did something before opening it.
##
## Run: Godot --headless --path client_godot -s res://tests/smoke_tutorial.gd
extends SceneTree

var _panel: TutorialPanel
var _failed := false

func _fail(msg: String) -> void:
	print("SMOKE_FAIL: ", msg)
	_failed = true
	quit(1)

func _text() -> String:
	var parts := PackedStringArray()
	for n in _panel.find_children("*", "Label", true, false):
		parts.append(String(n.text))
	return " | ".join(parts)

func _steps(done_upto: int) -> Array:
	var out := []
	var names := ["Get a pickaxe", "Mine 4 clay", "Mine 4 iron", "Load the furnace"]
	for i in range(names.size()):
		out.append({"id": "s%d" % i, "text": names[i], "done": i < done_upto})
	return out

func _initialize() -> void:
	_panel = TutorialPanel.new()
	root.add_child(_panel)

func _process(_delta: float) -> bool:
	# --- nothing yet: the panel stays out of the way ------------------------
	if _panel.visible:
		_fail("the panel should be hidden before any track arrives")
		return true

	# --- a fresh track shows progress and the NEXT step in full -------------
	_panel.set_track(_steps(0), 0, 4)
	var t := _text()
	if not _panel.visible:
		_fail("a track should show the panel")
		return true
	if t.findn("0/4") < 0:
		_fail("progress should be visible: %s" % t)
		return true
	if t.findn("▸ Get a pickaxe") < 0:
		_fail("the next step should be spelled out: %s" % t)
		return true
	if t.findn("Mine 4 clay") >= 0:
		_fail("later steps should not compete with the current one: %s" % t)
		return true

	# --- steps arriving already ticked is the whole point -------------------
	# A player who mined clay before ever meeting Marlow must see it done, not
	# be asked to do it again.
	_panel.set_track(_steps(2), 2, 4)
	t = _text()
	if t.findn("✓ Get a pickaxe") < 0 or t.findn("✓ Mine 4 clay") < 0:
		_fail("completed steps should be ticked: %s" % t)
		return true
	if t.findn("▸ Mine 4 iron") < 0:
		_fail("the next incomplete step should now be the one spelled out: %s" % t)
		return true
	if t.findn("2/4") < 0:
		_fail("the count should follow: %s" % t)
		return true

	# --- a completed track retires itself -----------------------------------
	# It was never mandatory; leaving a finished checklist on screen would make
	# it look like furniture the player is stuck with.
	_panel.set_track(_steps(4), 4, 4)
	if _panel.visible:
		_fail("a finished track should hide itself")
		return true
	if not _panel.is_complete():
		_fail("...but it should know it finished")
		return true

	# --- the reward is announced before it retires --------------------------
	_panel.note_complete("clay_lump", 6)
	if not _panel.visible:
		_fail("the reward line should be shown before the panel goes")
		return true
	if _text().findn("6 clay_lump") < 0:
		_fail("and it should say what was given: %s" % _text())
		return true

	# --- a redraw in the same frame must not double the list ----------------
	# `queue_free` is deferred, so this is a real hazard rather than a theoretical
	# one — the station panel had exactly this bug.
	_panel.set_track(_steps(1), 1, 4)
	_panel.set_track(_steps(1), 1, 4)
	var ticks := 0
	for n in _panel.find_children("*", "Label", true, false):
		if String(n.text).findn("Get a pickaxe") >= 0:
			ticks += 1
	if ticks != 1:
		_fail("two pushes in one frame drew the list %d times" % ticks)
		return true

	# --- an empty track is not a panel --------------------------------------
	_panel.set_track([], 0, 0)
	if _panel.visible:
		_fail("no steps means no panel — a world without tutorial.toml is playable")
		return true

	print("SMOKE_OK: the track shows progress and only the next step in full;",
		" steps that arrive already done are ticked rather than re-asked;",
		" a finished track announces its reward and then retires; two pushes in",
		" one frame draw one list; and an empty track shows nothing at all")
	quit(0)
	return true
