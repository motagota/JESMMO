//! Shared terrain tile format + canonical height sampler (terrain pipeline
//! epic, issue tracker #56). Consumed by both `rust_server` (authoritative
//! heights, movement validation) and the offline `terrain-bake` tool (writes
//! the tiles this crate reads) — the whole point is one height-at-(x,y)
//! answer, not two independently-implemented ones.
//!
//! The `delta` module (terrain-editing epic, #72) adds the sparse
//! hand-authored edit layer composited on top of the baked base.

pub mod delta;
pub mod manifest;
pub mod sampler;
pub mod tile;

pub use delta::{AuthorId, HeightBlock, Provenance, SparseHeightDelta, TerrainDelta, BLOCK_SIZE};
pub use manifest::{HeightEncoding, Manifest, ManifestError};
pub use sampler::{LoadError, Terrain};
pub use tile::{decode_height, encode_height, nav, HeightTile, MetaTile, TileError};
