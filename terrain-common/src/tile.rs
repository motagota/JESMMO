//! Tile binary format (design doc §6): height tiles (`h_x{X}_y{Y}.bin`) and
//! metadata tiles (`m_x{X}_y{Y}.bin`).
//!
//! ## Edge convention (read this before touching sampling code)
//!
//! A [`HeightTile`] stores **`tile_size + 1` corner samples per side**, not
//! `tile_size`. A tile with `tile_size = 512` cells has 513×513 height
//! corners, and a tile's rightmost column / bottommost row is **the same
//! data** as its right/bottom neighbor's leftmost column / topmost row —
//! deliberately redundant. This is the fix for the classic tile-seam bug: any
//! point strictly inside one tile's `[0, tile_size)` cell range can always be
//! bilinearly interpolated from **that tile alone**, never needing to reach
//! into a neighbor mid-sample. The cost is a `(2*side-1)/side²` sliver of
//! duplicated storage (~0.4% at `tile_size = 512`) — worth it for every
//! reader (server, bake tool, and eventually the Godot client) sharing
//! exactly one edge-handling rule instead of three independent ones.
//!
//! [`MetaTile`] (biome id + nav flags) has **no such duplication** —
//! `tile_size × tile_size` per-cell values, one per cell, no interpolation
//! needed for a categorical value.
//!
//! All multi-byte fields are little-endian, fixed regardless of host
//! architecture — determinism (design doc §8) means the same bytes on disk
//! everywhere, not just the same host reproducing its own output.

pub const HEIGHT_TILE_MAGIC: [u8; 4] = *b"TRHT";
pub const META_TILE_MAGIC: [u8; 4] = *b"TRMT";
/// magic(4) + format_version(2) + reserved(2) + tile_x(4) + tile_y(4).
pub const HEADER_LEN: usize = 16;

#[derive(Debug, PartialEq, Eq)]
pub enum TileError {
    Truncated,
    BadMagic { expected: [u8; 4], actual: [u8; 4] },
    WrongBodySize { expected: usize, actual: usize },
}

impl std::fmt::Display for TileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TileError::Truncated => write!(f, "tile buffer shorter than the {HEADER_LEN}-byte header"),
            TileError::BadMagic { expected, actual } => write!(
                f, "bad tile magic: expected {expected:?}, got {actual:?}"
            ),
            TileError::WrongBodySize { expected, actual } => write!(
                f, "tile body is {actual} bytes, expected {expected}"
            ),
        }
    }
}
impl std::error::Error for TileError {}

fn write_header(buf: &mut Vec<u8>, magic: [u8; 4], format_version: u16, tile_x: i32, tile_y: i32) {
    buf.extend_from_slice(&magic);
    buf.extend_from_slice(&format_version.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // reserved
    buf.extend_from_slice(&tile_x.to_le_bytes());
    buf.extend_from_slice(&tile_y.to_le_bytes());
}

/// Returns `(format_version, tile_x, tile_y)`.
fn read_header(bytes: &[u8], expected_magic: [u8; 4]) -> Result<(u16, i32, i32), TileError> {
    if bytes.len() < HEADER_LEN {
        return Err(TileError::Truncated);
    }
    let magic = [bytes[0], bytes[1], bytes[2], bytes[3]];
    if magic != expected_magic {
        return Err(TileError::BadMagic { expected: expected_magic, actual: magic });
    }
    let format_version = u16::from_le_bytes([bytes[4], bytes[5]]);
    let tile_x = i32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    let tile_y = i32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    Ok((format_version, tile_x, tile_y))
}

/// Map a real height in meters to its raw `u16` encoding — a linear map over
/// `[min, max]` (design doc §6: ~1.15cm quantization at a 755m range, well
/// below gameplay relevance). Out-of-range input clamps rather than wraps.
pub fn encode_height(h: f32, min: f32, max: f32) -> u16 {
    let t = ((h - min) / (max - min)).clamp(0.0, 1.0);
    (t * u16::MAX as f32).round() as u16
}

/// Inverse of [`encode_height`].
pub fn decode_height(v: u16, min: f32, max: f32) -> f32 {
    min + (v as f32 / u16::MAX as f32) * (max - min)
}

/// A height tile: `side * side` corner samples, `side == tile_size + 1` (see
/// the module doc for why). Samples are the raw `u16` encoding; use
/// [`decode_height`] with the manifest's `height_min_m`/`height_max_m` to get
/// meters.
#[derive(Debug, Clone, PartialEq)]
pub struct HeightTile {
    pub tile_x: i32,
    pub tile_y: i32,
    pub side: usize,
    pub samples: Vec<u16>,
}

impl HeightTile {
    pub fn new(tile_x: i32, tile_y: i32, side: usize) -> Self {
        HeightTile { tile_x, tile_y, side, samples: vec![0; side * side] }
    }

    pub fn get(&self, gx: usize, gy: usize) -> u16 {
        self.samples[gy * self.side + gx]
    }

    pub fn set(&mut self, gx: usize, gy: usize, v: u16) {
        self.samples[gy * self.side + gx] = v;
    }

    pub fn encode(&self, format_version: u16) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER_LEN + self.samples.len() * 2);
        write_header(&mut buf, HEIGHT_TILE_MAGIC, format_version, self.tile_x, self.tile_y);
        for s in &self.samples {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        buf
    }

    /// Decode a tile whose corner-sample side length is already known (from
    /// the manifest's `tile_size + 1` — the format doesn't self-describe
    /// `side`, since every tile in one artifact shares it).
    pub fn decode(bytes: &[u8], side: usize) -> Result<HeightTile, TileError> {
        let (_version, tile_x, tile_y) = read_header(bytes, HEIGHT_TILE_MAGIC)?;
        let body = &bytes[HEADER_LEN..];
        let expected = side * side * 2;
        if body.len() != expected {
            return Err(TileError::WrongBodySize { expected, actual: body.len() });
        }
        let samples = body.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        Ok(HeightTile { tile_x, tile_y, side, samples })
    }
}

