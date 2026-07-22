## Headless smoke test for the ability hotbar (mining/abilities epic #123,
## #119): populating slots from an `equip.update`-shaped abilities list,
## key/click presses emitting exactly one `use_pressed` per ready slot,
## the cooldown gate blocking a re-press until `ability.result` says it's
## ready again, and re-arming resetting any stale cooldown.
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

	print("SMOKE_OK: hotbar slots fill from equip.update, presses gate on cooldown, and re-arming resets it")
	quit(0)
