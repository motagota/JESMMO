## Headless smoke test for the ability hotbar (mining/abilities epic #123,
## #119/#120): populating slots from an `equip.update`-shaped abilities
## list, key/click presses emitting exactly one `use_pressed` per ready
## slot, the cooldown gate blocking a re-press until `ability.result` says
## it's ready again, re-arming resetting any stale cooldown, the pending
## lock preventing a double-send while a result is in flight, and
## auto-fire (`ready_auto_ids`) only offering armed/ready/non-pending slots.
## Run: Godot --headless --path client_godot -s res://tests/smoke_hotbar.gd
extends SceneTree

func _fail(message: String) -> void:
	print("SMOKE_FAIL: %s" % message)
	quit(1)

func _initialize() -> void:
	var bar := HotbarPanel.new()
	root.add_child(bar)

	var pressed: Array[String] = []
	bar.use_pressed.connect(func(id): pressed.append(id))

	# Empty hotbar: every slot press is a no-op.
	bar._try_use(0)
	if not pressed.is_empty():
		_fail("an empty slot should never emit use_pressed"); return

	# Arm Pick in slot 0 (mirrors equip.update's abilities list).
	bar.set_abilities([{"id": "pick", "name": "Pick", "cooldown_ms": 2000}])
	bar._try_use(0)
	if pressed != ["pick"]:
		_fail("expected exactly one 'pick' press, got %s" % [pressed]); return

	# Other slots stay empty.
	bar._try_use(1)
	if pressed != ["pick"]:
		_fail("slot 1 should still be empty"); return

	# A successful result starts the cooldown; a re-press while it's
	# running must not fire again (the client-side gate matches what the
	# server will reject anyway, but shouldn't even round-trip for it).
	bar.on_ability_result("pick", true, 2000)
	bar._try_use(0)
	if pressed != ["pick"]:
		_fail("a press mid-cooldown must not emit use_pressed"); return

	# A rejection (e.g. out_of_range) must NOT start/restart a cooldown —
	# only a successful swing does.
	var bar2 := HotbarPanel.new()
	root.add_child(bar2)
	bar2.set_abilities([{"id": "pick", "name": "Pick", "cooldown_ms": 2000}])
	bar2.on_ability_result("pick", false, 2000)
	var pressed2: Array[String] = []
	bar2.use_pressed.connect(func(id): pressed2.append(id))
	bar2._try_use(0)
	if pressed2 != ["pick"]:
		_fail("a failed swing must not have started a cooldown"); return

	# Re-arming (unequip then equip again) drops any stale cooldown —
	# picking the pick back up shouldn't inherit the old swing's timer.
	bar.set_abilities([])
	bar.set_abilities([{"id": "pick", "name": "Pick", "cooldown_ms": 2000}])
	bar._try_use(0)
	if pressed != ["pick", "pick"]:
		_fail("re-arming should reset the cooldown, expected a second press, got %s" % [pressed]); return

	# --- pending lock (#120): `_try_use` alone never sets the lock — only
	# `mark_sent` does, and Main only calls that when a real request went
	# out. So a local no-target miss (Main declines to send) must NOT wedge
	# the slot shut; only an ACTUAL send blocks a re-press until its result
	# (`on_ability_result`) lands.
	var bar3 := HotbarPanel.new()
	root.add_child(bar3)
	bar3.set_abilities([{"id": "pick", "name": "Pick", "cooldown_ms": 2000}])
	var pressed3: Array[String] = []
	bar3.use_pressed.connect(func(id): pressed3.append(id))
	bar3._try_use(0)
	if pressed3 != ["pick"]:
		_fail("expected the first press to fire: %s" % [pressed3]); return
	bar3._try_use(0) # no mark_sent happened (models a local no-target miss) — must fire again.
	if pressed3 != ["pick", "pick"]:
		_fail("without mark_sent, repeated presses must keep firing: %s" % [pressed3]); return
	# Now simulate Main actually sending: mark_sent locks it until the result lands.
	bar3.mark_sent("pick")
	bar3._try_use(0)
	if pressed3 != ["pick", "pick"]:
		_fail("mark_sent should block a re-press until the result lands: %s" % [pressed3]); return
	bar3.on_ability_result("pick", true, 2000)
	bar3._try_use(0) # now on cooldown from the confirmed success — still blocked.
	if pressed3 != ["pick", "pick"]:
		_fail("a just-confirmed swing should be on cooldown, not immediately re-usable: %s" % [pressed3]); return

	# --- auto-fire (#120): armed + ready + non-pending is the only thing
	# ready_auto_ids offers; toggling, re-arming, and mark_sent/result all
	# affect it exactly like they affect a manual press.
	var bar4 := HotbarPanel.new()
	root.add_child(bar4)
	if not bar4.ready_auto_ids().is_empty():
		_fail("nothing should be auto-ready before any ability is armed"); return
	bar4._toggle_auto(0) # nothing in slot 0 yet — toggling an empty slot is a no-op
	bar4.set_abilities([{"id": "pick", "name": "Pick", "cooldown_ms": 2000}])
	if not bar4.ready_auto_ids().is_empty():
		_fail("arming an ability must not itself arm auto-fire"); return
	bar4._toggle_auto(0)
	if bar4.ready_auto_ids() != ["pick"]:
		_fail("auto-armed + ready should offer 'pick': %s" % [bar4.ready_auto_ids()]); return
	bar4.mark_sent("pick")
	if not bar4.ready_auto_ids().is_empty():
		_fail("a pending slot must not be offered again by auto-fire"); return
	bar4.on_ability_result("pick", true, 2000)
	if not bar4.ready_auto_ids().is_empty():
		_fail("a slot on cooldown must not be offered by auto-fire"); return
	bar4._toggle_auto(0) # disarm
	if bar4._auto[0]:
		_fail("toggling auto twice should disarm it"); return
	# Re-arming the ability (unequip/equip) drops the auto toggle too.
	bar4._toggle_auto(0)
	bar4.set_abilities([])
	bar4.set_abilities([{"id": "pick", "name": "Pick", "cooldown_ms": 2000}])
	if bar4._auto[0]:
		_fail("re-arming the ability should drop a stale auto-fire toggle"); return

	print("SMOKE_OK: hotbar slots fill from equip.update, presses gate on cooldown/pending, re-arming resets both, and auto-fire only offers armed/ready/non-pending slots")
	quit(0)
