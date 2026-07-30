//! Wire-protocol constants shared by client and server.
//!
//! The protocol is JSON-over-WebSocket: every message is an object with a
//! `"type"` field. This module pins the protocol version (sent in `auth_required`
//! / `welcome` so a mismatched client can be rejected cleanly) and names every
//! message type — the M0 identity handshake plus the gameplay messages reserved
//! by domain prefix for Phase 1. See `docs/protocol.md` for the full catalogue.

/// Bumped whenever the message set changes incompatibly. The gateway advertises
/// it in `auth_required`; a client that declares a different `protocol_version`
/// in its handshake frame is refused with an `auth_error` (see proxy
/// `run_handshake`). Clients that omit it (legacy/bots) are accepted as guests.
pub const PROTOCOL_VERSION: u32 = 1;

// --- Server -> client, handshake ------------------------------------------------
pub const S_AUTH_REQUIRED: &str = "auth_required"; // first frame; carries protocol_version
pub const S_AUTH_OK: &str = "auth_ok"; // login/register succeeded (carries token + name)
pub const S_AUTH_ERROR: &str = "auth_error"; // login/register failed (carries message)
pub const S_WELCOME: &str = "welcome"; // identity assigned, world join begins

// --- Client -> server, handshake ------------------------------------------------
pub const C_REGISTER: &str = "register"; // {email, password, name}
pub const C_LOGIN: &str = "login"; // {email, password}
pub const C_TOKEN: &str = "token"; // {token}  (resume an in-memory session)
pub const C_GUEST: &str = "guest"; // ephemeral, non-persisted character

// ================================================================================
// Reserved gameplay message names (Phase 1).
//
// New gameplay messages are grouped by a domain prefix (`gather.*`, `inv.*`,
// `build.*`, `plot.*`, `skill.*`, `rent.*`, `craft.*`, `district.*`) so the wire
// stays self-describing as features land. These constants are the single source
// of truth that the Godot client's `Protocol.gd` mirrors and that
// `docs/protocol.md` documents. They are *reserved here now* (issue #3); the
// server handlers that act on them arrive with their milestones (M2-M4), so until
// then sending one is a no-op. The handshake messages above remain the only ones
// the gateway currently interprets.
// ================================================================================

// --- gather.*  (M2 §4.1) --------------------------------------------------------
pub const C_GATHER_START: &str = "gather.start"; // {node_id}
pub const S_GATHER_PROGRESS: &str = "gather.progress"; // {node_id, pct}
pub const S_GATHER_RESULT: &str = "gather.result"; // {item_id, qty}
pub const S_NODE_DEPLETED: &str = "node.depleted"; // {node_id}
pub const S_NODE_RESPAWN: &str = "node.respawn"; // {node_id}

// --- inv.* / store.*  (M2 §4.2) -------------------------------------------------
pub const S_INV_UPDATE: &str = "inv.update"; // {items}
pub const C_INV_MOVE: &str = "inv.move"; // {from, to}
pub const C_STORE_DEPOSIT: &str = "store.deposit"; // {item_id, qty}
pub const C_STORE_WITHDRAW: &str = "store.withdraw"; // {item_id, qty}

// --- build.*  (M2 §4.3 build orders, M3 §4.5 placement) -------------------------
pub const C_BUILD_LIST: &str = "build.list"; // -> open orders for the district
pub const C_BUILD_CONTRIBUTE: &str = "build.contribute"; // {order_id, item_id, qty}
pub const S_BUILD_PROGRESS: &str = "build.progress"; // {order_id, progress}
pub const S_BUILD_COMPLETED: &str = "build.completed"; // {order_id, structures}
pub const S_BUILD_UNLOCKED: &str = "build.unlocked"; // {order_ids}
pub const C_BUILD_PLACE: &str = "build.place"; // {kind, x, y, rot}
pub const S_BUILD_PLACED: &str = "build.placed"; // {structure}

// --- plot.*  (M3 §4.4) ----------------------------------------------------------
pub const S_PLOT_ASSIGNED: &str = "plot.assigned"; // {plot_id, district, bounds, tier, just_claimed}
pub const C_PLOT_INFO: &str = "plot.info"; // -> current plot details

