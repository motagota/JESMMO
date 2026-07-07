//! `terrain.toml` (design doc §4). Only the sections needed so far
//! (`[source]`, `[export]`, `[water]`) — `[stylize]`/`[detail]`/`[erosion]`/
//! `[classify]` land with their respective stages (#61, #65, #66, #67).

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub source: SourceConfig,
    pub export: ExportConfig,
    /// The design doc's sketch places `sea_level_m` under `[stylize]`; it's
    /// needed by the water-mask stage (#60), which runs before stylization
    /// (#61) in the pipeline, so it lives here instead — the only
    /// deliberate deviation from the doc's config layout.
    #[serde(default)]
    pub water: WaterConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceConfig {
    /// Real GeoTIFF DEM path — unused until real ingest (#69) replaces the
    /// synthetic placeholder; harmless to leave set in the config now.
    #[serde(default)]
    pub dem_path: Option<String>,
    /// `[x0, y0, x1, y1]` in UTM meters.
    pub bounds_utm: [f64; 4],
    pub working_res_m: f32,
    pub target_res_m: f32,
    /// Seeds the synthetic placeholder (#59) and every later noise stage —
    /// must stay a plain integer (not `rand`) for cross-run determinism.
    pub seed: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportConfig {
    pub tile_size: u32,
    pub out_dir: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaterConfig {
    pub sea_level_m: f32,
    /// Structuring-element radius (cells) for the open-then-close pass that
    /// removes single-cell shoreline noise (design doc §5.2).
    pub open_close_radius: u32,
    /// Minimum guaranteed navigable river width, honored via the hand-mask
    /// override (a human paints extra width in; the pipeline's job is to
    /// never let flood-fill/clamping undo that) — design doc §5.2's "Rivers"
    /// note.
    pub min_river_width_m: f32,
    /// How far above sea level an inland depression (thresholded as water,
    /// then reclassified as land by the edge flood-fill) gets clamped to —
    /// keeps it from rendering as an accidental sub-sea-level lake.
    pub clamp_epsilon_m: f32,
    /// Optional hand-painted override PNG (design doc §5.2's
    /// `bay_cleanup.png`): a grayscale image where white forces land, black
    /// forces water, and anything else leaves the derived mask alone.
    #[serde(default)]
    pub override_mask: Option<String>,
}

impl Default for WaterConfig {
    fn default() -> Self {
        WaterConfig {
            sea_level_m: 0.0,
            open_close_radius: 1,
            min_river_width_m: 20.0,
            clamp_epsilon_m: 0.2,
            override_mask: None,
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Toml(toml::de::Error),
    Io(std::io::Error),
}
impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Toml(e) => write!(f, "config parse error: {e}"),
            ConfigError::Io(e) => write!(f, "config read error: {e}"),
        }
    }
}
impl std::error::Error for ConfigError {}

impl Config {
    pub fn parse(toml_text: &str) -> Result<Config, ConfigError> {
        toml::from_str(toml_text).map_err(ConfigError::Toml)
    }

    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(ConfigError::Io)?;
        Config::parse(&text)
    }
}

impl SourceConfig {
    /// Width/height of the source bounds in meters.
    pub fn extent_m(&self) -> (f32, f32) {
        (
            (self.bounds_utm[2] - self.bounds_utm[0]) as f32,
            (self.bounds_utm[3] - self.bounds_utm[1]) as f32,
        )
    }

    /// Stable content hash of every field that affects the ingest stage's
    /// output — the stage cache key (see `cache.rs`). Serializes to TOML
    /// (deterministic field order, matching struct declaration order) and
    /// hashes the bytes, rather than std's `DefaultHasher`, which the
    /// standard library explicitly does not guarantee stable across
    /// compiler versions — a cache key must not silently change underneath
    /// an unrelated toolchain upgrade.
    pub fn content_hash(&self) -> String {
        let s = toml::to_string(self).unwrap_or_default();
        crate::hash::sha256_hex(s.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_source_and_export_sections() {
        let text = r#"
            [source]
            bounds_utm    = [472000.0, 6930000.0, 532000.0, 6990000.0]
            working_res_m = 10.0
            target_res_m  = 2.0
            seed          = 1337

            [export]
            tile_size = 512
            out_dir   = "artifacts/world_v1/"
        "#;
        let c = Config::parse(text).unwrap();
        assert_eq!(c.source.seed, 1337);
        assert_eq!(c.source.extent_m(), (60000.0, 60000.0));
        assert_eq!(c.export.tile_size, 512);
    }

    #[test]
    fn content_hash_is_stable_and_seed_sensitive() {
        let base = SourceConfig {
            dem_path: None,
            bounds_utm: [0.0, 0.0, 1000.0, 1000.0],
            working_res_m: 10.0,
            target_res_m: 2.0,
            seed: 1,
        };
        let same = base.clone();
        let mut different = base.clone();
        different.seed = 2;

        assert_eq!(base.content_hash(), same.content_hash());
        assert_ne!(base.content_hash(), different.content_hash());
    }
}
