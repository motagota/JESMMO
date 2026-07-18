## Headless smoke test for the editor toolbar (#103): button-driven and
## hotkey-driven tool switches converge on the same enabled matrix (exactly
## one pointed tool active; the brush enabled only while both pointed tools
## are off; each pointed tool's hotkey dead while the other owns the mouse),
## and the hint line tracks the active tool.
## Run: Godot --headless --path client_godot -s res://tests/smoke_editor_toolbar.gd
extends SceneTree

func _fail(message: String) -> void:
	print("SMOKE_FAIL: %s" % message)
	quit(1)

func _matrix(brush: BrushController, objects: ObjectTool, road: RoadTool) -> Dictionary:
	return {
		"brush_enabled": brush.enabled,
		"objects_mode": objects.mode,
		"objects_enabled": objects.enabled,
		"road_active": road.active,
		"road_enabled": road.enabled,
	}

func _initialize() -> void:
	var brush := BrushController.new()
	root.add_child(brush)
	var objects := ObjectTool.new()
	root.add_child(objects)
	var road := RoadTool.new()
	root.add_child(road)
	var history := HistoryPanel.new()
	root.add_child(history)
	var bar := EditorToolbar.new()
	root.add_child(bar)
	var changes: Array = []
	bar.tool_changed.connect(func(id): changes.append(id))
	bar.setup(brush, objects, road, history)

	# Fresh state: brush owns the mouse.
	if bar.active != "brush" or not brush.enabled:
		_fail("setup should land on the brush"); return

	# --- click-driven switches --------------------------------------------------
	bar.select("objects")
	var m := _matrix(brush, objects, road)
	if m != {"brush_enabled": false, "objects_mode": "place", "objects_enabled": true, "road_active": false, "road_enabled": false}:
		_fail("objects click matrix wrong: %s" % str(m)); return

	bar.select("road") # cross-switch: buttons can always switch
	m = _matrix(brush, objects, road)
	if m != {"brush_enabled": false, "objects_mode": "off", "objects_enabled": false, "road_active": true, "road_enabled": true}:
		_fail("road click matrix wrong: %s" % str(m)); return

	bar.select("brush")
	m = _matrix(brush, objects, road)
	if m != {"brush_enabled": true, "objects_mode": "off", "objects_enabled": true, "road_active": false, "road_enabled": true}:
		_fail("brush click matrix wrong: %s" % str(m)); return

	# --- hotkey-driven switches (the tools' own methods, as their keys call) ----
	objects.set_mode("delete") # [O][O] from brush state
	m = _matrix(brush, objects, road)
	if m["brush_enabled"] or m["road_enabled"] or bar.active != "objects":
		_fail("hotkey object activation should disable brush + road hotkey (got %s, active=%s)" % [str(m), bar.active]); return
	objects.set_mode("off") # [O] cycles off
	if not brush.enabled or bar.active != "brush":
		_fail("cycling objects off should fall back to the brush"); return

	road.set_active(true) # [R]
	m = _matrix(brush, objects, road)
	if m["brush_enabled"] or m["objects_enabled"] or bar.active != "road":
		_fail("hotkey road activation should disable brush + objects hotkey (got %s)" % str(m)); return
	road.set_active(false) # [R] again
	if not brush.enabled or bar.active != "brush":
		_fail("toggling road off should fall back to the brush"); return

	# Hint line follows the active tool.
	bar.select("road")
	if not ("Road" in bar._hint.text):
		_fail("hint should describe the road tool (got '%s')" % bar._hint.text); return
	bar.set_hint("custom status")
	if bar._hint.text != "custom status":
		_fail("tools' status stream should override the hint"); return

	# --- the Move slot (#105) ----------------------------------------------------
	bar.select("road_move")
	if bar.active != "road_move" or not road.move_mode or not road.active:
		_fail("road_move click should enter move mode (active=%s move=%s)" % [bar.active, road.move_mode]); return
	if brush.enabled or objects.enabled:
		_fail("move mode owns the mouse like lay mode"); return
	bar.select("road")
	if bar.active != "road" or road.move_mode:
		_fail("road click from move mode should switch to lay mode"); return
	road.set_move_active(true) # the [M] hotkey path
	if bar.active != "road_move":
		_fail("hotkey move activation should light the Move button"); return
	bar.select("brush")

	# Every transition emitted tool_changed.
	if changes.is_empty() or changes[-1] != "brush":
		_fail("tool_changed should have tracked the transitions (got %s)" % str(changes)); return

	print("SMOKE_OK: the toolbar owns the exclusivity matrix — clicks and hotkey paths converge, brush falls back, hints track")
	quit(0)