// --- skill.*  (M2 §4.6) ---------------------------------------------------------
pub const S_SKILL_UPDATE: &str = "skill.update"; // {skill_id, xp, level}
pub const S_SKILL_LEVELUP: &str = "skill.levelup"; // {skill_id, level}

// --- craft.* / home.*  (M3 §4.5) ------------------------------------------------
pub const C_HOME_SET_RESPAWN: &str = "home.set_respawn"; // {bed_id}
pub const S_HOME_RESPAWN_SET: &str = "home.respawn_set"; // {bed_id} -- ack once the bed is validated as the caller's own
pub const C_CRAFT_LIST: &str = "craft.list"; // -> available recipes
pub const S_CRAFT_RECIPES: &str = "craft.recipes"; // {recipes: [{id, name, inputs, output_item, output_qty}]}
pub const C_CRAFT_MAKE: &str = "craft.make"; // {recipe_id}
pub const S_CRAFT_MADE: &str = "craft.made"; // {recipe_id, item_id, qty} -- feedback once craft.make succeeds

// --- terrain.* — cosmetic heightmap (#54) + native-resolution tile streaming ----
pub const C_TERRAIN_LIST: &str = "terrain.list"; // -> the authored heightmap grid
// {resolution, world_size, heights: [f32; (resolution+1)^2]} -- the coarse,
// whole-world backdrop grid (unchanged since #54), plus manifest-derived
// fields (added for terrain streaming) so the client knows the streamable
// tile grid's shape: tile_size (cells/side), tiles: [cols, rows],
// cell_size_m, height_min_m, height_max_m.
pub const S_TERRAIN_DATA: &str = "terrain.data";
pub const C_TERRAIN_TILE_REQUEST: &str = "terrain.tile_request"; // {tx, ty}
// {tx, ty, side, encoding: "tile_v1", data_b64} -- data_b64 is exactly
// terrain_common::HeightTile::encode(1)'s bytes, base64-wrapped (16-byte
// header + little-endian u16 corner samples) -- the on-disk wire format
// reused byte-for-byte as the network format. A request for a tile outside
// the manifest's tile grid, or not currently loaded, is silently ignored.
pub const S_TERRAIN_TILE_DATA: &str = "terrain.tile_data";
// --- terrain editing (epic #72): hand-authored delta layer, per chunk ----------
pub const C_TERRAIN_DELTA_REQUEST: &str = "terrain.delta_request"; // {tx, ty}
// {tx, ty, has_delta, revision?, encoding?: "delta_v1", data_b64?} -- data_b64
// is terrain_common::SparseHeightDelta::encode(1)'s bytes, base64-wrapped
// (magic "TRHD" + block bitmap + touched 16x16 i16-cm blocks). Unlike
// `terrain.tile_request`, an IN-RANGE chunk always answers -- `has_delta:
// false` when unedited -- so the client never has to distinguish "not
// answered yet" from "answered, nothing here". Out-of-range requests are
// silently ignored, same as the tile path.
pub const S_TERRAIN_DELTA_DATA: &str = "terrain.delta_data";
// {brush, cells: [[cx, cy, d_cm], ...]} -- one editor brush stroke. Cells are
// WORLD corner coordinates (cx in [0, tile_size*tiles_x], same for cy) with
// centimeter height increments; the server maps each corner to every chunk
// that shares it (the duplicated-edge convention), so a stroke crossing a
// chunk seam can never open a gap. Restricted to role == "editor".
pub const C_TERRAIN_EDIT_OP: &str = "terrain.edit_op";
// {message} -- the op was rejected (not an editor / out of bounds / over the
// per-corner offset cap / malformed).
pub const S_TERRAIN_EDIT_ERROR: &str = "terrain.edit_error";
// {tx, ty, revision, encoding: "delta_v1", data_b64} -- pushed to EVERY
// connected client after an accepted edit op, once per chunk the op touched.
// data_b64 is the chunk's full current delta (same encoding as
// terrain.delta_data), not just the changed blocks -- deltas are small, and
// replace-not-merge keeps the client decode path single.
pub const S_TERRAIN_DELTA_PATCH: &str = "terrain.delta_patch";
// {op_id, brush} -- sent to the op's AUTHOR only, before the patches, so
// its history/undo UI can record the id the server minted for the stroke.
pub const S_TERRAIN_EDIT_ACK: &str = "terrain.edit_ack";
// {op_id} -- undo one accepted op: restores every block it touched to its
// pre-op content (whole-block snapshots from the op log), bumps revisions,
// and broadcasts terrain.delta_patch per affected chunk like a normal edit.
// Editor-role-gated like terrain.edit_op; an unknown or already-reverted op
// is rejected with terrain.edit_error. Note: reverting out of stroke order
// can clobber a later overlapping op (whole-block restore, by design) --
// clients should offer undo-last.
pub const C_TERRAIN_REVERT_OP: &str = "terrain.revert_op";
// {op_id} -- the revert was applied (patches follow separately).
pub const S_TERRAIN_REVERT_ACK: &str = "terrain.revert_ack";

