//! Shared server library for the MMO.
//!
//! Code used across the `proxy` and `zone_server` binaries: durable persistence,
//! account authentication, and the wire-protocol constants. Keeping these in a
//! library (rather than duplicated in each binary) means the schema, hashing,
//! and protocol version have a single source of truth.

pub mod auth;
pub mod persistence;
pub mod protocol;
pub mod world;
