//! Tile & manifest export (design doc §6, §5.6): the stage that actually
//! produces the artifact `terrain-common` reads. Landed early (design doc
//! §10 item 4, before detail/erosion/classification are polished) so
//! server/client integration isn't blocked on the remaining stages —
//! biome id is a placeholder constant until classification (#67) lands, and
//! nav flags derive from the water mask alone (walkable = not water) until
//! slope-based walkability is added.

use std::path::Path;

use terrain_common::{encode_height, nav, HeightEncoding, HeightTile, Manifest, MetaTile};

use crate::config::Config;
use crate::grid::Grid;
use crate::hash::sha256_hex;
use crate::water::WaterMask;

/// Placeholder biome id every cell gets until classification (#67) assigns
/// real ones.
const PLACEHOLDER_BIOME: u8 = 0;

pub struct ExportedArtifact {
    pub manifest: Manifest,
    pub height_tiles: Vec<HeightTile>,
    pub meta_tiles: Vec<MetaTile>,
}

/// Tile `grid` (already stylized) and `mask` (compressed to match, see
/// `stylize::compress_mask_horizontal`) into the export artifact. `mask` may
/// be smaller/larger than an exact multiple of `tile_size` — corner/cell
/// indices past the grid's actual extent clamp to its far edge (the same
/// convention `terrain_common::Terrain::locate` uses for the world's outer
/// boundary), so a not-yet-upsampled-to-target-resolution working grid (no
/// detail-synthesis stage yet, #65) still tiles cleanly.
pub fn export_artifact(grid: &Grid, mask: &WaterMask, config: &Config) -> ExportedArtifact {
    let tile_size = config.export.tile_size as usize;
    let tiles_x = grid.width.div_ceil(tile_size).max(1);
    let tiles_y = grid.height.div_ceil(tile_size).max(1);

    let (mut height_min, mut height_max) = (f32::INFINITY, f32::NEG_INFINITY);
    for &h in &grid.heights {
        height_min = height_min.min(h);
        height_max = height_max.max(h);
    }
    if !(height_max > height_min) {
        height_max = height_min + 1.0; // degenerate (flat) grid — keep encode_height's range valid
    }

    let side = tile_size + 1;
    let mut height_tiles = Vec::with_capacity(tiles_x * tiles_y);
    let mut meta_tiles = Vec::with_capacity(tiles_x * tiles_y);
    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            let mut ht = HeightTile::new(tx as i32, ty as i32, side);
            for gy in 0..side {
                for gx in 0..side {
                    let wx = (tx * tile_size + gx).min(grid.width - 1);
                    let wy = (ty * tile_size + gy).min(grid.height - 1);
                    ht.set(gx, gy, encode_height(grid.get(wx, wy), height_min, height_max));
                }
            }
            height_tiles.push(ht);

            let mut mt = MetaTile::new(tx as i32, ty as i32, tile_size);
            for gy in 0..tile_size {
                for gx in 0..tile_size {
                    let wx = (tx * tile_size + gx).min(mask.width - 1);
                    let wy = (ty * tile_size + gy).min(mask.height - 1);
                    let flags = if mask.get(wx, wy) { nav::WATER } else { nav::WALKABLE | nav::BUILDABLE };
                    mt.set(gx, gy, PLACEHOLDER_BIOME, flags);
                }
            }
            meta_tiles.push(mt);
        }
    }

    let manifest = Manifest {
        format_version: 1,
        bake_hash: compute_bake_hash(config),
        world_size_m: (grid.width as f32 * grid.cell_size_m, grid.height as f32 * grid.cell_size_m),
        tile_size: tile_size as u32,
        cell_size_m: grid.cell_size_m,
        tiles: (tiles_x as u32, tiles_y as u32),
        height_encoding: HeightEncoding::U16,
        height_min_m: height_min,
        height_max_m: height_max,
        sea_level_m: config.water.sea_level_m,
    };

    ExportedArtifact { manifest, height_tiles, meta_tiles }
}

/// The determinism anchor (design doc §8): hash of the full resolved config
/// plus this tool's own version, so a dependency/toolchain bump that changes
/// output is at least visible as a hash change rather than a silent
/// divergence. Doesn't yet fold in source-data identity (a real DEM file's
/// own hash) since ingest is still synthetic (#69 adds real ingest).
fn compute_bake_hash(config: &Config) -> String {
    let resolved = toml::to_string(config).unwrap_or_default();
    sha256_hex(format!("{resolved}\ntool_version={}", env!("CARGO_PKG_VERSION")).as_bytes())
}

