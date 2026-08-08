## Seeding for live probes (#195).
##
## Three issues (#182, #183, #184) shipped without their live probe running,
## all for the same reason: the only way to put a character into a known state
## was editing `mmo_dev.db` directly, and that does not survive the gateway's
## cache for a logged-in character. Inventory in particular gets written back
## over the top, so a probe reads an empty pack and stops.
##
## This talks to the admin socket instead, which drives the SAME code a player's
## actions do. A seeded character is therefore in a state the game could really
## have produced, and any connected client is pushed the change rather than
## holding a stale view.
##
## Usage:
##
##     var seed := ProbeSeed.new()
##     add_child(seed)
##     await seed.connect_admin()
##     var cid := await seed.whois("probe@t.test")
##     await seed.items(cid, "iron_ore", 8)
##     await seed.items(cid, "stone", 40, true)   # to the storehouse
##     await seed.skill(cid, "smelting", 8)
##
## Every call waits for the server's ack, so a probe never races its own setup —
## which was the OTHER half of why direct DB edits were unreliable.
class_name ProbeSeed
extends Node

signal ready_for_commands()

const ADMIN_URL := "ws://127.0.0.1:8767"

var _ws := WebSocketPeer.new()
var _open := false
var _acks: Array = []
var _whois: Dictionary = {}


func connect_admin(timeout_s: float = 5.0) -> bool:
	_ws.connect_to_url(ADMIN_URL)
	var waited := 0.0
	while waited < timeout_s:
		_ws.poll()
		if _ws.get_ready_state() == WebSocketPeer.STATE_OPEN:
			_open = true
			return true
		await Engine.get_main_loop().process_frame
		waited += 0.016
	push_error("ProbeSeed: could not reach the admin socket at %s" % ADMIN_URL)
	return false


func _pump() -> void:
	_ws.poll()
	while _ws.get_available_packet_count() > 0:
		var raw := _ws.get_packet().get_string_from_utf8()
		var msg = JSON.parse_string(raw)
		if typeof(msg) != TYPE_DICTIONARY:
			continue
		match String(msg.get("type", "")):
			"ack": _acks.append(String(msg.get("message", "")))
			"whois": _whois[String(msg.get("email", ""))] = msg.get("character_id")


func _send_and_wait(payload: Dictionary, timeout_s: float = 5.0) -> String:
	if not _open:
		return ""
	var before := _acks.size()
	_ws.send_text(JSON.stringify(payload))
	var waited := 0.0
	while waited < timeout_s:
		_pump()
		if _acks.size() > before:
			return String(_acks[-1])
		await Engine.get_main_loop().process_frame
		waited += 0.016
	push_error("ProbeSeed: no ack for %s" % [payload])
	return ""


## The character behind an account email. Lets a probe register normally and
## then name itself, rather than guessing a uuid.
func whois(email: String, timeout_s: float = 5.0) -> String:
	if not _open:
		return ""
	_ws.send_text(JSON.stringify({"type": "whois", "email": email}))
	var waited := 0.0
	while waited < timeout_s:
		_pump()
		if _whois.has(email):
			var v = _whois[email]
			return "" if v == null else String(v)
		await Engine.get_main_loop().process_frame
		waited += 0.016
	return ""


## Items into the pack, or the storehouse when `to_storage` — a station costs
## more than a pack holds (#180), so a probe that could only fill inventory
## could not set up a build.
func items(character_id: String, item_id: String, qty: int, to_storage: bool = false) -> String:
	return await _send_and_wait({
		"type": "seed_items", "character_id": character_id,
		"item_id": item_id, "qty": qty, "to_storage": to_storage,
	})


## Raise a skill to at least `level`, by granting the XP the curve wants.
func skill(character_id: String, skill_id: String, level: int) -> String:
	return await _send_and_wait({
		"type": "seed_skill", "character_id": character_id,
		"skill_id": skill_id, "level": level,
	})
