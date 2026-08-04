## The bounty offer on the client (#161): where you stand, and a claim button
## that only appears when claiming would actually work.
##
## Run: Godot --headless --path client_godot -s res://tests/smoke_bounty.gd
extends SceneTree

var _panel: NpcDialoguePanel
var _claims := 0

func _fail(msg: String) -> void:
	print("SMOKE_FAIL: ", msg)
	quit(1)

func _text() -> String:
	var parts := PackedStringArray()
	for n in _panel.find_children("*", "Label", true, false):
		if n.visible:
			parts.append(String(n.text))
	return " | ".join(parts)

func _buttons() -> Array:
	var out := []
	for n in _panel.find_children("*", "Button", true, false):
		if n.visible:
			out.append(String(n.text))
	return out

func _initialize() -> void:
	_panel = NpcDialoguePanel.new()
	root.add_child(_panel)
	_panel.do_bounty_turn_in.connect(func(): _claims += 1)

func _process(_delta: float) -> bool:
	_panel.show_dialogue("Bram", ["Bring me ten pelts."], false)

	# --- short of the count: progress shown, no button ------------------------
	# Offering an action the server is going to refuse is worse than not
	# offering it, and the label already says how many more are needed.
	_panel.set_bounty("dog_pelt", 10, 100, 7)
	var short := _text()
	if not short.contains("10 dog_pelt") or not short.contains("100g"):
		_fail("the offer should state the terms, got: %s" % short); return true
	if not short.contains("you have 7") or not short.contains("3 more"):
		_fail("progress should be explicit, got: %s" % short); return true
	if _buttons().any(func(b): return b.contains("Hand over")):
		_fail("a claim button appeared while short of the count"); return true

	# --- enough: the button appears ------------------------------------------
	_panel.set_bounty("dog_pelt", 10, 100, 10)
	if not _buttons().any(func(b): return b.contains("Hand over")):
		_fail("no claim button with a full count, got: %s" % [_buttons()]); return true
	if _text().contains("more)"):
		_fail("shouldn't still be asking for more, got: %s" % _text()); return true

	# --- claiming is a deliberate act, and only fires on the press -----------
	if _claims != 0:
		_fail("the bounty was claimed without anyone pressing anything"); return true
	for b in _panel.find_children("*", "Button", true, false):
		if String(b.text).contains("Hand over"):
			b.pressed.emit()
	if _claims != 1:
		_fail("pressing the button should claim exactly once, got %d" % _claims); return true

	# --- after paying, it drops back to progress -----------------------------
	_panel.set_bounty("dog_pelt", 10, 100, 0)
	if _buttons().any(func(b): return b.contains("Hand over")):
		_fail("the claim button survived spending the pelts"); return true
	if not _text().contains("10 more"):
		_fail("should show the full gap again, got: %s" % _text()); return true

	# --- an NPC who pays nothing shows no offer at all -----------------------
	_panel.clear_bounty()
	var none := _text()
	if none.contains("Bounty"):
		_fail("an ordinary NPC shouldn't advertise a bounty, got: %s" % none); return true
	if _buttons().any(func(b): return b.contains("Hand over")):
		_fail("an ordinary NPC offered a claim button"); return true

	print("SMOKE_OK: the bounty shows progress, offers a claim only when it would ",
		"succeed, claims once per press, and stays hidden for NPCs who pay none")
	quit(0)
	return true
