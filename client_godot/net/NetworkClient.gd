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
signal gather_progress(node_id: String, pct: int)
signal gather_result(item_id: String, qty: int)
signal inv_update(items: Array, used: int, capacity: int)
signal skill_update(skill_id: String, xp: int, level: int)
signal skill_levelup(skill_id: String, level: int)
signal store_update(items: Array)
signal build_list(orders: Array)
signal build_progress(order_id: String, required: Dictionary, progress: Dictionary)
signal build_completed(order_id: String, structures: Array)
signal build_unlocked(order_ids: Array)
signal plot_assigned(plot_id: String, district: String, bounds: Dictionary, tier: int, just_claimed: bool)
signal plot_district(plots: Array)
signal build_placed(structure: Dictionary)
signal craft_recipes(recipes: Array)
signal craft_made(recipe_id: String, item_id: String, qty: int)
signal terrain_data(resolution: int, world_size: float, heights: PackedFloat32Array)
signal terrain_tile_data(tx: int, ty: int, heights: PackedFloat32Array)
## `offsets` is a dense side*side meter-offset grid (zeros where unedited);
## empty when `has_delta` is false.
signal terrain_delta_data(tx: int, ty: int, has_delta: bool, offsets: PackedFloat32Array)
## An accepted edit op's authoritative result for one chunk (terrain editing
## #72) — pushed by the server to every client, whoever painted. Same dense
## meter-offset decode as `terrain_delta_data`; replace-not-merge.
signal terrain_delta_patch(tx: int, ty: int, revision: int, offsets: PackedFloat32Array)
## This client's own edit op was rejected (not an editor / bounds / caps).
signal terrain_edit_error(message: String)
## This client's own accepted op, with the server-minted id (undo handle).
signal terrain_edit_ack(op_id: String, brush: String)
## This client's own revert was applied (patches arrive separately).
signal terrain_revert_ack(op_id: String)
## Placed world props (#86): the full roster (answer to `object.list`), and
## the per-object broadcasts every client receives on an accepted editor
## place/delete. `object_edit_error` is this client's own rejected op.
signal object_list(objects: Array)
signal object_placed(id: String, kind: String, x: float, y: float)
signal object_removed(id: String)
signal object_edit_error(message: String)
signal home_respawn_set(bed_id: String)
signal rent_status(plot_id: String, due_at: int, paid_through: int, state: String, auto_pay: bool, gold: int)
signal rent_warning(plot_id: String, due_at: int)
signal rent_reclaimed(plot_id: String, moved_to_storage: Array)
signal district_ready
signal mayor_build_error(message: String)

var url := "ws://127.0.0.1:8766"

var _ws := WebSocketPeer.new()
var _was_open := false