/// Nav-flag bits (design doc §5.6) — the high byte of a [`MetaTile`] cell.
pub mod nav {
    pub const WALKABLE: u8 = 1 << 0;
    pub const WATER: u8 = 1 << 1;
    pub const BUILDABLE: u8 = 1 << 2;
}

/// A metadata tile: `tile_size * tile_size` per-cell values, low byte biome
/// id, high byte nav-flag bitfield ([`nav`]). One value per **cell**, not
/// corner — no edge duplication needed for a categorical value.
#[derive(Debug, Clone, PartialEq)]
pub struct MetaTile {
    pub tile_x: i32,
    pub tile_y: i32,
    pub side: usize,
    pub cells: Vec<u16>,
}

impl MetaTile {
    pub fn new(tile_x: i32, tile_y: i32, side: usize) -> Self {
        MetaTile { tile_x, tile_y, side, cells: vec![0; side * side] }
    }

    pub fn biome(&self, gx: usize, gy: usize) -> u8 {
        (self.cells[gy * self.side + gx] & 0x00FF) as u8
    }

    pub fn nav_flags(&self, gx: usize, gy: usize) -> u8 {
        (self.cells[gy * self.side + gx] >> 8) as u8
    }

    pub fn set(&mut self, gx: usize, gy: usize, biome_id: u8, nav_flags: u8) {
        self.cells[gy * self.side + gx] = (biome_id as u16) | ((nav_flags as u16) << 8);
    }

    pub fn encode(&self, format_version: u16) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER_LEN + self.cells.len() * 2);
        write_header(&mut buf, META_TILE_MAGIC, format_version, self.tile_x, self.tile_y);
        for c in &self.cells {
            buf.extend_from_slice(&c.to_le_bytes());
        }
        buf
    }

    pub fn decode(bytes: &[u8], side: usize) -> Result<MetaTile, TileError> {
        let (_version, tile_x, tile_y) = read_header(bytes, META_TILE_MAGIC)?;
        let body = &bytes[HEADER_LEN..];
        let expected = side * side * 2;
        if body.len() != expected {
            return Err(TileError::WrongBodySize { expected, actual: body.len() });
        }
        let cells = body.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        Ok(MetaTile { tile_x, tile_y, side, cells })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn height_encode_decode_round_trips_within_quantization() {
        let (min, max) = (-5.0, 750.0);
        for h in [-5.0, 0.0, 100.0, 287.3, 750.0] {
            let v = encode_height(h, min, max);
            let back = decode_height(v, min, max);
            assert!((back - h).abs() < 0.02, "{h} -> {v} -> {back}");
        }
        // Out-of-range clamps rather than wrapping.
        assert_eq!(encode_height(-100.0, min, max), 0);
        assert_eq!(encode_height(10_000.0, min, max), u16::MAX);
    }

    #[test]
    fn height_tile_encode_decode_round_trips() {
        let mut t = HeightTile::new(3, -2, 3);
        for gy in 0..3 {
            for gx in 0..3 {
                t.set(gx, gy, (gy * 3 + gx) as u16 * 1000);
            }
        }
        let bytes = t.encode(1);
        assert_eq!(bytes.len(), HEADER_LEN + 3 * 3 * 2);
        let back = HeightTile::decode(&bytes, 3).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn height_tile_decode_rejects_bad_magic_and_wrong_size() {
        let t = HeightTile::new(0, 0, 3);
        let bytes = t.encode(1);
        // Wrong side -> wrong expected body size.
        assert!(matches!(HeightTile::decode(&bytes, 4), Err(TileError::WrongBodySize { .. })));
        // Corrupted magic.
        let mut corrupt = bytes.clone();
        corrupt[0] = b'X';
        assert!(matches!(HeightTile::decode(&corrupt, 3), Err(TileError::BadMagic { .. })));
    }

    #[test]
    fn meta_tile_packs_biome_and_nav_flags_independently() {
        let mut m = MetaTile::new(0, 0, 2);
        m.set(0, 0, 3, nav::WALKABLE | nav::BUILDABLE);
        m.set(1, 1, 0, nav::WATER);
        assert_eq!(m.biome(0, 0), 3);
        assert_eq!(m.nav_flags(0, 0), nav::WALKABLE | nav::BUILDABLE);
        assert_eq!(m.biome(1, 1), 0);
        assert_eq!(m.nav_flags(1, 1), nav::WATER);

        let bytes = m.encode(1);
        let back = MetaTile::decode(&bytes, 2).unwrap();
        assert_eq!(back, m);
    }
}
