//! Erosion (design doc §5.5): thermal erosion cleans up slopes steeper than
//! a natural talus angle — specifically the cliffs stylization's vertical
//! exaggeration can introduce, not a general-purpose erosion simulation
//! (real DEM data is already naturally eroded). A hydraulic pass exists as
//! a hook but is off by default, per the design doc.

use crate::config::ErosionConfig;
use crate::grid::Grid;

fn neighbors8(gx: usize, gy: usize, width: usize, height: usize) -> impl Iterator<Item = (usize, usize)> {
    let (w, h) = (width as i64, height as i64);
    let (x, y) = (gx as i64, gy as i64);
    (-1..=1).flat_map(move |dy| (-1..=1).map(move |dx| (dx, dy))).filter_map(move |(dx, dy)| {
        if dx == 0 && dy == 0 {
            return None;
        }
        let (nx, ny) = (x + dx, y + dy);
        (nx >= 0 && nx < w && ny >= 0 && ny < h).then_some((nx as usize, ny as usize))
    })
}

/// One pass of steepest-descent mass transfer: for each cell, find its
/// lowest 8-connected neighbor and move `fraction` of *however much of the
/// height difference exceeds `threshold`* to it — mass-conserving (every
/// unit moved off one cell lands on exactly one neighbor, computed from a
/// snapshot of the previous iteration so processing order can't bias the
/// result). `threshold = 0.0` makes this plain hydraulic-style smoothing
/// (erodes regardless of angle); a positive `threshold` makes it thermal
/// (only erodes past the talus angle's height-difference equivalent).
fn steepest_descent_pass(grid: &Grid, threshold: f32, fraction: f32) -> Vec<f32> {
    let (w, h) = (grid.width, grid.height);
    let mut delta = vec![0.0f32; w * h];
    for gy in 0..h {
        for gx in 0..w {
            let idx = gy * w + gx;
            let hc = grid.heights[idx];
            let mut steepest_diff = 0.0f32;
            let mut steepest_idx = None;
            for (nx, ny) in neighbors8(gx, gy, w, h) {
                let nidx = ny * w + nx;
                let diff = hc - grid.heights[nidx];
                if diff > steepest_diff {
                    steepest_diff = diff;
                    steepest_idx = Some(nidx);
                }
            }
            if let Some(nidx) = steepest_idx {
                if steepest_diff > threshold {
                    let move_amt = (steepest_diff - threshold) * fraction;
                    delta[idx] -= move_amt;
                    delta[nidx] += move_amt;
                }
            }
        }
    }
    delta
}

fn apply_delta(grid: &Grid, delta: &[f32]) -> Grid {
    let heights = grid.heights.iter().zip(delta).map(|(h, d)| h + d).collect();
    Grid { width: grid.width, height: grid.height, cell_size_m: grid.cell_size_m, heights }
}

/// Thermal erosion: `thermal_iters` passes, each moving half the excess
/// (over the talus angle's height-difference equivalent) from a cell to its
/// steepest-descent neighbor — converges toward, not instantly to, the
/// threshold, so a barely-over-the-limit slope softens gradually rather
/// than snapping flat in one step.
pub fn run_thermal_erosion(grid: &Grid, max_natural_slope_deg: f32, iters: u32) -> Grid {
    let talus_diff = grid.cell_size_m * max_natural_slope_deg.to_radians().tan();
    let mut current = grid.clone();
    for _ in 0..iters {
        let delta = steepest_descent_pass(&current, talus_diff, 0.5);
        current = apply_delta(&current, &delta);
    }
    current
}

/// Hydraulic erosion hook (off by default, `iters == 0`): unlike thermal,
/// no talus threshold — water erodes downhill regardless of angle, just a
/// small `strength` fraction per iteration.
pub fn run_hydraulic_erosion(grid: &Grid, iters: u32, strength: f32) -> Grid {
    let mut current = grid.clone();
    for _ in 0..iters {
        let delta = steepest_descent_pass(&current, 0.0, strength);
        current = apply_delta(&current, &delta);
    }
    current
}

