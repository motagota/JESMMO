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

// --- rent.*  (M4 §4.7) ----------------------------------------------------------
pub const S_RENT_STATUS: &str = "rent.status"; // {plot_id, due_at, paid_through, state, auto_pay, gold}
pub const C_RENT_PAY: &str = "rent.pay"; // {plot_id}
pub const S_RENT_WARNING: &str = "rent.warning"; // {plot_id, due_at}
pub const S_RENT_RECLAIMED: &str = "rent.reclaimed"; // {plot_id, moved_to_storage}
pub const C_RENT_SET_AUTOPAY: &str = "rent.set_autopay"; // {plot_id, enabled} -- opt-in; off by default

// --- district.*  (M4 §4.8 gated transitions) ------------------------------------
pub const C_DISTRICT_ENTER: &str = "district.enter"; // {from, to}
pub const S_DISTRICT_READY: &str = "district.ready"; // zone loaded; resume control
