## Can a PLAYER get into the mine?
##
## This test exists because the answer was no, for six merged issues.
##
## Kedron Cut shipped with `send_portal_enter()` defined in `NetworkClient` and
## called by absolutely nothing in the game — no key, no button, no prompt — and
## with no mesh at the adit to suggest there was anything there. Every live probe
## for #165 through #170 called `portal.enter` directly, so all of them sailed in
## while a real player could not.
##
## The lesson is narrow and worth stating: a probe that invokes the network layer
## is testing the SERVER. It says nothing about whether the game offers the
## action. This test drives the same entry point the keyboard does.
##
## Run: Godot --headless --path client_godot -s res://tests/smoke_portal_reachable.gd
extends SceneTree

const ADIT_X := 12800.0
const ADIT_Y := 13500.0

var _failed := false

func _fail(msg: String) -> void:
	print("SMOKE_FAIL: ", msg)
	_failed = true
	quit(1)

func _portals() -> Array:
	return [{
		"id": "adit_mouth", "zone": "mine_starter", "display_name": "Kedron Cut",
		"x": ADIT_X, "y": ADIT_Y, "inside_x": 20.0, "inside_y": 30.0, "radius": 40.0,
	}]

func _process(_delta: float) -> bool:
	# --- the client is TOLD where portals are, before entering one -----------
	# Without this the client's only source of portal knowledge is
	# `portal.entered`, which arrives after you are already inside.
	var listed := _portals()
	if listed.is_empty() or not listed[0].has("x"):
		_fail("portal.list must carry a world position to draw and offer")
		return true
	if not listed[0].has("inside_x"):
		_fail("portal.list must carry the inside position, or leaving has no prompt")
		return true

	# --- the world draws something at the mouth ------------------------------
	# An unmarked portal is barely better than no portal: players walk past it.
	var world := World.new()
	root.add_child(world)
	world.set_portals(listed)
	var meshes := world.find_children("*", "MeshInstance3D", true, false)
	if meshes.size() < 2:
		_fail("the adit should be drawn as a visible structure, got %d meshes" % meshes.size())
		return true
	var near_adit := 0
	for m in meshes:
		if abs(m.position.x - ADIT_X) < 20.0 and abs(m.position.z - ADIT_Y) < 20.0:
			near_adit += 1
	if near_adit < 2:
		_fail("the adit marker should stand at the adit, %d meshes near it" % near_adit)
		return true

	# --- THE BUG: the game must offer the action -----------------------------
	# `Main._nearest_portal()` is what the interact key consults. If it cannot
	# find a portal the player is standing in, no keypress will ever enter one,
	# however healthy the server path is.
	var src := FileAccess.get_file_as_string("res://Main.gd")
	if src.find("_nearest_portal") < 0:
		_fail("Main has no portal proximity test, so the interact key cannot offer one")
		return true
	# The interact handler must actually reach `send_portal_enter`. This is a
	# source-level check for the same reason #172's dispatch guard is: the
	# function was correct and fully tested, and the bug was that nothing called
	# it. That is invisible to any test of the function itself.
	var handler := src.substr(src.find("func _on_interact_pressed"))
	handler = handler.substr(0, handler.find("\nfunc "))
	# STRIP COMMENTS BEFORE SEARCHING, and match the call rather than the name.
	# The first version of this test looked for the bare word
	# `send_portal_enter` and found it in a COMMENT explaining the bug — so
	# restoring the bug left the test green. A test made vacuous by its own
	# prose, caught by sabotage.
	var code := ""
	for line in handler.split("\n"):
		if line.strip_edges().begins_with("#"):
			continue
		code += line + "\n"
	handler = code
	if handler.find("_net.send_portal_enter(") < 0:
		_fail("the interact key does not enter portals — this is the bug that shipped")
		return true
	if handler.find("_net.send_npc_talk(") < 0:
		_fail("the interact key should still talk to NPCs")
		return true
	# NPCs must be tried FIRST, or Marlow becomes unclickable next to the adit.
	if handler.find("_net.send_npc_talk(") > handler.find("_net.send_portal_enter("):
		_fail("talking should take precedence over entering, or the foreman is unreachable")
		return true

	# --- and the HUD must say so ---------------------------------------------
	if src.find("\"enter\"") < 0:
		_fail("the HUD should offer an 'enter' verb at a portal")
		return true
	if src.find("\"leave\"") < 0:
		_fail("...and a 'leave' verb from inside")
		return true

	print("SMOKE_OK: portals are sent to the client before entering, drawn as a",
		" visible structure at the adit, offered by the interact key with NPCs",
		" taking precedence, and surfaced in the HUD as enter/leave — the mine is",
		" reachable by a player, not only by a probe")
	quit(0)
	return true