func connect_to(u: String) -> void:
    url = u
    # Godot's default inbound buffer is 64KiB per frame — the one-time
    # `terrain.data` backdrop (a (TERRAIN_RESOLUTION+1)^2 JSON heights array,
    # ~700KB at resolution 192 on the 25.6km world) silently exceeds it and
    # the client just never sees the message. Size generously; it's a cap,
    # not an allocation.
    _ws.inbound_buffer_size = 8 * 1024 * 1024
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
        Protocol.S_GATHER_PROGRESS:
            gather_progress.emit(String(msg.get("node_id", "")), int(msg.get("pct", 0)))
        Protocol.S_GATHER_RESULT:
            gather_result.emit(String(msg.get("item_id", "")), int(msg.get("qty", 0)))
        Protocol.S_INV_UPDATE:
            inv_update.emit(
                msg.get("items", []),
                int(msg.get("used", 0)),
                int(msg.get("capacity", 0)))
        Protocol.S_SKILL_UPDATE:
            skill_update.emit(
                String(msg.get("skill_id", "")),
                int(msg.get("xp", 0)),
                int(msg.get("level", 0)))
        Protocol.S_SKILL_LEVELUP:
            skill_levelup.emit(
                String(msg.get("skill_id", "")),
                int(msg.get("level", 0)))
        Protocol.S_STORE_UPDATE:
            store_update.emit(msg.get("items", []))
        Protocol.S_BUILD_LIST:
            build_list.emit(msg.get("orders", []))
        Protocol.S_BUILD_PROGRESS:
            build_progress.emit(
                String(msg.get("order_id", "")),
                msg.get("required", {}),
                msg.get("progress", {}))
        Protocol.S_BUILD_COMPLETED:
            build_completed.emit(
                String(msg.get("order_id", "")),
                msg.get("structures", []))
        Protocol.S_BUILD_UNLOCKED:
            build_unlocked.emit(msg.get("order_ids", []))
        Protocol.S_PLOT_ASSIGNED:
            plot_assigned.emit(
                String(msg.get("plot_id", "")),
                String(msg.get("district", "")),
                msg.get("bounds", {}),
                int(msg.get("tier", 0)),
                bool(msg.get("just_claimed", false)))
        Protocol.S_PLOT_DISTRICT:
            plot_district.emit(msg.get("plots", []))
        Protocol.S_BUILD_PLACED:
            build_placed.emit(msg.get("structure", {}))
        Protocol.S_CRAFT_RECIPES:
            craft_recipes.emit(msg.get("recipes", []))
        Protocol.S_CRAFT_MADE:
            craft_made.emit(
                String(msg.get("recipe_id", "")),
                String(msg.get("item_id", "")),
                int(msg.get("qty", 0)))
        Protocol.S_TERRAIN_DATA:
            var raw_heights: Array = msg.get("heights", [])
            var packed := PackedFloat32Array()
            packed.resize(raw_heights.size())
            for i in range(raw_heights.size()):
                packed[i] = float(raw_heights[i])
            # The streamable tile grid's shape rides the same message —
            # applied here, at the decode layer, so it's guaranteed in place
            # before any terrain.tile_data payload could need it.
            var tiles: Array = msg.get("tiles", [0, 0])
            Protocol.apply_terrain_meta(
                int(msg.get("tile_size", 0)),
                float(msg.get("cell_size_m", 0.0)),
                int(tiles[0]) if tiles.size() > 0 else 0,
                int(tiles[1]) if tiles.size() > 1 else 0,
                float(msg.get("height_min_m", 0.0)),
                float(msg.get("height_max_m", 0.0)))
            terrain_data.emit(int(msg.get("resolution", 0)), float(msg.get("world_size", 0.0)), packed)
        Protocol.S_TERRAIN_TILE_DATA:
            # data_b64 is terrain-common's HeightTile::encode bytes verbatim,
            # base64-wrapped to ride the all-JSON transport. Decoded to meters
            # here (Protocol.decode_height_tile mirrors HeightTile::decode +
            # decode_height); a malformed payload is dropped silently.
            var decoded := Protocol.decode_height_tile(
                Marshalls.base64_to_raw(String(msg.get("data_b64", ""))))
            if not decoded.is_empty():
                terrain_tile_data.emit(int(decoded["tx"]), int(decoded["ty"]), decoded["heights"])
        Protocol.S_TERRAIN_DELTA_DATA:
            # Hand-authored edit layer (terrain editing #72). `has_delta:
            # false` still emits — the streamer counts the answer so a chunk
            # isn't left waiting — just with empty offsets. A malformed
            # payload decodes to empty and is treated as no delta, degrading
            # to base terrain (same posture as a malformed tile).
            var has_delta: bool = bool(msg.get("has_delta", false))
            var offsets := PackedFloat32Array()
            if has_delta:
                offsets = Protocol.decode_height_delta(
                    Marshalls.base64_to_raw(String(msg.get("data_b64", ""))))
                has_delta = not offsets.is_empty()
            terrain_delta_data.emit(int(msg.get("tx", 0)), int(msg.get("ty", 0)), has_delta, offsets)
        Protocol.S_TERRAIN_DELTA_PATCH:
            # An accepted edit's per-chunk authoritative state (terrain
            # editing #72). A malformed payload is dropped silently — the
            # chunk simply keeps its current (possibly preview) heights until
            # the next stream-in re-requests the delta.
            var patch_offsets := Protocol.decode_height_delta(
                Marshalls.base64_to_raw(String(msg.get("data_b64", ""))))
            if not patch_offsets.is_empty():
                terrain_delta_patch.emit(
                    int(msg.get("tx", 0)), int(msg.get("ty", 0)),
                    int(msg.get("revision", 0)), patch_offsets)
        Protocol.S_TERRAIN_EDIT_ERROR:
            terrain_edit_error.emit(String(msg.get("message", "edit rejected")))
        Protocol.S_TERRAIN_EDIT_ACK:
            terrain_edit_ack.emit(String(msg.get("op_id", "")), String(msg.get("brush", "")))
        Protocol.S_TERRAIN_REVERT_ACK:
            terrain_revert_ack.emit(String(msg.get("op_id", "")))
        Protocol.S_OBJECT_LIST:
            object_list.emit(msg.get("objects", []))
        Protocol.S_OBJECT_PLACED:
            object_placed.emit(
                String(msg.get("id", "")),
                String(msg.get("kind", "")),
                float(msg.get("x", 0)),
                float(msg.get("y", 0)))
        Protocol.S_OBJECT_REMOVED:
            object_removed.emit(String(msg.get("id", "")))
        Protocol.S_OBJECT_EDIT_ERROR:
            object_edit_error.emit(String(msg.get("message", "object edit rejected")))
        Protocol.S_HOME_RESPAWN_SET:
            home_respawn_set.emit(String(msg.get("bed_id", "")))
        Protocol.S_RENT_STATUS:
            rent_status.emit(
                String(msg.get("plot_id", "")),
                int(msg.get("due_at", 0)),
                int(msg.get("paid_through", 0)),
                String(msg.get("state", "")),
                bool(msg.get("auto_pay", false)),
                int(msg.get("gold", 0)))
        Protocol.S_RENT_WARNING:
            rent_warning.emit(String(msg.get("plot_id", "")), int(msg.get("due_at", 0)))
        Protocol.S_RENT_RECLAIMED:
            rent_reclaimed.emit(String(msg.get("plot_id", "")), msg.get("moved_to_storage", []))
        Protocol.S_DISTRICT_READY:
            district_ready.emit()
        Protocol.S_MAYOR_BUILD_ERROR:
            mayor_build_error.emit(String(msg.get("message", "that build order was rejected")))
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

