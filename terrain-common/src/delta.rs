//! Sparse hand-authored terrain edits (terrain-editing epic, issue tracker
//! #72): per-chunk **height deltas** composited on top of the immutable baked
//! artifact. Offsets, not absolute heights — a pipeline re-bake shifts the
//! base underneath but never invalidates an edit, and "revert to procedural"
//! is just "delete the block."
//!
//! ## Vocabulary (deliberate, to avoid a collision)
//!
//! "Tile" in this codebase already means the 640m streaming chunk
//! ([`crate::tile::HeightTile`], `terrain.tile_request`). The 16×16 interior
//! subdivision this module adds is therefore called a **block**, everywhere.
//! A chunk's delta stores only the blocks a brush actually touched; an
//! unedited chunk has no delta at all (zero bytes — sparsity is principle #2
//! of the design doc).
//!
//! ## Grid alignment
//!
//! Height deltas offset the chunk's **corner samples** — the same
//! `side = tile_size + 1` grid `HeightTile` stores — because that's what
//! bilinear sampling interpolates between. `side` is generally not a
//! multiple of [`BLOCK_SIZE`] (production: 129 corners → a 9×9 block grid),
//! so the last block row/column is only partially in range; out-of-range
//! cells inside an edge block are stored (as zeros) but never read.
//!
//! ## Shared-edge caveat (read before writing brush code)
//!
//! A chunk's last corner row/column is *the same world data* as its
//! neighbor's first (see `tile.rs`'s edge convention). A delta that touches
//! a shared edge must be written into **both** chunks' deltas or a seam
//! opens up. That's the write path's job (edit-op application), not this
//! module's — composition here is strictly per-chunk.

use std::collections::BTreeMap;

use crate::tile::TileError;

/// Corner samples per block side. 16×16 i16 offsets = 512 bytes raw per
/// block — small enough that v1 skips compression entirely (epic #72's
/// "zstd deferred" adaptation).
pub const BLOCK_SIZE: usize = 16;

pub const DELTA_MAGIC: [u8; 4] = *b"TRHD";

/// One touched 16×16 region of a chunk's corner grid: height offsets in
/// **centimeters** from the baked base. `i16` cm gives ±327m of adjustment —
/// far beyond any hand edit — at a precision finer than the base encoding's
/// own ~1.15cm quantization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeightBlock {
    pub offsets_cm: [i16; BLOCK_SIZE * BLOCK_SIZE],
}

impl HeightBlock {
    pub fn zeroed() -> Self {
        HeightBlock { offsets_cm: [0; BLOCK_SIZE * BLOCK_SIZE] }
    }

    pub fn is_all_zero(&self) -> bool {
        self.offsets_cm.iter().all(|&v| v == 0)
    }
}

impl Default for HeightBlock {
    fn default() -> Self {
        Self::zeroed()
    }
}

/// A chunk's sparse height-offset layer: only touched blocks exist. Keyed by
/// block index (`block_row * blocks_per_side + block_col`) in a `BTreeMap`
/// so iteration — and therefore [`SparseHeightDelta::encode`]'s output — is
/// deterministic without a separate sort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SparseHeightDelta {
    /// Corner samples per chunk side (`tile_size + 1`), same value every
    /// `HeightTile` in the artifact shares. Not serialized — like
    /// `HeightTile::decode`, the reader supplies it from the manifest.
    side: usize,
    blocks: BTreeMap<usize, HeightBlock>,
}

/// How many blocks per side cover `side` corner samples (ceiling division —
/// the last block may be partial).
pub fn blocks_per_side(side: usize) -> usize {
    side.div_ceil(BLOCK_SIZE)
}

impl SparseHeightDelta {
    pub fn new(side: usize) -> Self {
        SparseHeightDelta { side, blocks: BTreeMap::new() }
    }

    pub fn side(&self) -> usize {
        self.side
    }

    /// True when no blocks are stored — the caller should treat this the
    /// same as "no delta at all" (and the write path should delete rather
    /// than persist an empty one).
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn touched_block_count(&self) -> usize {
        self.blocks.len()
    }

