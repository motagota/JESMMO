## Headless smoke test for the vitals HUD (#89): visibility rules (bars hide
## until the first server vitals; breath shows in water and while refilling;
## poison shows with any buildup and locks on at the proc with the screen
## tint), fill ratios track the server values exactly, and the death overlay
## shows on you_died. Built in _init, so this drives it before tree entry —
## the #79 gotcha this component is designed around.
## Run: Godot --headless --path client_godot -s res://tests/smoke_vitals.gd
extends SceneTree

func _fail(message: String) -> void:
	print("SMOKE_FAIL: %s" % message)
	quit(1)

func _initialize() -> void:
	var v := VitalsHud.new()
	# Deliberately exercise it BEFORE add_child — must not crash (#79).
	if v._stack.visible:
		_fail("bars must stay hidden until the first vitals arrive"); return
	v.set_vitals(100, 100, 200, 200, false, 0, 100, false)
	root.add_child(v)

	# Healthy on land: hp only.
	if not v._stack.visible or not v._hp_row.visible:
		_fail("hp bar should show after the first vitals"); return
	if v._breath_row.visible or v._poison_row.visible or v._poison_tint.visible:
		_fail("breath/poison must be tucked away when full/clean"); return
	if v._hp_label.text != "HP 100 / 100":
		_fail("hp label wrong: %s" % v._hp_label.text); return

	# Swimming: breath bar out, fill tracks the server ratio.
	v.set_vitals(100, 100, 50, 200, true, 0, 100, false)
	if not v._breath_row.visible:
		_fail("breath must show while submerged"); return
	var want := (260.0 - 4.0) * 0.25
	if absf(v._breath_fill.size.x - want) > 0.5:
		_fail("breath fill should be 1/4 (got %f, want %f)" % [v._breath_fill.size.x, want]); return
	if v._breath_label.text != "Breath":
		_fail("breath label wrong: %s" % v._breath_label.text); return

	# Out of breath underwater: the label goes urgent.
	v.set_vitals(40, 100, 0, 200, true, 0, 100, false)
	if v._breath_label.text != "DROWNING":
		_fail("empty breath underwater should read DROWNING"); return

	# Surfaced but refilling: still visible, marked as recovering.
	v.set_vitals(40, 100, 120, 200, false, 0, 100, false)
	if not v._breath_row.visible or v._breath_label.text != "Breath (recovering)":
		_fail("refilling breath should linger as recovering"); return
	# Fully recovered: tucks away.
	v.set_vitals(40, 100, 200, 200, false, 0, 100, false)
	if v._breath_row.visible:
		_fail("full breath on land should hide the meter"); return

	# Poison buildup at the forest edge: gauge out, no tint yet.
	v.set_vitals(100, 100, 200, 200, false, 60, 100, false)
	if not v._poison_row.visible or v._poison_tint.visible:
		_fail("buildup shows the gauge without the proc tint"); return
	if v._poison_label.text != "Poison":
		_fail("pre-proc poison label wrong: %s" % v._poison_label.text); return

	# The proc: locked full, warning label, screen tint on.
	v.set_vitals(80, 100, 200, 200, false, 100, 100, true)
	if not v._poison_tint.visible or v._poison_label.text != "POISONED":
		_fail("the proc must be unmissable (tint + POISONED)"); return
	if absf(v._poison_fill.size.x - (260.0 - 4.0)) > 0.5:
		_fail("procced poison gauge should lock to full"); return

	# Death: overlay on; a fresh respawn status clears the poison state.
	v.show_death()
	if not v._death_overlay.visible or v._death_overlay.color.a < 0.5:
		_fail("you_died must show the overlay"); return
	v.set_vitals(100, 100, 200, 200, false, 0, 100, false)
	if v._poison_tint.visible or v._poison_row.visible:
		_fail("the respawn's clean vitals must clear the poison treatment"); return

	print("SMOKE_OK: vitals visibility rules, server-ratio fills, drowning/recovering labels, proc tint lock, and the death overlay all behave")
	quit(0)
