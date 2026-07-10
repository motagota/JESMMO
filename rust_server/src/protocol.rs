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

// --- rent.*  (M4 §4.7) ----------------------------------------------------------
pub const S_RENT_STATUS: &str = "rent.status"; // {plot_id, due_at, paid_through, state, auto_pay, gold}
pub const C_RENT_PAY: &str = "rent.pay"; // {plot_id}
pub const S_RENT_WARNING: &str = "rent.warning"; // {plot_id, due_at}
pub const S_RENT_RECLAIMED: &str = "rent.reclaimed"; // {plot_id, moved_to_storage}
pub const C_RENT_SET_AUTOPAY: &str = "rent.set_autopay"; // {plot_id, enabled} -- opt-in; off by default

// --- district.*  (M4 §4.8 gated transitions) ------------------------------------
pub const C_DISTRICT_ENTER: &str = "district.enter"; // {from, to}
pub const S_DISTRICT_READY: &str = "district.ready"; // zone loaded; resume control

// --- mayor.*  (city build orders commissioned at runtime, e.g. roads) ----------
// Restricted to the account with `role = "mayor"`; rejected for everyone else.
pub const C_MAYOR_BUILD_CREATE: &str = "mayor.build_create"; // {district, kind, structure_kind, required_json, x, y, x1?, y1?}
pub const S_MAYOR_BUILD_ERROR: &str = "mayor.build_error"; // {message} -- rejected (not mayor / not on city land)
