//! The working grid: heights in meters at `working_res_m`/`target_res_m`
//! resolution, before tiling (design doc §2 — everything between ingest and
//! export operates on one of these, not on tiles yet).

use crate::hash::sha256_hex;

#[derive(Debug, Clone, PartialEq)]
pub struct Grid {
    pub width: usize,
    pub height: usize,
    pub cell_size_m: f32,
    /// Row-major, `width * height` heights in meters.
    pub heights: Vec<f32>,
}

impl Grid {
    pub fn new(width: usize, height: usize, cell_size_m: f32) -> Self {
        Grid { width, height, cell_size_m, heights: vec![0.0; width * height] }
    }

    pub fn get(&self, gx: usize, gy: usize) -> f32 {
        self.heights[gy * self.width + gx]
    }

    pub fn set(&mut self, gx: usize, gy: usize, v: f32) {
        self.heights[gy * self.width + gx] = v;
    }

    /// Deterministic content hash over shape + every height (sha256 hex) —
    /// the two-full-bakes-are-byte-identical property (design doc §8)
    /// checked at the grid level.
    pub fn content_hash(&self) -> String {
        let mut bytes = Vec::with_capacity(12 + self.heights.len() * 4);
        bytes.extend_from_slice(&(self.width as u64).to_le_bytes());
        bytes.extend_from_slice(&(self.height as u64).to_le_bytes());
        bytes.extend_from_slice(&self.cell_size_m.to_le_bytes());
        for h in &self.heights {
            bytes.extend_from_slice(&h.to_le_bytes());
        }
        sha256_hex(&bytes)
    }

    /// Simple whole-grid binary encoding for the stage cache (`cache.rs`) —
    /// not the tile format (`terrain-common`'s job once export tiles it up).
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(12 + self.heights.len() * 4);
        bytes.extend_from_slice(&(self.width as u64).to_le_bytes());
        bytes.extend_from_slice(&(self.height as u64).to_le_bytes());
        bytes.extend_from_slice(&self.cell_size_m.to_le_bytes());
        for h in &self.heights {
            bytes.extend_from_slice(&h.to_le_bytes());
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Grid> {
        if bytes.len() < 20 {
            return None;
        }
        let width = u64::from_le_bytes(bytes[0..8].try_into().ok()?) as usize;
        let height = u64::from_le_bytes(bytes[8..16].try_into().ok()?) as usize;
        let cell_size_m = f32::from_le_bytes(bytes[16..20].try_into().ok()?);
        let body = &bytes[20..];
        if body.len() != width * height * 4 {
            return None;
        }
        let heights = body.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
        Some(Grid { width, height, cell_size_m, heights })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trips() {
        let mut g = Grid::new(3, 2, 10.0);
        for gy in 0..2 {
            for gx in 0..3 {
                g.set(gx, gy, (gy * 3 + gx) as f32 * 1.5);
            }
        }
        let bytes = g.encode();
        let back = Grid::decode(&bytes).unwrap();
        assert_eq!(back, g);
    }

    #[test]
    fn content_hash_is_stable_and_sensitive_to_every_height() {
        let mut a = Grid::new(2, 2, 5.0);
        a.set(0, 0, 1.0);
        let mut b = a.clone();
        assert_eq!(a.content_hash(), b.content_hash());
        b.set(1, 1, 2.0);
        assert_ne!(a.content_hash(), b.content_hash());
    }
}