// --- object.* — placed world props (player-attributes epic #83, issue #85) -----
// Editor-authored props with gameplay meaning (first kind: "poison_tree").
// World-scoped like terrain: every client sees every object regardless of
// zone/district. Coordinates are world units (metres), the same space as
// structures and resource nodes.
// (no payload) -> {objects: [{id, kind, x, y}, ...]} -- the full current
// object roster, answered from the gateway's in-memory cache. Stateless
// read like terrain.list; clients request it once the world is up and then
// stay current via the placed/removed broadcasts.
pub const C_OBJECT_LIST: &str = "object.list";
pub const S_OBJECT_LIST: &str = "object.list";
// {kind, x, y} -- place one object. Restricted to role == "editor"; kind
// must be a registered object kind, (x, y) inside the world.
pub const C_OBJECT_PLACE: &str = "object.place";
// {object_id} -- delete one placed object. Restricted to role == "editor".
pub const C_OBJECT_DELETE: &str = "object.delete";
// {id, kind, x, y} / {id} -- pushed to EVERY connected client after an
// accepted place/delete (the author included -- clients render acks, same
// reconcile shape as terrain.delta_patch).
pub const S_OBJECT_PLACED: &str = "object.placed";
pub const S_OBJECT_REMOVED: &str = "object.removed";
// {message} -- the place/delete was rejected (not an editor / unknown kind /
// out of bounds / no such object / no database).
pub const S_OBJECT_EDIT_ERROR: &str = "object.edit_error";

// --- rent.*  (M4 §4.7) ----------------------------------------------------------
pub const S_RENT_STATUS: &str = "rent.status"; // {plot_id, due_at, paid_through, state, auto_pay, gold}
pub const C_RENT_PAY: &str = "rent.pay"; // {plot_id}
pub const S_RENT_WARNING: &str = "rent.warning"; // {plot_id, due_at}
pub const S_RENT_RECLAIMED: &str = "rent.reclaimed"; // {plot_id, moved_to_storage}
pub const C_RENT_SET_AUTOPAY: &str = "rent.set_autopay"; // {plot_id, enabled} -- opt-in; off by default

// --- gold.* (build wages, #145) -------------------------------------------------
// {gold, delta, reason} -- the character's authoritative gold balance after it
// changed, plus what moved and why. Until #145 gold only ever changed at rent
// time, so `rent.status`'s own `gold` field was enough; build wages make the
// balance move during ordinary play, and a player earning money they can't see
// is a bug in feel if not in code. `delta` is signed (positive for wages);
// `reason` is a short tag ("build_wages") for client feedback and telemetry.
pub const S_GOLD_UPDATE: &str = "gold.update";

