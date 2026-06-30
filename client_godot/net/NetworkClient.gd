## WebSocket transport + JSON codec + message dispatch for the gateway connection.
##
## Owns a single `WebSocketPeer`, polled each frame. Incoming frames are decoded
## and re-emitted as typed signals so the rest of the client never touches JSON or
## the socket directly. Outgoing helpers stamp the protocol version on handshake
## frames so a version-mismatched build is refused cleanly by the gateway.
class_name NetworkClient
extends Node

signal opened
signal closed
signal auth_required(version: int)
signal auth_ok(data: Dictionary)
signal auth_error(message: String)
signal welcome(data: Dictionary)
signal partition(data: Dictionary)
signal status_update(id: String, zone: String, state: Dictionary)
signal despawn(id: String)
signal zone_migration(zone: String)
signal you_died

var url := "ws://127.0.0.1:8766"

var _ws := WebSocketPeer.new()
var _was_open := false

func connect_to(u: String) -> void:
	url = u
	var err := _ws.connect_to_url(url)
	if err != OK:
		push_error("[net] connect_to_url(%s) failed: %s" % [url, err])

func is_open() -> bool:
	return _ws.get_ready_state() == WebSocketPeer.STATE_OPEN

func _process(_delta: float) -> void:
	_ws.poll()
	match _ws.get_ready_state():
		WebSocketPeer.STATE_OPEN:
			if not _was_open:
				_was_open = true
				opened.emit()
			while _ws.get_available_packet_count() > 0:
				_handle_text(_ws.get_packet().get_string_from_utf8())
		WebSocketPeer.STATE_CLOSED:
			if _was_open:
				_was_open = false
				closed.emit()

func _handle_text(text: String) -> void:
	var parsed: Variant = JSON.parse_string(text)
	if typeof(parsed) != TYPE_DICTIONARY:
		push_warning("[net] dropping non-object frame: %s" % text)
		return
	var msg: Dictionary = parsed
	match String(msg.get("type", "")):
		Protocol.S_AUTH_REQUIRED:
			auth_required.emit(int(msg.get("protocol_version", 0)))
		Protocol.S_AUTH_OK:
			auth_ok.emit(msg)
		Protocol.S_AUTH_ERROR:
			auth_error.emit(String(msg.get("message", "authentication failed")))
		Protocol.S_WELCOME:
			welcome.emit(msg)
		Protocol.S_PARTITION:
			partition.emit(msg)
		Protocol.S_STATUS_UPDATE:
			status_update.emit(
				String(msg.get("player_id", "")),
				String(msg.get("zone", "")),
				msg.get("state", {}))
		Protocol.S_DESPAWN:
			despawn.emit(String(msg.get("player_id", "")))
		Protocol.S_ZONE_MIGRATION:
			zone_migration.emit(String(msg.get("zone", "")))
		Protocol.S_YOU_DIED:
			you_died.emit()
		_:
			pass # zone_capture and any future server messages are ignored for now

# --- outgoing -----------------------------------------------------------------

func _send(obj: Dictionary) -> void:
	if is_open():
		_ws.send_text(JSON.stringify(obj))

func login(email: String, password: String) -> void:
	_send({"type": Protocol.C_LOGIN, "email": email, "password": password,
		"protocol_version": Protocol.VERSION})

func register(email: String, password: String, character_name: String) -> void:
	_send({"type": Protocol.C_REGISTER, "email": email, "password": password,
		"name": character_name, "protocol_version": Protocol.VERSION})

func guest() -> void:
	_send({"type": Protocol.C_GUEST, "protocol_version": Protocol.VERSION})

func resume_token(token: String) -> void:
	_send({"type": Protocol.C_TOKEN, "token": token,
		"protocol_version": Protocol.VERSION})

## Send a movement delta (world units). The gateway stamps the real player id.
func send_move(dx: int, dy: int) -> void:
	_send({"type": Protocol.C_MOVE, "dx": dx, "dy": dy})

## Flag a melee swing in a facing direction.
func send_attack(dx: int, dy: int) -> void:
	_send({"type": Protocol.C_ATTACK, "dx": dx, "dy": dy})
