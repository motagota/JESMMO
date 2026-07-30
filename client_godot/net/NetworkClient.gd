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
## Equipment (mining/abilities epic #123, #119): the tool slot changed (or
## login hydration is reporting its current state) — `tool` is "" when
## nothing's armed, in which case `durability`/`max_durability` are 0.
## `abilities` mirrors the server's `equip.update` shape.
signal equip_update(tool: String, durability: int, max_durability: int, abilities: Array)
signal equip_error(message: String)
## A repair (#128) went through — `cost` is `{item_id: qty, ...}` consumed.
signal repair_done(instance_id: String, item_id: String, cost: Dictionary)
## One ability use's outcome. `reason` is only meaningful when `ok` is
## false ("no_tool" | "cooldown" | "out_of_range" | "exhausted");
## `item_id`/`qty` only when `ok` is true (#125/#127) — what the swing
## actually took, so the client can flash an ordinary "+N" gain notice.
signal ability_result(id: String, ok: bool, cooldown_ms: int, reason: String, item_id: String, qty: int)
## An NPC's reply to `npc.talk` (mining/abilities epic #123, #121).
signal npc_dialogue(npc_id: String, npc_name: String, lines: Array, granted: bool)
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
## Road plans (#95): this editor's own plan was accepted (the order arrives
## via the ordinary build.list broadcast) / rejected.
signal road_planned(order_id: String)
signal road_plan_error(message: String)
signal road_cancelled(order_id: String)
signal road_demolition_planned(order_id: String, demo_order_id: String)
## Progressive road building (#131, issue #134): a road's full cell list
## (answering road.cells_request), and a live per-cell update as one
## contribution lands (road.cell_progress) — see Protocol.gd's doc for the
## exact shapes.
signal road_cells(order_id: String, cells: Array)
signal road_cell_progress(order_id: String, cell_index: int, required: Dictionary, progress: Dictionary, completed: bool)
## Market (#137): you're standing at a built market and may trade, or the
## command was refused (out of range, no market built yet).
signal market_opened(market_id: String, x: int, y: int)
signal market_error(code: String, detail: String)
## Your warehouse at one market (#138): every row, available and locked alike,
## plus slot usage. Pushed on `market.open` and after every deposit/withdraw.
signal warehouse_state(market_id: String, items: Array, used: int, slots: int)
## Order book (#139): aggregated depth for one commodity, your own resting
## orders, and the trade ticker.
signal market_book(market_id: String, item_id: String, asks: Array, bids: Array)
signal market_orders(market_id: String, orders: Array)
signal market_trade(market_id: String, item_id: String, unit_price: int, qty: int)
## What the house took from your last market command (#141): the listing fee
## (both sides, never refunded) and any sale tax out of your proceeds.
signal market_fees(market_id: String, listing_fee: int, sale_tax: int)
## Listing board (#142): the board itself, and a broadcast when something on
## it sells so every onlooker stops showing an item that's gone.
signal listing_page(market_id: String, listings: Array)
signal listing_sold(market_id: String, listing_id: String, item_id: String, ask_price: int)
signal home_respawn_set(bed_id: String)
## The character's gold balance changed (#145) — `delta` is signed, `reason`
## is a short tag ("build_wages"). Until wages existed gold only moved at rent
## time and rode `rent_status`; it now moves during ordinary play.
signal gold_update(gold: int, delta: int, reason: String)
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
        Protocol.S_ROAD_PLANNED:
            road_planned.emit(String(msg.get("order_id", "")))
        Protocol.S_ROAD_PLAN_ERROR:
            road_plan_error.emit(String(msg.get("message", "road plan rejected")))
        Protocol.S_ROAD_CANCELLED:
            road_cancelled.emit(String(msg.get("order_id", "")))
        Protocol.S_ROAD_DEMOLITION_PLANNED:
            road_demolition_planned.emit(
                String(msg.get("order_id", "")),
                String(msg.get("demo_order_id", "")))
        Protocol.S_ROAD_CELLS:
            road_cells.emit(String(msg.get("order_id", "")), msg.get("cells", []))
        Protocol.S_ROAD_CELL_PROGRESS:
            road_cell_progress.emit(
                String(msg.get("order_id", "")),
                int(msg.get("cell_index", -1)),
                msg.get("required", {}),
                msg.get("progress", {}),
                bool(msg.get("completed", false)))
        Protocol.S_MARKET_OPENED:
            market_opened.emit(
                String(msg.get("market_id", "")),
                int(msg.get("x", 0)),
                int(msg.get("y", 0)))
        Protocol.S_MARKET_ERROR:
            market_error.emit(
                String(msg.get("code", "")),
                String(msg.get("detail", "market command refused")))
        Protocol.S_WAREHOUSE_STATE:
            warehouse_state.emit(
                String(msg.get("market_id", "")),
                msg.get("items", []),
                int(msg.get("used", 0)),
                int(msg.get("slots", 0)))
        Protocol.S_MARKET_BOOK:
            market_book.emit(
                String(msg.get("market_id", "")),
                String(msg.get("item_id", "")),
                msg.get("asks", []),
                msg.get("bids", []))
        Protocol.S_MARKET_ORDERS:
            market_orders.emit(String(msg.get("market_id", "")), msg.get("orders", []))
        Protocol.S_MARKET_TRADE:
            market_trade.emit(
                String(msg.get("market_id", "")),
                String(msg.get("item_id", "")),
                int(msg.get("unit_price", 0)),
                int(msg.get("qty", 0)))
        Protocol.S_MARKET_FEES:
            market_fees.emit(
                String(msg.get("market_id", "")),
                int(msg.get("listing_fee", 0)),
                int(msg.get("sale_tax", 0)))
        Protocol.S_LISTING_PAGE:
            listing_page.emit(String(msg.get("market_id", "")), msg.get("listings", []))
        Protocol.S_LISTING_SOLD:
            listing_sold.emit(
                String(msg.get("market_id", "")),
                String(msg.get("listing_id", "")),
                String(msg.get("item_id", "")),
                int(msg.get("ask_price", 0)))
        Protocol.S_GOLD_UPDATE:
            gold_update.emit(
                int(msg.get("gold", 0)),
                int(msg.get("delta", 0)),
                String(msg.get("reason", "")))
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
        Protocol.S_EQUIP_UPDATE:
            # `tool`/`durability`/`max_durability` ride as JSON null (not
            # just absent) when nothing's equipped — Dictionary.get's
            # default only covers a missing key, so present-but-null values
            # must be handled explicitly.
            var tool_v: Variant = msg.get("tool")
            var durability_v: Variant = msg.get("durability")
            var max_durability_v: Variant = msg.get("max_durability")
            equip_update.emit(
                String(tool_v) if tool_v != null else "",
                int(durability_v) if durability_v != null else 0,
                int(max_durability_v) if max_durability_v != null else 0,
                msg.get("abilities", []))
        Protocol.S_EQUIP_ERROR:
            equip_error.emit(String(msg.get("message", "couldn't equip that")))
        Protocol.S_REPAIR_DONE:
            repair_done.emit(
                String(msg.get("instance_id", "")),
                String(msg.get("item_id", "")),
                msg.get("cost", {}))
        Protocol.S_ABILITY_RESULT:
            ability_result.emit(
                String(msg.get("id", "")),
                bool(msg.get("ok", false)),
                int(msg.get("cooldown_ms", 0)),
                String(msg.get("reason", "")),
                String(msg.get("item_id", "")),
                int(msg.get("qty", 0)))
        Protocol.S_NPC_DIALOGUE:
            npc_dialogue.emit(
                String(msg.get("npc_id", "")),
                String(msg.get("name", "")),
                msg.get("lines", []),
                bool(msg.get("granted", false)))
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

