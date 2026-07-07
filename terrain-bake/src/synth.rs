//! Synthetic placeholder height source (#59): stands in for real GeoTIFF
//! ingest (#69) so every other stage can be built and tested against
//! real-shaped input — a rough "ranges in the west, bay in the east" profile
//! plus deterministic noise texture — without needing GDAL or a sourced DEM
//! file. Every downstream stage should not need to know or care that this
//! isn't real ingest yet: same `Grid` shape, same determinism guarantees.

use crate::config::SourceConfig;
use crate::grid::Grid;

/// A small deterministic integer hash (splitmix-style), mapped to `[-1, 1]`.
/// Deliberately not `rand` — the whole pipeline must reproduce byte-identical
/// output from the same seed, forever (design doc §8), which a
/// non-reproducible-across-versions RNG can't promise. Mirrors the pattern
/// `rust_server::world::hash_corner` already uses for the same reason.
fn hash_corner(gx: i32, gy: i32, seed: u32) -> f32 {
    let mut h = (gx as u32)
        .wrapping_mul(374_761_393)
        .wrapping_add((gy as u32).wrapping_mul(668_265_263))
        .wrapping_add(seed.wrapping_mul(2_246_822_519));
    h = (h ^ (h >> 15)).wrapping_mul(2_246_822_519);
    h = (h ^ (h >> 13)).wrapping_mul(3_266_489_917);
    h ^= h >> 16;
    (h as f32 / u32::MAX as f32) * 2.0 - 1.0
}

/// Bilinear sample of an `(n+1) x (n+1)` coarse control grid at fractional
/// position `(u, v)` in `[0, 1]`.
fn sample_coarse(coarse: &[f32], n: usize, u: f32, v: f32) -> f32 {
    let uf = u.clamp(0.0, 1.0) * n as f32;
    let vf = v.clamp(0.0, 1.0) * n as f32;
    let x0 = (uf.floor() as usize).min(n - 1);
    let y0 = (vf.floor() as usize).min(n - 1);
    let fx = uf - x0 as f32;
    let fy = vf - y0 as f32;
    let stride = n + 1;
    let c00 = coarse[y0 * stride + x0];
    let c10 = coarse[y0 * stride + x0 + 1];
    let c01 = coarse[(y0 + 1) * stride + x0];
    let c11 = coarse[(y0 + 1) * stride + x0 + 1];
    let top = c00 + (c10 - c00) * fx;
    let bot = c01 + (c11 - c01) * fx;
    top + (bot - top) * fy
}

fn build_coarse_grid(n: usize, seed: u32) -> Vec<f32> {
    let stride = n + 1;
    let mut coarse = vec![0.0f32; stride * stride];
    for gy in 0..stride {
        for gx in 0..stride {
            coarse[gy * stride + gx] = hash_corner(gx as i32, gy as i32, seed);
        }
    }
    coarse
}

/// Generate the synthetic working grid: deterministic for a given
/// `(bounds_utm, working_res_m, seed)` — same config in, byte-identical
/// [`Grid`] out, every time (design doc §8).
pub fn synthesize(source: &SourceConfig) -> Grid {
    let (width_m, height_m) = source.extent_m();
    let cols = ((width_m / source.working_res_m).round().max(1.0)) as usize;
    let rows = ((height_m / source.working_res_m).round().max(1.0)) as usize;
    let mut grid = Grid::new(cols, rows, source.working_res_m);

    // Two independent coarse noise fields: one for the broad west-high/
    // east-low shape (very low frequency), one for texture on top (higher
    // frequency, smaller amplitude) — a cheap two-octave FBM stand-in.
    let shape_coarse = build_coarse_grid(4, source.seed);
    let detail_coarse = build_coarse_grid(9, source.seed.wrapping_add(1));

    let cols_f = (cols.max(2) - 1) as f32;
    let rows_f = (rows.max(2) - 1) as f32;
    for gy in 0..rows {
        for gx in 0..cols {
            let u = gx as f32 / cols_f; // 0 (west) -> 1 (east)
            let v = gy as f32 / rows_f;

            // Ranges in the west, flattening toward the bay in the east —
            // a smooth falloff, not a cliff.
            let west_to_east = (1.0 - u).powf(1.6);
            let base = 300.0 * west_to_east;

            let shape_wobble = sample_coarse(&shape_coarse, 4, u, v) * 40.0;
            let detail = sample_coarse(&detail_coarse, 9, u, v) * 15.0;

            let h = (base + shape_wobble + detail).max(-5.0);
            grid.set(gx, gy, h);
        }
    }
    grid
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(seed: u32) -> SourceConfig {
        SourceConfig {
            dem_path: None,
            bounds_utm: [0.0, 0.0, 1000.0, 500.0],
            working_res_m: 50.0,
            target_res_m: 10.0,
            seed,
        }
    }

    #[test]
    fn deterministic_for_the_same_config() {
        let a = synthesize(&test_config(1337));
        let b = synthesize(&test_config(1337));
        assert_eq!(a, b);
        assert_eq!(a.content_hash(), b.content_hash());
    }

    #[test]
    fn different_seeds_produce_different_terrain() {
        let a = synthesize(&test_config(1337));
        let b = synthesize(&test_config(7331));
        assert_ne!(a.heights, b.heights);
    }

    #[test]
    fn shape_is_higher_in_the_west_than_the_east_on_average() {
        // Not a strict per-cell guarantee (there's noise on top), but the
        // averaged west-vs-east relief should clearly reflect the
        // ranges-west/bay-east profile the config asks for.
        let grid = synthesize(&test_config(42));
        let west_avg: f32 = (0..grid.height).map(|gy| grid.get(0, gy)).sum::<f32>() / grid.height as f32;
        let east_avg: f32 = (0..grid.height)
            .map(|gy| grid.get(grid.width - 1, gy))
            .sum::<f32>()
            / grid.height as f32;
        assert!(west_avg > east_avg, "west_avg={west_avg} should exceed east_avg={east_avg}");
    }

    #[test]
    fn grid_shape_matches_bounds_and_working_resolution() {
        let grid = synthesize(&test_config(1));
        assert_eq!(grid.width, 20); // 1000m / 50m
        assert_eq!(grid.height, 10); // 500m / 50m
        assert_eq!(grid.cell_size_m, 50.0);
    }
}
