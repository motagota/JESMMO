## The station panel (#167): the fuel gauge, the per-player job slots, and the
## recipe list — all of it drawn from `station.state` and nothing else.
##
## What this is really guarding is that the panel never DECIDES anything. Every
## Start button's enabled-ness, every duration, every locked recipe comes from
## the server's own view; the panel's job is to render it and to say clearly why
## a button won't work. A panel that computed any of that itself would disagree
## with the server the moment `crafting.toml` was edited.
##
## Run: Godot --headless --path client_godot -s res://tests/smoke_station.gd
extends SceneTree

var _panel: StationPanel

## `quit()` only REQUESTS an exit — the rest of `_process` still runs, which is
## how an earlier version of this test printed SMOKE_OK after five failures.
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

func _buttons() -> Array:
	return _panel.find_children("*", "Button", true, false)

func _button(fragment: String) -> Button:
	for b in _buttons():
		if String(b.text).findn(fragment) >= 0:
			return b
	return null

func _state(fuel: int, jobs: Array, locked := false) -> Dictionary:
	return {
		"station_id": "furnace_mine_yard",
		"name": "Furnace",
		"kind": "heat",
		"fuel_units": fuel,
		"fuels": [{"item": "charcoal", "units": 2}],
		"job_slots": 2,
		"usage_fee_gold": 2,
		"skill_level": 1,
		"recipes": [{
			"id": "iron_ingot", "name": "Smelt Iron Ingot",
			"inputs": [{"item": "iron_ore", "qty": 2}],
			"output_item": "iron_ingot", "output_qty": 1,
			"fuel_units": 2, "duration_ms": 12000,
			"skill": "smelting", "required_level": 1, "locked": locked,
		}],
		"jobs": jobs,
	}

func _initialize() -> void:
	_panel = StationPanel.new()
	root.add_child(_panel)
	_panel.visible = true