    fn block_index(&self, gx: usize, gy: usize) -> usize {
        debug_assert!(gx < self.side && gy < self.side, "corner ({gx},{gy}) outside side {}", self.side);
        let bps = blocks_per_side(self.side);
        (gy / BLOCK_SIZE) * bps + (gx / BLOCK_SIZE)
    }

    fn cell_index(gx: usize, gy: usize) -> usize {
        (gy % BLOCK_SIZE) * BLOCK_SIZE + (gx % BLOCK_SIZE)
    }

    /// Offset at corner `(gx, gy)` in centimeters; `0` for any untouched
    /// block — an absent block *is* "no edit here."
    pub fn offset_cm(&self, gx: usize, gy: usize) -> i16 {
        match self.blocks.get(&self.block_index(gx, gy)) {
            Some(b) => b.offsets_cm[Self::cell_index(gx, gy)],
            None => 0,
        }
    }

    /// Offset at corner `(gx, gy)` in meters — the unit composition happens
    /// in ([`crate::Terrain::sample_height_with_delta`]).
    pub fn offset_m(&self, gx: usize, gy: usize) -> f32 {
        self.offset_cm(gx, gy) as f32 * 0.01
    }

    /// Set the offset at corner `(gx, gy)`, materializing the containing
    /// block if this is its first touch. Setting `0` does **not** delete the
    /// block — call [`SparseHeightDelta::prune_zero_blocks`] after a batch
    /// of writes (an edit op) so mid-stroke zero-crossings don't thrash the
    /// map.
    pub fn set_offset_cm(&mut self, gx: usize, gy: usize, v: i16) {
        let idx = self.block_index(gx, gy);
        let block = self.blocks.entry(idx).or_default();
        block.offsets_cm[Self::cell_index(gx, gy)] = v;
    }

    /// Drop blocks that are now all-zero — "revert to procedural = delete
    /// the block" made literal.
    pub fn prune_zero_blocks(&mut self) {
        self.blocks.retain(|_, b| !b.is_all_zero());
    }

    /// Serialize: `magic(4) + format_version(2) + reserved(2)`, then the
    /// block-presence bitmap (`ceil(blocks_per_side² / 64)` little-endian
    /// u64 words, bit *i* = block index *i* present), then each present
    /// block's 256 offsets as LE i16, in ascending block-index order. Same
    /// conventions as `tile.rs`: little-endian everywhere, `side` supplied
    /// by the reader rather than self-described.
    pub fn encode(&self, format_version: u16) -> Vec<u8> {
        let bps = blocks_per_side(self.side);
        let words = (bps * bps).div_ceil(64);
        let mut buf = Vec::with_capacity(8 + words * 8 + self.blocks.len() * BLOCK_SIZE * BLOCK_SIZE * 2);
        buf.extend_from_slice(&DELTA_MAGIC);
        buf.extend_from_slice(&format_version.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // reserved
        let mut bitmap = vec![0u64; words];
        for &idx in self.blocks.keys() {
            bitmap[idx / 64] |= 1 << (idx % 64);
        }
        for w in &bitmap {
            buf.extend_from_slice(&w.to_le_bytes());
        }
        for block in self.blocks.values() {
            for v in &block.offsets_cm {
                buf.extend_from_slice(&v.to_le_bytes());
            }
        }
        buf
    }

    pub fn decode(bytes: &[u8], side: usize) -> Result<SparseHeightDelta, TileError> {
        if bytes.len() < 8 {
            return Err(TileError::Truncated);
        }
        let magic = [bytes[0], bytes[1], bytes[2], bytes[3]];
        if magic != DELTA_MAGIC {
            return Err(TileError::BadMagic { expected: DELTA_MAGIC, actual: magic });
        }
        let bps = blocks_per_side(side);
        let words = (bps * bps).div_ceil(64);
        let bitmap_end = 8 + words * 8;
        if bytes.len() < bitmap_end {
            return Err(TileError::Truncated);
        }
        let mut indices = Vec::new();
        for w in 0..words {
            let word = u64::from_le_bytes(bytes[8 + w * 8..8 + w * 8 + 8].try_into().unwrap());
            for bit in 0..64 {
                if word & (1 << bit) != 0 {
                    indices.push(w * 64 + bit);
                }
            }
        }
        let body = &bytes[bitmap_end..];
        let expected = indices.len() * BLOCK_SIZE * BLOCK_SIZE * 2;
        if body.len() != expected {
            return Err(TileError::WrongBodySize { expected, actual: body.len() });
        }
        let mut blocks = BTreeMap::new();
        for (i, idx) in indices.into_iter().enumerate() {
            let start = i * BLOCK_SIZE * BLOCK_SIZE * 2;
            let mut block = HeightBlock::zeroed();
            for (j, chunk) in body[start..start + BLOCK_SIZE * BLOCK_SIZE * 2].chunks_exact(2).enumerate() {
                block.offsets_cm[j] = i16::from_le_bytes([chunk[0], chunk[1]]);
            }
            blocks.insert(idx, block);
        }
        Ok(SparseHeightDelta { side, blocks })
    }
}

/// Who authored an edit. Strings are the account/character ids the rest of
/// the codebase already uses — no new identity type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorId {
    /// The bake pipeline itself (a pipeline-written delta would be unusual,
    /// but the doc's "multiple writers from day one" principle reserves it).
    System,
    Editor(String),
    Player(String),
}

