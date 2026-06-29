//! Wire-protocol constants shared by client and server.
//!
//! The protocol is JSON-over-WebSocket: every message is an object with a
//! `"type"` field. This module pins the protocol version (sent in `auth_required`
//! / `welcome` so a mismatched client can be rejected cleanly) and names the
//! message types introduced for M0 identity. See `docs/protocol.md` for the full
//! catalogue.

/// Bumped whenever the message set changes incompatibly. The client compares the
/// value it was built against to the one the gateway advertises in `auth_required`.
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