## Submit a road plan (editor role only): `points` is `[[x, y], ...]` lattice
## coordinates whose consecutive pairs are axis-aligned runs. Answered with
## `road.planned` (and a district `build.list` broadcast) or `road.plan_error`.
func send_road_plan(points: Array) -> void:
    _send({"type": Protocol.C_ROAD_PLAN, "points": points})

## Re-route an open road plan (#104/#105, editor role only). Same shape as
## send_road_plan plus the order being moved.
func send_road_replan(order_id: String, points: Array) -> void:
    _send({"type": Protocol.C_ROAD_REPLAN, "order_id": order_id, "points": points})

## Remove a pristine road plan (#106, editor role only).
func send_road_cancel(order_id: String) -> void:
    _send({"type": Protocol.C_ROAD_CANCEL, "order_id": order_id})

## Post a demolition order for a built/part-built road (#106, editor only).
func send_road_demolish(order_id: String) -> void:
    _send({"type": Protocol.C_ROAD_DEMOLISH, "order_id": order_id})

## Ask to trade at the market you're standing next to (#137). No id: the
## server resolves and range-checks it from your live position.
func send_market_open() -> void:
    _send({"type": Protocol.C_MARKET_OPEN})

## Move goods between carried inventory and your warehouse at the market
## you're standing at (#138). Same range gate as `market.open`; the server
## bounds both by what's actually there, carry capacity, and slot capacity.
func send_warehouse_deposit(item_id: String, qty: int) -> void:
    _send({"type": Protocol.C_WAREHOUSE_DEPOSIT, "item_id": item_id, "qty": qty})

