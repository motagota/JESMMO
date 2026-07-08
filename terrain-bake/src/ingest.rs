//! Real DEM ingest (design doc §5.1, issue #69). No `gdal` crate dependency
//! here at all — reprojection (to UTM), cropping (to `bounds_utm`),
//! resampling (to `working_res_m`), and NoData-fill (LiDAR gaps over water,
//! filled with sea level) all happen once, outside this tool, in
//! `tools/convert_dem.py` (documented in `terrain-bake/README.md`). That
//! script writes its result using [`Grid`]'s own binary encoding (see
//! `grid.rs`), so this module's whole job is: read that file back.
//!
//! When `source.dem_path` is unset, falls back to the synthetic placeholder
//! (#59) — every stage built and tested against synthetic-only configs
//! before this issue keeps working unmodified, proving the synthetic-first
//! approach didn't paper over a real-data-shaped assumption (this issue's
//! own acceptance criteria).

use std::fmt;
use std::path::Path;

use crate::config::SourceConfig;
use crate::grid::Grid;
use crate::synth;

#[derive(Debug)]
pub enum IngestError {
    Io(std::io::Error),
    /// The file at `dem_path` isn't a valid `Grid::encode()` payload (wrong
    /// magic/length, truncated, hand-edited) — distinct from `Io` so the CLI
    /// can give a more useful message than a bare filesystem error.
    Decode,
}

impl fmt::Display for IngestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IngestError::Io(e) => write!(f, "failed to read DEM grid file: {e}"),
            IngestError::Decode => write!(f, "DEM grid file isn't a valid pre-converted grid (see tools/convert_dem.py)"),
        }
    }
}
impl std::error::Error for IngestError {}
impl From<std::io::Error> for IngestError {
    fn from(e: std::io::Error) -> Self {
        IngestError::Io(e)
    }
}

/// Loads a pre-converted raw grid file (`tools/convert_dem.py`'s output) —
/// just `Grid::decode` over the file's bytes, no parsing logic of its own.
pub fn load_dem_grid(path: &Path) -> Result<Grid, IngestError> {
    let bytes = std::fs::read(path)?;
    Grid::decode(&bytes).ok_or(IngestError::Decode)
}

/// The ingest stage's entry point: real DEM if `source.dem_path` is set,
/// otherwise the synthetic placeholder. Every later stage takes a [`Grid`]
/// and doesn't know or care which path produced it.
pub fn run_ingest(source: &SourceConfig) -> Result<Grid, IngestError> {
    match &source.dem_path {
        Some(path) => load_dem_grid(Path::new(path)),
        None => Ok(synth::synthesize(source)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_source(dem_path: Option<String>) -> SourceConfig {
        SourceConfig { dem_path, bounds_utm: [0.0, 0.0, 1000.0, 500.0], working_res_m: 50.0, target_res_m: 10.0, seed: 42 }
    }

    #[test]
    fn falls_back_to_synth_when_dem_path_is_unset() {
        let source = test_source(None);
        let got = run_ingest(&source).unwrap();
        assert_eq!(got, synth::synthesize(&source));
    }

    #[test]
    fn loads_a_pre_converted_grid_file_when_dem_path_is_set() {
        let mut grid = Grid::new(4, 3, 25.0);
        for gy in 0..3 {
            for gx in 0..4 {
                grid.set(gx, gy, (gx + gy * 4) as f32 * 1.25);
            }
        }
        let path = std::env::temp_dir().join(format!("terrain-bake-ingest-test-{}.grid", std::process::id()));
        std::fs::write(&path, grid.encode()).unwrap();

        let source = test_source(Some(path.to_str().unwrap().to_string()));
        let got = run_ingest(&source).unwrap();
        assert_eq!(got, grid);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_missing_dem_file_is_an_io_error_not_a_panic() {
        let source = test_source(Some("this/path/does/not/exist.grid".to_string()));
        assert!(matches!(run_ingest(&source), Err(IngestError::Io(_))));
    }

    #[test]
    fn a_corrupt_dem_file_is_a_decode_error_not_a_panic() {
        let path = std::env::temp_dir().join(format!("terrain-bake-ingest-corrupt-{}.grid", std::process::id()));
        std::fs::write(&path, b"not a grid file").unwrap();
        let source = test_source(Some(path.to_str().unwrap().to_string()));
        assert!(matches!(run_ingest(&source), Err(IngestError::Decode)));
        std::fs::remove_file(&path).ok();
    }
}