## Begin gathering a resource node.
func send_gather_start(node_id: String) -> void:
    _send({"type": Protocol.C_GATHER_START, "node_id": node_id})

func send_gather_stop() -> void:
    _send({"type": Protocol.C_GATHER_STOP})

## Deposit / withdraw items at a storage point (validated server-side by proximity).
func send_store_deposit(item_id: String, qty: int) -> void:
    _send({"type": Protocol.C_STORE_DEPOSIT, "item_id": item_id, "qty": qty})

func send_store_withdraw(item_id: String, qty: int) -> void:
    _send({"type": Protocol.C_STORE_WITHDRAW, "item_id": item_id, "qty": qty})

## Request the district's build-order board (the server also pushes it unprompted).
func send_build_list() -> void:
    _send({"type": Protocol.C_BUILD_LIST})

## Request the current district's plot roster (the server also pushes it
## unprompted on login/district-crossing/a plot changing hands).
func send_plot_district() -> void:
    _send({"type": Protocol.C_PLOT_DISTRICT})

## Contribute carried items to a build order (validated server-side by board proximity).
func send_build_contribute(order_id: String, item_id: String, qty: int) -> void:
    _send({"type": Protocol.C_BUILD_CONTRIBUTE, "order_id": order_id, "item_id": item_id, "qty": qty})

## Place a home structure at a world position (validated server-side: on your own
## plot, in bounds, no overlap).
func send_build_place(kind: String, x: int, y: int, rot: int) -> void:
    _send({"type": Protocol.C_BUILD_PLACE, "kind": kind, "x": x, "y": y, "rot": rot})

## Request the static recipe registry.
func send_craft_list() -> void:
    _send({"type": Protocol.C_CRAFT_LIST})

## Request the authored terrain heightmap (#54) — static and session-long, so
## sent once, same pattern as `send_craft_list`.
func send_terrain_list() -> void:
    _send({"type": Protocol.C_TERRAIN_LIST})