// --- market.* — player-to-player trading (market epic #136, issue #137) ---------
// {} -- ask to trade at whatever market you're standing next to. Deliberately
// carries NO market id: the server resolves it from the caller's live position
// like every other proximity-gated action, so a client can't name a market it
// isn't at. Answered with market.opened, or market.error when nothing's in
// range. This is the subsystem's first command and establishes the range gate
// (MARKET_RANGE, server-enforced) that every later market command inherits.
pub const C_MARKET_OPEN: &str = "market.open";
// {market_id, x, y} -- you're at a built market and may trade. `market_id` is
// the completed build order's own id; books/warehouses/listings are keyed by it
// from day one, since per-market state is the point of the design even though
// only the capital's market exists in v1.
pub const S_MARKET_OPENED: &str = "market.opened";
// {code, detail} -- a market command was refused ("out_of_range",
// "warehouse_full", ...).
pub const S_MARKET_ERROR: &str = "market.error";
// {item_id, qty} -- move goods between carried inventory and your warehouse AT
// the market you're standing at (#138). Both share market.open's server-side
// range gate. Custody is the point: goods are local, so you deposit stock to
// sell it and collect purchases where you bought them; nothing teleports
// between markets. Tools move as INSTANCES, keeping their own durability and
// row id (#128), which is what lets the listing board sell a specific worn one.
pub const C_WAREHOUSE_DEPOSIT: &str = "warehouse.deposit";
pub const C_WAREHOUSE_WITHDRAW: &str = "warehouse.withdraw";
// {market_id, items: [{id, item_id, qty, state, durability?, max_durability?}],
// used, slots} -- your full warehouse at one market, pushed on market.open and
// after every deposit/withdraw. `state` is "available" | "locked"; locked stock
// is escrowed against an open sell order (#139) and can't be withdrawn, and
// travels as its own row rather than being merged into the available total so
// the client can show WHY it isn't takeable. Capacity is counted in SLOTS
// (rows), not units.
pub const S_WAREHOUSE_STATE: &str = "warehouse.state";

// --- market order book (issue #139) ---------------------------------------------
// All three share market.open's server-side range gate and carry a
// client-generated `command_id`, deduped server-side so a reconnect-and-resend
// can't place or buy twice. The commodity key is `item_id` ALONE (no quality
// system exists); only stackable items are commodities, and tools go to the
// listing board (#142) instead.
//
// {command_id, item_id, unit_price, qty} -- rest a SELL order. Escrows the
// goods out of your warehouse at this market (available -> locked) and rests
// the order for exactly what could be escrowed, never a promise you can't keep.
pub const C_MARKET_SELL: &str = "market.sell";
// {command_id, item_id, unit_price, qty} -- buy IMMEDIATELY against the book.
// Never rests: whatever can't fill now simply isn't bought. Sweeps
// cheapest-first, and every fill executes at the RESTING order's price, not
// your limit -- price improvement goes to whoever crosses the spread. Bounded
// by your limit, your gold, the resting size, and your own warehouse capacity.
pub const C_MARKET_BUY: &str = "market.buy";
// {command_id, order_id} -- cancel your own resting order; unsold escrow
// returns to available.
pub const C_MARKET_CANCEL: &str = "market.cancel";
// {item_id} -- ask for one commodity's depth without waiting for a change.
pub const C_MARKET_BOOK_REQUEST: &str = "market.book_request";
// {market_id, item_id, asks: [{price, qty}], bids: [...]} -- depth AGGREGATED
// by price level. Individual order ownership is never broadcast: it keeps the
// message small and stops players reading each other's positions. Pushed to the
// whole district on any change (markets are per-district, so this is cheap;
// the design doc's MarketSubscribe is the upgrade path if it stops being).
pub const S_MARKET_BOOK: &str = "market.book";
// {market_id, orders: [{order_id, side, item_id, unit_price, qty_total,
// qty_remaining}]} -- YOUR resting orders only, pushed on market.open and
// after any change to them.
pub const S_MARKET_ORDERS: &str = "market.orders";
// {market_id, item_id, unit_price, qty} -- a fill just happened; the ticker.
pub const S_MARKET_TRADE: &str = "market.trade";
// {market_id, listing_fee, sale_tax} -- what the house just took from YOUR
// command (issue #141). A listing fee is charged to BOTH sides at placement on
// notional and is never refunded on cancel or expiry -- that's what makes
// posting an order you don't mean to honour cost something. Sale tax comes out
// of a seller's proceeds per fill. Both are BURNED at the capital market: the
// gold leaves the economy, which is the point (rent was the only sink before
// #145 made gold earnable at all). Every fee rounds UP and is never zero on a
// nonzero amount, so splitting one order into many can't dodge the sink.
pub const S_MARKET_FEES: &str = "market.fees";