func send_warehouse_withdraw(item_id: String, qty: int) -> void:
    _send({"type": Protocol.C_WAREHOUSE_WITHDRAW, "item_id": item_id, "qty": qty})

## Order book (#139). `command_id` is client-generated and deduped server-side,
## so a resend after a dropped connection can't place or buy twice.
func send_market_sell(item_id: String, unit_price: int, qty: int, duration_hours: int) -> void:
    _send({"type": Protocol.C_MARKET_SELL, "command_id": _command_id(),
        "item_id": item_id, "unit_price": unit_price, "qty": qty,
        "duration_hours": duration_hours})

func send_market_buy(item_id: String, unit_price: int, qty: int, duration_hours: int) -> void:
    _send({"type": Protocol.C_MARKET_BUY, "command_id": _command_id(),
        "item_id": item_id, "unit_price": unit_price, "qty": qty,
        "duration_hours": duration_hours})

func send_market_cancel(order_id: String) -> void:
    _send({"type": Protocol.C_MARKET_CANCEL, "command_id": _command_id(), "order_id": order_id})

## Listing board (#142). `expected_price` is what you were SHOWN — the server
## refuses the buy if the ask has changed since, so you can never be charged a
## price you didn't agree to.
func send_listing_place(warehouse_item_id: String, ask_price: int, duration_hours: int) -> void:
    _send({"type": Protocol.C_LISTING_PLACE, "command_id": _command_id(),
        "warehouse_item_id": warehouse_item_id, "ask_price": ask_price,
        "duration_hours": duration_hours})

func send_listing_buy(listing_id: String, expected_price: int) -> void:
    _send({"type": Protocol.C_LISTING_BUY, "command_id": _command_id(),
        "listing_id": listing_id, "expected_price": expected_price})

func send_listing_cancel(listing_id: String) -> void:
    _send({"type": Protocol.C_LISTING_CANCEL, "command_id": _command_id(), "listing_id": listing_id})

## Browse the board. Every filter is optional; omit for everything.
func send_listing_list(item_id := "", min_durability := 0, max_price := 0) -> void:
    var msg := {"type": Protocol.C_LISTING_LIST}
    if item_id != "":
        msg["item_id"] = item_id
    if min_durability > 0:
        msg["min_durability"] = min_durability
    if max_price > 0:
        msg["max_price"] = max_price
    _send(msg)

func send_market_book_request(item_id: String) -> void:
    _send({"type": Protocol.C_MARKET_BOOK_REQUEST, "item_id": item_id})

## A fresh idempotency key per command. Time alone isn't unique enough at this
## resolution (the same trap as #128's test emails), so mix in randomness.
func _command_id() -> String:
    return "%d-%d" % [Time.get_ticks_usec(), randi()]

## Ask for a road order's full cell list (#134) — no role restriction, a
## stateless read like terrain/object list requests. Answered with
## `road_cells`.
func send_road_cells_request(order_id: String) -> void:
    _send({"type": Protocol.C_ROAD_CELLS_REQUEST, "order_id": order_id})

## Craft a recipe (validated server-side: owns a crafting station, has ingredients).
func send_craft_make(recipe_id: String) -> void:
    _send({"type": Protocol.C_CRAFT_MAKE, "recipe_id": recipe_id})

## Arm a SPECIFIC owned tool instance (#128 — "the pickaxe" stopped being
## well-defined once tools carry their own durability). Answered with
## `equip.update` on success, `equip_error` if not owned.
func send_equip(instance_id: String) -> void:
    _send({"type": Protocol.C_EQUIP, "instance_id": instance_id})

## Clear the tool slot.
func send_unequip() -> void:
    _send({"type": Protocol.C_UNEQUIP})

## Use an equipped ability against a target node (validated server-side:
## the tool grants it, its cooldown has elapsed, the node's in range/stocked).
func send_ability_use(ability_id: String, node_id: String) -> void:
    _send({"type": Protocol.C_ABILITY_USE, "id": ability_id, "node_id": node_id})

## Repair a specific owned tool instance at an owned crafting station
## (#128) — validated server-side; silent no-op on failure, same posture
## as `send_craft_make`.
func send_repair(instance_id: String) -> void:
    _send({"type": Protocol.C_REPAIR, "instance_id": instance_id})

## Talk to an NPC (validated server-side by proximity). Answered with
## `npc.dialogue` (mining/abilities epic #123, #121).
func send_npc_talk(npc_id: String) -> void:
    _send({"type": Protocol.C_NPC_TALK, "npc_id": npc_id})

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
