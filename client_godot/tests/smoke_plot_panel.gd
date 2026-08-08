## The plot management panel (#185): policy, roster, and the recovery vault.
##
## What this guards is that the panel never DECIDES anything. Every mode, fee,
## grant and expiry it shows comes from the server; every button round-trips.
## The panel's only job is to render the truth and make the dangerous states
## impossible to miss.
##
## Run: Godot --headless --path client_godot -s res://tests/smoke_plot_panel.gd
extends SceneTree

var _panel: PlotPanel
var _failed := false

func _fail(msg: String) -> void:
	print("SMOKE_FAIL: ", msg)
	_failed = true
	quit(1)

func _text() -> String:
	var parts := PackedStringArray()
	for n in _panel.find_children("*", "Label", true, false):
		parts.append(String(n.text))
	for n in _panel.find_children("*", "Button", true, false):
		parts.append(String(n.text))
	return " | ".join(parts)

func _button(fragment: String) -> Button:
	for b in _panel.find_children("*", "Button", true, false):
		if String(b.text).findn(fragment) >= 0:
			return b
	return null

func _initialize() -> void:
	_panel = PlotPanel.new()
	root.add_child(_panel)
	_panel.visible = true

func _process(_delta: float) -> bool:
	# --- a healthy lease says so quietly -------------------------------------
	_panel.set_lease("p1", "active")
	_panel.set_policy("closed", 0, 0, 0.0, 0)
	var t := _text()
	if t.findn("good standing") < 0:
		_fail("a paid-up lease should say so: %s" % t)
		return true
	if t.findn("Closed — only you") < 0:
		_fail("the current mode should be spelled out: %s" % t)
		return true

	# --- the mode you are already on cannot be re-selected -------------------
	var closed_btn := _button("closed")
	if closed_btn == null or not closed_btn.disabled:
		_fail("the active mode should not be a live button")
		return true
	if _button("public") == null or _button("public").disabled:
		_fail("the other modes should be selectable")
		return true

	# --- a fee, and an in-kind share as "1 in N" -----------------------------
	# Rendering 0.25 would undo the only reason in-kind was chosen over a
	# percentage: that it needs no explanation (#182).
	_panel.set_policy("fee", 7, 3, 0.25, 50)
	t = _text()
	if t.findn("7g per job") < 0:
		_fail("the gold fee should be shown: %s" % t)
		return true
	if t.findn("1 in 4") < 0:
		_fail("the in-kind share should read as '1 in 4': %s" % t)
		return true
	if t.findn("up to 50g") < 0:
		_fail("...with its value ceiling: %s" % t)
		return true
	if t.find("0.25") >= 0:
		_fail("never as a decimal: %s" % t)
		return true
	if t.findn("Minimum skill 3") < 0:
		_fail("a skill floor should be visible: %s" % t)
		return true

	# --- the roster shows WHEN each grant runs out ---------------------------
	# An expiry nobody can see is an expiry that surprises people (#183).
	_panel.set_roster([
		{"character_id": "abcdef1234", "role": "worker", "days_left": 12, "expired": false},
		{"character_id": "99998888", "role": "patron", "days_left": 0, "expired": true},
	])
	t = _text()
	if t.findn("12 days left") < 0:
		_fail("a live grant should show its remaining days: %s" % t)
		return true
	if t.findn("EXPIRED") < 0:
		_fail("a lapsed grant should be shown as lapsed, not hidden: %s" % t)
		return true
	if t.findn("Roster (2)") < 0:
		_fail("the roster count should include the expired one: %s" % t)
		return true

	var removed := []
	_panel.do_revoke.connect(func(cid): removed.append(cid))
	var remove := _button("Remove")
	if remove == null:
		_fail("each grant should be removable")
		return true
	remove.pressed.emit()
	if removed.size() != 1:
		_fail("Remove should ask the server, not act locally: %s" % [removed])
		return true

	# --- an empty vault is not shown at all ----------------------------------
	_panel.set_vault([])
	if _text().findn("Recovery vault") >= 0:
		_fail("an empty vault is a reminder of nothing: %s" % _text())
		return true

	# --- a full one is loud, and says what happens if ignored ----------------
	# This is the one place the game DOES eventually destroy property, and only
	# after saying so (#184).
	_panel.set_vault([
		{"item": "stone", "qty": 40, "source": "station", "days_left": 6},
		{"item": "iron_ingot", "qty": 12, "source": "station", "days_left": 6},
	])
	t = _text()
	if t.findn("Recovery vault") < 0:
		_fail("a stocked vault should be shown: %s" % t)
		return true
	if t.findn("stone x40") < 0 or t.findn("iron_ingot x12") < 0:
		_fail("it should list what is waiting: %s" % t)
		return true
	if t.findn("6 days") < 0:
		_fail("...and how long is left: %s" % t)
		return true
	if t.findn("gone") < 0:
		_fail("it must say plainly that ignoring it destroys the goods: %s" % t)
		return true

	var claimed := []
	_panel.do_claim_vault.connect(func(): claimed.append(true))
	var claim := _button("Claim")
	if claim == null:
		_fail("the vault should be claimable")
		return true
	claim.pressed.emit()
	if claimed.size() != 1:
		_fail("Claim should round-trip to the server")
		return true

	# --- lapsed and derelict are impossible to miss --------------------------
	# A colour change is not enough for something that ends in losing the plot.
	_panel.set_lease("p1", "lapsed")
	t = _text()
	if t.findn("OVERDUE") < 0:
		_fail("a lapsed lease should shout: %s" % t)
		return true
	if t.findn("still run") < 0:
		_fail("...and say the stations are still working, because they are: %s" % t)
		return true

	_panel.set_lease("p1", "derelict")
	t = _text()
	if t.findn("DERELICT") < 0:
		_fail("a derelict lease should shout louder: %s" % t)
		return true
	if t.findn("STOPPED") < 0:
		_fail("...and say production has halted: %s" % t)
		return true
	if t.findn("recovery vault") < 0:
		_fail("...and that nothing will be destroyed: %s" % t)
		return true

	# --- two redraws in one frame draw one panel -----------------------------
	# `queue_free` is deferred; the station panel had exactly this bug.
	_panel.set_policy("fee", 7, 0, 0.0, 0)
	_panel.set_policy("fee", 7, 0, 0.0, 0)
	var heads := 0
	for n in _panel.find_children("*", "Label", true, false):
		if String(n.text).findn("Who may use your stations") >= 0:
			heads += 1
	if heads != 1:
		_fail("two pushes in one frame drew the panel %d times" % heads)
		return true

	print("SMOKE_OK: the plot panel renders the server's policy, roster and vault;",
		" an in-kind share reads as '1 in N' and never a decimal; grants show when",
		" they expire and expired ones are shown rather than hidden; an empty vault",
		" is absent and a stocked one says plainly that ignoring it destroys the",
		" goods; lapsed and derelict both shout; and two pushes draw one panel")
	quit(0)
	return true
