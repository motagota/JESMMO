//! `manifest.toml` — the top-level description of a baked terrain artifact
//! (design doc §6). Everything a reader needs to make sense of the tile
//! files: world size, tile grid shape, and how raw sample integers map back
//! to real heights in meters.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub format_version: u32,
    /// Hash of (resolved config + source data identity + tool version) —
    /// the determinism anchor: two bakes from identical inputs must produce
    /// an identical hash (design doc §8).
    pub bake_hash: String,
    /// `(width, height)` of the whole baked world, in meters.
    pub world_size_m: (f32, f32),
    /// Cells per side of a tile (not corner-sample count — see
    /// [`crate::tile::HeightTile`] for why height tiles store one more
    /// sample per side than this).
    pub tile_size: u32,
    pub cell_size_m: f32,
    /// `(x, y)` tile grid dimensions. May differ per axis.
    pub tiles: (u32, u32),
    pub height_encoding: HeightEncoding,
    pub height_min_m: f32,
    pub height_max_m: f32,
    pub sea_level_m: f32,
}

/// How a height sample's raw integer maps to meters. Only `U16` exists today
/// (design doc §6: linear map over `[height_min_m, height_max_m]`, ~1.15cm
/// quantization at a 755m range — well below gameplay relevance) but this is
/// named rather than assumed so a future encoding doesn't need a format break.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeightEncoding {
    U16,
}

#[derive(Debug)]
pub enum ManifestError {
    Toml(toml::de::Error),
    Io(std::io::Error),
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestError::Toml(e) => write!(f, "manifest parse error: {e}"),
            ManifestError::Io(e) => write!(f, "manifest read error: {e}"),
        }
    }
}
impl std::error::Error for ManifestError {}

impl Manifest {
    pub fn parse(toml_text: &str) -> Result<Manifest, ManifestError> {
        toml::from_str(toml_text).map_err(ManifestError::Toml)
    }

    pub fn load(path: &std::path::Path) -> Result<Manifest, ManifestError> {
        let text = std::fs::read_to_string(path).map_err(ManifestError::Io)?;
        Manifest::parse(&text)
    }

    /// A tile's edge length in meters (`tile_size` cells × `cell_size_m`).
    pub fn tile_extent_m(&self) -> f32 {
        self.tile_size as f32 * self.cell_size_m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_design_doc_sketch() {
        let text = r#"
            format_version = 1
            bake_hash      = "abc123"
            world_size_m   = [24000.0, 24000.0]
            tile_size      = 512
            cell_size_m    = 2.0
            tiles          = [12, 12]
            height_encoding = "u16"
            height_min_m   = -5.0
            height_max_m   = 750.0
            sea_level_m    = 0.0
        "#;
        let m = Manifest::parse(text).unwrap();
        assert_eq!(m.format_version, 1);
        assert_eq!(m.world_size_m, (24000.0, 24000.0));
        assert_eq!(m.tile_size, 512);
        assert_eq!(m.tiles, (12, 12));
        assert_eq!(m.height_encoding, HeightEncoding::U16);
        assert_eq!(m.tile_extent_m(), 1024.0);
    }
}
