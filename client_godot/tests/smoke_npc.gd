## Headless smoke test for the NPC client pieces (mining/abilities epic
## #123, #121): EntityManager renders/tracks an "npc" kind entity distinctly
## from resource nodes (name, `nearest_npc`, `wpos_of`, `name_of`, in/out of
## range), and the dialogue panel shows/hides correctly including the
## granted-pickaxe mention and closing on a click.
## Run: Godot --headless --path client_godot -s res://tests/smoke_npc.gd
extends SceneTree

func _fail(message: String) -> void:
	print("SMOKE_FAIL: %s" % message)
	quit(1)

func _initialize() -> void:
	var entities := EntityManager.new()
	root.add_child(entities)

	# An NPC status_update (mirrors the server's npc_status_json) plus an
	# ordinary stone resource node, both near the origin.
	entities.upsert("npc_quarry_foreman", "zone_a", {
		"x": 100, "y": 100, "type": "npc", "name": "Sten", "facing": [0, 0],
	})
	entities.upsert("node_civic_rock_0", "zone_a", {
		"x": 100, "y": 100, "type": "resource", "item_id": "stone", "qty": 5, "facing": [0, 0],
	})

	if entities.nearest_npc(Vector2(100, 100), 10.0) != "npc_quarry_foreman":
		_fail("expected to find the NPC within range"); return
	if entities.nearest_resource(Vector2(100, 100), 10.0) != "node_civic_rock_0":
		_fail("the resource node must still resolve separately from the NPC"); return
	if entities.nearest_npc(Vector2(100, 100), 10.0) == "node_civic_rock_0":
		_fail("nearest_npc must never return a resource node"); return
	if entities.name_of("npc_quarry_foreman") != "Sten":
		_fail("expected the NPC's authored name to be tracked"); return
	if entities.wpos_of("npc_quarry_foreman") != Vector2(100, 100):
		_fail("expected the NPC's last known position to be tracked"); return

	# Out of range: "" (a resource node has no stock gate for NPCs, so
	# distance alone must be what excludes it).
	if entities.nearest_npc(Vector2(500, 500), 10.0) != "":
		_fail("an NPC well outside max_dist must not resolve"); return

	# --- dialogue panel ---------------------------------------------------
	var dlg := NpcDialoguePanel.new()
	root.add_child(dlg)
	if dlg.visible:
		_fail("the dialogue panel must start hidden"); return

	dlg.show_dialogue("Sten", ["No pick? Take mine.", "Mind the edge."], true)
	if not dlg.visible:
		_fail("show_dialogue should reveal the panel"); return
	if dlg._name_label.text != "Sten":
		_fail("expected the speaker's name on the panel: %s" % dlg._name_label.text); return
	if dlg._lines_label.text.find("Mind the edge.") == -1:
		_fail("expected the dialogue lines on the panel: %s" % dlg._lines_label.text); return
	if dlg._lines_label.text.find("pickaxe") == -1:
		_fail("a granted talk should mention the pickaxe: %s" % dlg._lines_label.text); return

	dlg.close()
	if dlg.visible:
		_fail("close() should hide the panel"); return

	# A returning-visitor talk (nothing granted) must NOT mention a pickaxe.
	dlg.show_dialogue("Sten", ["Keep swinging."], false)
	if dlg._lines_label.text.find("pickaxe") != -1:
		_fail("a non-grant talk must not mention a pickaxe: %s" % dlg._lines_label.text); return

	# A click anywhere on the panel closes it (the gui_input wiring), same
	# as the [E] path Main gates separately.
	var click := InputEventMouseButton.new()
	click.button_index = MOUSE_BUTTON_LEFT
	click.pressed = true
	dlg.get_child(0).gui_input.emit(click)
	if dlg.visible:
		_fail("a click on the panel should close it"); return

	print("SMOKE_OK: NPCs track distinctly from resource nodes (name/position/range), and the dialogue panel shows/hides, mentions a grant only when one happened, and closes on click")
	quit(0)
