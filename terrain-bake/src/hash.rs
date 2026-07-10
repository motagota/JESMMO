//! Deterministic content hashing (config sections, grids) for the stage
//! cache and the eventual `bake_hash` (design doc §8's determinism anchor).
//! Sha256, not std's `DefaultHasher` — see [`crate::config::SourceConfig::content_hash`]
//! for why that matters here.

use sha2::{Digest, Sha256};

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}