func _process(_delta: float) -> bool:
	# --- the fee and the shared fire are both stated ------------------------
	_panel.set_inventory([{"item_id": "iron_ore", "qty": 4}, {"item_id": "charcoal", "qty": 3}])
	_panel.set_state(_state(6, []))
	var t := _text()
	if t.findn("2g per job") < 0:
		_fail("the panel should quote the fee it will charge: %s" % t)
		return true
	if t.findn("shared") < 0:
		_fail("a public fire's shared fuel should be stated, not discovered: %s" % t)
		return true
	if t.findn("Fuel: 6") < 0:
		_fail("the fuel gauge should show the server's number: %s" % t)
		return true
	if t.findn("Slot 1 — empty") < 0 or t.findn("Slot 2 — empty") < 0:
		_fail("both of this player's slots should be listed: %s" % t)
		return true

	# --- a startable recipe offers a live button -----------------------------
	var start := _button("Start")
	if start == null or start.disabled:
		_fail("with ore, fuel and a free slot, Start should be live: %s" % t)
		return true

	# --- every refusal says WHY, rather than greying out silently ------------
	_panel.set_inventory([{"item_id": "charcoal", "qty": 3}])
	_panel.set_state(_state(6, []))
	start = _button("Start")
	if start == null or not start.disabled:
		_fail("without ore, Start should be disabled")
		return true
	if String(start.tooltip_text).findn("materials") < 0:
		_fail("a disabled Start should say it's the materials: '%s'" % start.tooltip_text)
		return true

	_panel.set_inventory([{"item_id": "iron_ore", "qty": 4}])
	_panel.set_state(_state(0, []))
	start = _button("Start")
	if not start.disabled or String(start.tooltip_text).findn("fuel") < 0:
		_fail("a cold fire should disable Start and say so: '%s'" % start.tooltip_text)
		return true

	_panel.set_inventory([{"item_id": "iron_ore", "qty": 4}])
	_panel.set_state(_state(6, [], true))
	start = _button("Start")
	if not start.disabled or String(start.tooltip_text).findn("level") < 0:
		_fail("a locked recipe should name the level it needs: '%s'" % start.tooltip_text)
		return true

	# --- fuel can only be loaded when you actually have some -----------------
	_panel.set_inventory([])
	_panel.set_state(_state(6, []))
	var load_btn := _button("Load")
	if load_btn == null or not load_btn.disabled:
		_fail("with no charcoal, Load should be disabled")
		return true

	# --- a running job shows a countdown, not a Collect ----------------------
	var now := int(Time.get_unix_time_from_system())
	_panel.set_inventory([{"item_id": "iron_ore", "qty": 4}, {"item_id": "charcoal", "qty": 3}])
	_panel.set_state(_state(4, [{
		"id": "j1", "slot": 0, "recipe_id": "iron_ingot",
		"output_item": "iron_ingot", "output_qty": 1, "state": "running",
		"started_at": now, "ready_at": now + 12, "remaining_secs": 12, "refund": [],
	}]))
	t = _text()
	if t.findn("1/2 in use") < 0:
		_fail("slot usage should be visible: %s" % t)
		return true
	if _button("Collect") != null:
		_fail("a running job must not offer Collect")
		return true
	if t.findn("(12s)") < 0:
		_fail("a running job should show its remaining time: %s" % t)
		return true

	# --- a ready job offers Collect ------------------------------------------
	var collected := []
	_panel.do_collect.connect(func(job_id): collected.append(job_id))
	_panel.set_state(_state(4, [{
		"id": "j1", "slot": 0, "recipe_id": "iron_ingot",
		"output_item": "iron_ingot", "output_qty": 1, "state": "ready",
		"started_at": now - 12, "ready_at": now, "remaining_secs": 0, "refund": [],
	}]))
	var collect := _button("Collect")
	if collect == null:
		_fail("a ready job should offer Collect: %s" % _text())
		return true
	collect.pressed.emit()
	if collected != ["j1"]:
		_fail("Collect should ask the server for that specific job: %s" % [collected])
		return true

	# --- a failed job explains itself and offers the materials back ----------
	# A refund with no explanation is indistinguishable from a bug, which is
	# exactly how it would be read.
	_panel.set_state(_state(4, [{
		"id": "j2", "slot": 0, "recipe_id": "gone",
		"output_item": "iron_ingot", "output_qty": 1, "state": "failed",
		"fail_reason": "recipe_removed",
		"started_at": now - 60, "ready_at": now - 40, "remaining_secs": 0,
		"refund": [{"item": "iron_ore", "qty": 2}],
	}]))
	t = _text()
	if t.findn("no longer exists") < 0:
		_fail("a failed job should say why in words: %s" % t)
		return true
	if _button("Take materials back") == null:
		_fail("a failed job should offer the escrow back: %s" % t)
		return true

	# --- a full pack is a refusal, and must read as one ----------------------
	# The output is still in the slot. Silence here looks exactly like the
	# ingot having been destroyed, which is the one outcome that must not happen.
	var msg := Protocol.station_error_text("no_room", {"need": 1, "room": 0})
	if msg.findn("wait in the slot") < 0:
		_fail("a full pack must be described as waiting, not lost: '%s'" % msg)
		return true

	# --- a shaping station has no fuel gauge at all --------------------------
	var wheel := _state(0, [])
	wheel["kind"] = "shaping"
	wheel["name"] = "Potter's Wheel"
	wheel["fuels"] = []
	_panel.set_state(wheel)
	if _text().findn("Fuel:") >= 0:
		_fail("a shaping station should show no fuel gauge: %s" % _text())
		return true

	print("SMOKE_OK: the station panel renders the server's fuel, slots and recipes;",
		" every disabled button says why; a running job counts down, a ready one",
		" collects, a failed one explains itself and hands the escrow back; and a",
		" shaping station has no fire")
	quit(0)
	return true