/// Write `manifest.toml` and every tile under `out_dir` — the exact shape
/// `terrain_common::Terrain::load_dir` reads back with no special-casing.
pub fn write_artifact(artifact: &ExportedArtifact, out_dir: &Path) -> std::io::Result<()> {
    let tiles_dir = out_dir.join("tiles");
    std::fs::create_dir_all(&tiles_dir)?;
    std::fs::write(out_dir.join("manifest.toml"), toml::to_string(&artifact.manifest).unwrap_or_default())?;
    let fv = artifact.manifest.format_version as u16;
    for t in &artifact.height_tiles {
        std::fs::write(tiles_dir.join(format!("h_x{}_y{}.bin", t.tile_x, t.tile_y)), t.encode(fv))?;
    }
    for t in &artifact.meta_tiles {
        std::fs::write(tiles_dir.join(format!("m_x{}_y{}.bin", t.tile_x, t.tile_y)), t.encode(fv))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ExportConfig, SourceConfig, StylizeConfig, WaterConfig};

    fn test_config(tile_size: u32, out_dir: &str) -> Config {
        Config {
            source: SourceConfig {
                dem_path: None,
                bounds_utm: [0.0, 0.0, 1000.0, 800.0],
                working_res_m: 10.0,
                target_res_m: 10.0,
                seed: 42,
            },
            export: ExportConfig { tile_size, out_dir: out_dir.to_string() },
            water: WaterConfig::default(),
            stylize: StylizeConfig::default(),
        }
    }

    fn ramp_grid(width: usize, height: usize, cell_size_m: f32) -> Grid {
        let mut g = Grid::new(width, height, cell_size_m);
        for gy in 0..height {
            for gx in 0..width {
                g.set(gx, gy, gx as f32 * cell_size_m); // height = world x, exactly like terrain-common's own golden fixture
            }
        }
        g
    }

    #[test]
    fn exported_artifact_loads_via_terrain_common_with_no_special_casing() {
        let dir = std::env::temp_dir().join(format!("terrain-bake-export-load-{}", std::process::id()));
        let config = test_config(4, dir.to_str().unwrap());
        let grid = ramp_grid(10, 6, 10.0);
        let mask = WaterMask::new(grid.width, grid.height);

        let artifact = export_artifact(&grid, &mask, &config);
        write_artifact(&artifact, &dir).unwrap();

        let terrain = terrain_common::Terrain::load_dir(&dir).unwrap();
        // height = world x by construction; sample_height must agree, via
        // ordinary terrain-common code, no bake-tool-specific handling.
        assert!((terrain.sample_height(35.0, 25.0) - 35.0).abs() < 0.5);
        assert!(!terrain.is_water(10.0, 10.0));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn nav_flags_reflect_the_water_mask() {
        let dir = std::env::temp_dir().join(format!("terrain-bake-export-nav-{}", std::process::id()));
        let config = test_config(8, dir.to_str().unwrap());
        let grid = ramp_grid(8, 8, 10.0);
        let mut mask = WaterMask::new(8, 8);
        mask.set(1, 1, true);

        let artifact = export_artifact(&grid, &mask, &config);
        write_artifact(&artifact, &dir).unwrap();
        let terrain = terrain_common::Terrain::load_dir(&dir).unwrap();

        assert!(terrain.is_water(15.0, 15.0)); // cell (1,1) at 10m cells
        assert!(!terrain.is_water(75.0, 75.0)); // cell (7,7): land
        assert!(terrain.is_walkable(75.0, 75.0));
        assert!(!terrain.is_walkable(15.0, 15.0));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn adjacent_exported_tiles_agree_on_their_shared_edge() {
        // tile_size=4 over a 10-wide grid -> 3 tiles across (0,1,2); check
        // every interior seam, not just the hand-built golden fixture
        // terrain-common's own test uses.
        let dir = std::env::temp_dir().join(format!("terrain-bake-export-seam-{}", std::process::id()));
        let config = test_config(4, dir.to_str().unwrap());
        let grid = ramp_grid(10, 10, 10.0);
        let mask = WaterMask::new(grid.width, grid.height);
        let artifact = export_artifact(&grid, &mask, &config);

        for ty in 0..3usize {
            for tx in 0..2usize {
                let a = artifact.height_tiles.iter().find(|t| t.tile_x == tx as i32 && t.tile_y == ty as i32).unwrap();
                let b = artifact.height_tiles.iter().find(|t| t.tile_x == tx as i32 + 1 && t.tile_y == ty as i32).unwrap();
                for gy in 0..a.side {
                    assert_eq!(a.get(a.side - 1, gy), b.get(0, gy), "seam mismatch at tile ({tx},{ty})/({},{ty}) row {gy}", tx + 1);
                }
            }
        }

        let _ = dir; // no files written in this test — checked in memory
    }

    #[test]
    fn two_full_bakes_from_identical_inputs_are_byte_identical() {
        // Design doc §8's determinism test, end-to-end: the same config run
        // through `export_artifact` twice must produce an identical
        // `bake_hash` (the CI-checked anchor) and byte-identical tiles.
        let dir = std::env::temp_dir().join(format!("terrain-bake-export-det-{}", std::process::id()));
        let config = test_config(4, dir.to_str().unwrap());
        let grid = ramp_grid(9, 7, 10.0);
        let mask = WaterMask::new(grid.width, grid.height);

        let artifact_a = export_artifact(&grid, &mask, &config);
        let artifact_b = export_artifact(&grid, &mask, &config);

        assert_eq!(artifact_a.manifest.bake_hash, artifact_b.manifest.bake_hash);
        assert_eq!(artifact_a.manifest, artifact_b.manifest);
        for (a, b) in artifact_a.height_tiles.iter().zip(&artifact_b.height_tiles) {
            assert_eq!(a.encode(1), b.encode(1));
        }
        for (a, b) in artifact_a.meta_tiles.iter().zip(&artifact_b.meta_tiles) {
            assert_eq!(a.encode(1), b.encode(1));
        }

        // And the actual on-disk write is just as deterministic.
        write_artifact(&artifact_a, &dir).unwrap();
        let h1 = std::fs::read(dir.join("tiles/h_x0_y0.bin")).unwrap();
        write_artifact(&artifact_b, &dir).unwrap();
        let h2 = std::fs::read(dir.join("tiles/h_x0_y0.bin")).unwrap();
        assert_eq!(h1, h2);

        std::fs::remove_dir_all(&dir).ok();
    }
}