// --- district.*  (M4 §4.8 gated transitions) ------------------------------------
pub const C_DISTRICT_ENTER: &str = "district.enter"; // {from, to}
pub const S_DISTRICT_READY: &str = "district.ready"; // zone loaded; resume control

// --- mayor.*  (city build orders commissioned at runtime, e.g. roads) ----------
// Restricted to the account with `role = "mayor"`; rejected for everyone else.
pub const C_MAYOR_BUILD_CREATE: &str = "mayor.build_create"; // {district, kind, structure_kind, required_json, x, y, x1?, y1?}
pub const S_MAYOR_BUILD_ERROR: &str = "mayor.build_error"; // {message} -- rejected (not mayor / not on city land)

// --- road.* — editor-laid grid roads (roads & quarry epic #93, #94) -------------
// {points: [[x, y], ...]} -- a polyline of lattice points (integer metres, the
// world's native 1m grid) whose consecutive pairs are AXIS-ALIGNED runs. One
// accepted plan becomes ONE ordinary build order (structure_kind "dirt_road",
// stone cost scaled by total path length) that players fulfil via the normal
// build.contribute flow. Restricted to role == "editor".
pub const C_ROAD_PLAN: &str = "road.plan";
// {order_id} -- the plan was accepted and its work order created (the order
// itself arrives through the ordinary build.list broadcast).
pub const S_ROAD_PLANNED: &str = "road.planned";
// {message} -- the plan was rejected (not an editor / malformed / diagonal
// run / off-world / over the length cap / crossing an owned plot / no db).
pub const S_ROAD_PLAN_ERROR: &str = "road.plan_error";
// {order_id, points} -- re-route an OPEN road plan (#104): full road.plan
// validation, stone cost recomputed from the new length, contributed
// progress kept (if it already covers the new cost the order completes on
// the spot). Editor-gated; acked with road.planned, rejected with
// road.plan_error. Built roads don't move -- demolish + re-lay (#106).
pub const C_ROAD_REPLAN: &str = "road.replan";
// {order_id} -- remove a pristine (open, zero-progress) road plan (#106).
// Progressed plans and built roads are refused toward road.demolish -- no
// silent vaporising of players' hauled stone. Ack: road.cancelled.
pub const C_ROAD_CANCEL: &str = "road.cancel";
pub const S_ROAD_CANCELLED: &str = "road.cancelled"; // {order_id}
// {order_id} -- post a DEMOLITION order for a built road or part-built plan
// (#106): kind demo_<target id>, requires {tool_kit: 1}, worked on site via
// the ordinary contribution proximity (it carries the road's path, no
// placement). Completion removes the road everywhere and refunds its banked
// stone (built: full required; plan: contributed progress) pro-rata to the
// demolition's contributors, into town storage.
pub const C_ROAD_DEMOLISH: &str = "road.demolish";
pub const S_ROAD_DEMOLITION_PLANNED: &str = "road.demolition_planned"; // {order_id, demo_order_id}
// {order_id, cell_index, required, progress, completed} -- pushed on a
// build.contribute that lands on a road (progressive road building epic
// #131, issue #133): a road's path is chopped into fixed-length cells at
// plan time (see road_cell in persistence), each with its own cost, and a
// contribution routes to the nearest INCOMPLETE cell within BOARD_RANGE of
// the contributor -- no build-board fallback like every other order, you
// build the stretch you're standing on. Fires alongside the ordinary
// build.progress (still the order's pooled aggregate, unchanged shape, for
// anything reading the total). The order as a whole still completes -- and
// still fires build.completed -- once every cell is done, same as before.
pub const S_ROAD_CELL_PROGRESS: &str = "road.cell_progress";
// {order_id} -- ask for a road order's full cell list (progressive road
// building epic #131, issue #134): a stateless read, same as
// terrain.list/object.list. Answered with road.cells.
pub const C_ROAD_CELLS_REQUEST: &str = "road.cells_request";
// {order_id, cells: [{cell_index, x0, y0, x1, y1, required, progress,
// completed}]} -- a road order's full cell geometry + state, in path order.
// The client seeds its progressive pavement render and nearest-cell
// contribution readout from this once per road, then keeps it current via
// road.cell_progress deltas.
pub const S_ROAD_CELLS: &str = "road.cells";