impl std::fmt::Display for AuthorId {
    /// `"system"` / `"editor:<id>"` / `"player:<id>"` — the exact string the
    /// `terrain_deltas.author` TEXT column stores (issue #74).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthorId::System => write!(f, "system"),
            AuthorId::Editor(id) => write!(f, "editor:{id}"),
            AuthorId::Player(id) => write!(f, "player:{id}"),
        }
    }
}

impl std::str::FromStr for AuthorId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "system" {
            return Ok(AuthorId::System);
        }
        if let Some(id) = s.strip_prefix("editor:") {
            return Ok(AuthorId::Editor(id.to_string()));
        }
        if let Some(id) = s.strip_prefix("player:") {
            return Ok(AuthorId::Player(id.to_string()));
        }
        Err(format!("unrecognized author id: {s:?}"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    pub author: AuthorId,
    /// Unix seconds, matching `now_secs` convention in the server's
    /// persistence layer.
    pub edited_at: i64,
}

/// A chunk's full delta record — what the `terrain_deltas` table row (issue
/// #74) round-trips. `chunk_tx/chunk_ty` is the *existing* streaming-chunk
/// key (`HeightTile::tile_x/tile_y`); the design doc's `region_id` is
/// dropped — this codebase has exactly one world.
#[derive(Debug, Clone, PartialEq)]
pub struct TerrainDelta {
    pub chunk_tx: i32,
    pub chunk_ty: i32,
    /// The manifest `bake_hash` current when this delta was last written —
    /// the codebase's real "pipeline version" identity (the design doc's
    /// `pipeline_version: u32` adapted to what actually exists). Compared
    /// against the loaded manifest's hash later to flag base drift; a
    /// mismatch never invalidates the delta (offsets survive a re-bake).
    pub bake_hash: String,
    /// Monotonic per chunk; bumped by every accepted edit op. Sync and
    /// optimistic-concurrency anchor.
    pub revision: u64,
    pub height_delta: Option<SparseHeightDelta>,
    pub provenance: Provenance,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Production-shaped side: tile_size 128 → 129 corners → 9×9 blocks,
    /// with the last block row/column only partially in range.
    const SIDE: usize = 129;

    #[test]
    fn blocks_per_side_ceils_partial_edge_blocks() {
        assert_eq!(blocks_per_side(129), 9);
        assert_eq!(blocks_per_side(128), 8);
        assert_eq!(blocks_per_side(16), 1);
        assert_eq!(blocks_per_side(17), 2);
    }

    #[test]
    fn untouched_corners_read_zero_and_store_nothing() {
        let d = SparseHeightDelta::new(SIDE);
        assert!(d.is_empty());
        assert_eq!(d.offset_cm(0, 0), 0);
        assert_eq!(d.offset_cm(128, 128), 0);
        assert_eq!(d.touched_block_count(), 0);
    }

    #[test]
    fn set_get_round_trips_across_block_boundaries() {
        let mut d = SparseHeightDelta::new(SIDE);
        // (15,15) and (16,16) are diagonal neighbors in different blocks.
        d.set_offset_cm(15, 15, 250);
        d.set_offset_cm(16, 16, -300);
        // The far corner lives in the partial edge block.
        d.set_offset_cm(128, 128, 100);
        assert_eq!(d.offset_cm(15, 15), 250);
        assert_eq!(d.offset_cm(16, 16), -300);
        assert_eq!(d.offset_cm(128, 128), 100);
        assert_eq!(d.offset_cm(15, 16), 0, "same block as nothing set here");
        assert_eq!(d.touched_block_count(), 3);
        assert!((d.offset_m(15, 15) - 2.5).abs() < 1e-6);
        assert!((d.offset_m(16, 16) + 3.0).abs() < 1e-6);
    }

    #[test]
    fn prune_drops_blocks_zeroed_back_out() {
        let mut d = SparseHeightDelta::new(SIDE);
        d.set_offset_cm(5, 5, 100);
        d.set_offset_cm(70, 70, 100);
        d.set_offset_cm(5, 5, 0);
        assert_eq!(d.touched_block_count(), 2, "setting zero alone doesn't delete");
        d.prune_zero_blocks();
        assert_eq!(d.touched_block_count(), 1, "revert-to-procedural = the block is gone");
        assert_eq!(d.offset_cm(70, 70), 100);
    }

    #[test]
    fn encode_decode_round_trips_a_sparse_delta() {
        let mut d = SparseHeightDelta::new(SIDE);
        d.set_offset_cm(0, 0, i16::MIN);
        d.set_offset_cm(31, 2, 1234);
        d.set_offset_cm(128, 0, i16::MAX); // partial edge block, x axis
        d.set_offset_cm(64, 128, -1);      // partial edge block, y axis
        let bytes = d.encode(1);
        let back = SparseHeightDelta::decode(&bytes, SIDE).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn empty_delta_encodes_to_header_plus_bitmap_only() {
        let d = SparseHeightDelta::new(SIDE);
        let bytes = d.encode(1);
        // 8-byte header + 2 bitmap words (81 bits) and nothing else.
        assert_eq!(bytes.len(), 8 + 2 * 8);
        let back = SparseHeightDelta::decode(&bytes, SIDE).unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn decode_rejects_bad_magic_truncation_and_wrong_body_size() {
        let mut d = SparseHeightDelta::new(SIDE);
        d.set_offset_cm(3, 3, 7);
        let bytes = d.encode(1);

        let mut corrupt = bytes.clone();
        corrupt[0] = b'X';
        assert!(matches!(SparseHeightDelta::decode(&corrupt, SIDE), Err(TileError::BadMagic { .. })));

        assert!(matches!(SparseHeightDelta::decode(&bytes[..4], SIDE), Err(TileError::Truncated)));

        let mut short = bytes.clone();
        short.truncate(bytes.len() - 2);
        assert!(matches!(SparseHeightDelta::decode(&short, SIDE), Err(TileError::WrongBodySize { .. })));
    }

    #[test]
    fn encoding_is_deterministic_regardless_of_write_order() {
        let mut a = SparseHeightDelta::new(SIDE);
        a.set_offset_cm(100, 100, 5);
        a.set_offset_cm(2, 2, 9);
        let mut b = SparseHeightDelta::new(SIDE);
        b.set_offset_cm(2, 2, 9);
        b.set_offset_cm(100, 100, 5);
        assert_eq!(a.encode(1), b.encode(1));
    }

    #[test]
    fn author_id_string_form_round_trips() {
        for author in [
            AuthorId::System,
            AuthorId::Editor("acct-123".to_string()),
            AuthorId::Player("char-456".to_string()),
        ] {
            let s = author.to_string();
            assert_eq!(s.parse::<AuthorId>().unwrap(), author);
        }
        assert!("gremlin:9".parse::<AuthorId>().is_err());
    }
}