/// The full stage: thermal first, then hydraulic (a no-op unless
/// `hydraulic_iters > 0`). `enabled == false` (the config default) skips
/// both entirely.
pub fn run_erosion_stage(grid: &Grid, config: &ErosionConfig) -> Grid {
    if !config.enabled {
        return grid.clone();
    }
    let thermal = run_thermal_erosion(grid, config.max_natural_slope, config.thermal_iters);
    run_hydraulic_erosion(&thermal, config.hydraulic_iters, config.hydraulic_strength)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single artificially steep cliff (well past any reasonable talus
    /// angle), isolated on its own grid — deliberately *not* combined with a
    /// gentle slope on the same grid, since thermal erosion cascades outward
    /// each iteration (the cliff's excess keeps propagating to its
    /// neighbor's neighbor, and so on); a shared grid would eventually let
    /// the cliff's erosion reach a "gentle" region too, which isn't what
    /// "untouched" is meant to test. See `gentle_grid` for that, fully
    /// separate.
    fn cliff_grid() -> Grid {
        let mut g = Grid::new(2, 1, 10.0);
        g.set(0, 0, 100.0);
        g.set(1, 0, 0.0); // cliff: 100m drop over 10m = ~84 degrees
        g
    }

    /// A slope well under the talus angle, with *no* steep cell anywhere on
    /// the grid to cascade in from.
    fn gentle_grid() -> Grid {
        let mut g = Grid::new(2, 1, 10.0);
        g.set(0, 0, 1.0); // gentle: 1m drop over 10m = ~6 degrees
        g.set(1, 0, 0.0);
        g
    }

    #[test]
    fn steep_slope_softens_toward_the_talus_angle() {
        let grid = cliff_grid();
        let eroded = run_thermal_erosion(&grid, 45.0, 40);
        let talus_diff = grid.cell_size_m * 45.0f32.to_radians().tan(); // == cell_size_m here
        let final_diff = eroded.get(0, 0) - eroded.get(1, 0);
        assert!(
            (final_diff - talus_diff).abs() < 0.5,
            "steep slope should converge close to the talus threshold: final_diff={final_diff}, talus_diff={talus_diff}"
        );
        assert!(final_diff < 100.0, "the cliff must actually have softened");
    }

    #[test]
    fn gentle_slope_under_the_threshold_is_untouched() {
        let grid = gentle_grid();
        let eroded = run_thermal_erosion(&grid, 45.0, 40);
        assert_eq!(eroded, grid, "a slope already under the talus angle, with no steeper cell anywhere to cascade in from, must not move");
    }

    #[test]
    fn zero_iterations_is_a_no_op() {
        let grid = cliff_grid();
        let eroded = run_thermal_erosion(&grid, 45.0, 0);
        assert_eq!(eroded, grid);
    }

    #[test]
    fn disabled_stage_is_a_no_op() {
        let grid = cliff_grid();
        let out = run_erosion_stage(&grid, &ErosionConfig::default());
        assert_eq!(out, grid);
    }

    #[test]
    fn thermal_erosion_conserves_total_mass() {
        let grid = cliff_grid();
        let eroded = run_thermal_erosion(&grid, 45.0, 40);
        let before: f32 = grid.heights.iter().sum();
        let after: f32 = eroded.heights.iter().sum();
        assert!((before - after).abs() < 0.01, "steepest-descent transfer must conserve total height: {before} vs {after}");
    }

    #[test]
    fn erosion_is_deterministic() {
        let grid = cliff_grid();
        let config = ErosionConfig { enabled: true, max_natural_slope: 45.0, thermal_iters: 20, hydraulic_iters: 0, hydraulic_strength: 0.1 };
        let a = run_erosion_stage(&grid, &config);
        let b = run_erosion_stage(&grid, &config);
        assert_eq!(a, b);
        assert_eq!(a.content_hash(), b.content_hash());
    }

    #[test]
    fn hydraulic_erosion_smooths_regardless_of_angle_when_enabled() {
        let grid = gentle_grid();
        let out = run_hydraulic_erosion(&grid, 10, 0.1);
        // Even a slope well under any thermal threshold should move under
        // plain hydraulic smoothing, since it has no angle cutoff.
        assert_ne!(out.get(0, 0), grid.get(0, 0));
    }

    #[test]
    fn hydraulic_erosion_is_a_no_op_at_zero_iterations() {
        let grid = gentle_grid();
        let out = run_hydraulic_erosion(&grid, 0, 0.1);
        assert_eq!(out, grid);
    }
}