## Request one native-resolution terrain tile (terrain streaming) — sent by
## `TerrainStreamer` as the player nears a tile it doesn't have. Stateless
## and idempotent server-side; an out-of-range coordinate is silently ignored.
func send_terrain_tile_request(tx: int, ty: int) -> void:
    _send({"type": Protocol.C_TERRAIN_TILE_REQUEST, "tx": tx, "ty": ty})

## Request a chunk's hand-authored edit layer (terrain editing #72) — sent by
## `TerrainStreamer` alongside each tile request. An in-range chunk always
## answers (`has_delta: false` when unedited); out-of-range is silently
## ignored, same as the tile path.
func send_terrain_delta_request(tx: int, ty: int) -> void:
    _send({"type": Protocol.C_TERRAIN_DELTA_REQUEST, "tx": tx, "ty": ty})

## Send one editor brush stroke (terrain editing #72): `cells` is
## `[[cx, cy, d_cm], ...]` in world corner coordinates. Server-validated
## (editor role, bounds, caps); answered with `terrain.delta_patch` per
## touched chunk on success, `terrain.edit_error` on rejection.
func send_terrain_edit_op(brush: String, cells: Array) -> void:
    _send({"type": Protocol.C_TERRAIN_EDIT_OP, "brush": brush, "cells": cells})

## Undo one accepted edit op by its acked id (terrain editing #79).
func send_terrain_revert_op(op_id: String) -> void:
    _send({"type": Protocol.C_TERRAIN_REVERT_OP, "op_id": op_id})

## Request the full placed-object roster (#86) — sent once per session after
## `welcome` (the answer is explicit even when empty), then the
## placed/removed broadcasts keep the client current.
func send_object_list() -> void:
    _send({"type": Protocol.C_OBJECT_LIST})

## Place a world object (editor role only; the server broadcasts
## `object.placed` to everyone on success, `object.edit_error` back on
## rejection).
func send_object_place(kind: String, x: int, y: int) -> void:
    _send({"type": Protocol.C_OBJECT_PLACE, "kind": kind, "x": x, "y": y})

## Delete a placed world object by id (editor role only; broadcast
## `object.removed` on success).
func send_object_delete(object_id: String) -> void:
    _send({"type": Protocol.C_OBJECT_DELETE, "object_id": object_id})

## Craft a recipe (validated server-side: owns a crafting station, has ingredients).
func send_craft_make(recipe_id: String) -> void:
    _send({"type": Protocol.C_CRAFT_MAKE, "recipe_id": recipe_id})

## Set a bed (must be on your own plot) as your respawn point.
func send_home_set_respawn(bed_id: String) -> void:
    _send({"type": Protocol.C_HOME_SET_RESPAWN, "bed_id": bed_id})

## Pay rent on your own plot (deducts gold server-side; validated by ownership
## and balance).
func send_rent_pay(plot_id: String) -> void:
    _send({"type": Protocol.C_RENT_PAY, "plot_id": plot_id})

## Toggle whether the rent ticker should auto-pay this plot when due (opt-in).
func send_rent_set_autopay(plot_id: String, enabled: bool) -> void:
    _send({"type": Protocol.C_RENT_SET_AUTOPAY, "plot_id": plot_id, "enabled": enabled})

## Announce a self-detected district crossing (the client already knows every
## zone's district from `partition`). The gateway refreshes district-scoped
## content and acks `district.ready` (#15); the actual position/zone handoff
## already happened via the ordinary migrate-request path.
func send_district_enter(from_district: String, to_district: String) -> void:
    _send({"type": Protocol.C_DISTRICT_ENTER, "from": from_district, "to": to_district})

## Commission a city build order (mayor-only; the server rejects anyone else with
## `mayor.build_error`). `x1`/`y1` are the end point of a segment-shaped structure
## (e.g. a dirt path); omit them (pass `x`/`y` again) for a point structure.
func send_mayor_build_create(district: String, kind: String, structure_kind: String,
        required_json: String, x: int, y: int, x1: int, y1: int) -> void:
    _send({
        "type": Protocol.C_MAYOR_BUILD_CREATE, "district": district, "kind": kind,
        "structure_kind": structure_kind, "required_json": required_json,
        "x": x, "y": y, "x1": x1, "y1": y1,
    })
