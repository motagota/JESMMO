//! Stylization (design doc §5.3): reshape ingest/water-masked heights into
//! something game-sized — compress a ~60km real span down to a playable
//! ~24km world, exaggerate modest real relief into a proper mountain
//! silhouette, and flatten the future capital city's footprint to one
//! buildable plateau with a smooth (not terraced) blend back to natural
//! terrain at its edges.

use std::collections::VecDeque;

use crate::config::StylizeConfig;
use crate::grid::Grid;
use crate::water::WaterMask;

/// A binary hand-painted footprint (design doc's `capital_flatten.png`):
/// white = inside, anything else = outside. Distinct from `water::
/// OverrideMask` (which forces a binary decision directly) — this instead
/// drives a *distance-based smooth blend*, not a hard cutover.
#[derive(Debug, Clone, PartialEq)]
pub struct FootprintMask {
    pub width: usize,
    pub height: usize,
    pub cells: Vec<bool>,
}

impl FootprintMask {
    pub fn none(width: usize, height: usize) -> Self {
        FootprintMask { width, height, cells: vec![false; width * height] }
    }

    pub fn get(&self, gx: usize, gy: usize) -> bool {
        self.cells[gy * self.width + gx]
    }

    pub fn set(&mut self, gx: usize, gy: usize, v: bool) {
        self.cells[gy * self.width + gx] = v;
    }

    pub fn is_empty(&self) -> bool {
        !self.cells.iter().any(|&c| c)
    }

    /// `>= 128` (grayscale) counts as inside the footprint.
    pub fn from_luma_png(path: &std::path::Path) -> Result<FootprintMask, image::ImageError> {
        let img = image::open(path)?.to_luma8();
        let (width, height) = (img.width() as usize, img.height() as usize);
        let mut mask = FootprintMask::none(width, height);
        for gy in 0..height {
            for gx in 0..width {
                if img.get_pixel(gx as u32, gy as u32)[0] >= 128 {
                    mask.set(gx, gy, true);
                }
            }
        }
        Ok(mask)
    }
}

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

/// Multi-source BFS distance (in cells, 8-connected — Chebyshev-ish, not
/// exact Euclidean, which is plenty for a blend-margin heuristic) from the
/// nearest `true` cell in `mask`. Footprint cells themselves are distance 0.
fn distance_from_footprint(mask: &FootprintMask) -> Vec<f32> {
    let mut dist = vec![f32::INFINITY; mask.width * mask.height];
    let mut queue = VecDeque::new();
    for gy in 0..mask.height {
        for gx in 0..mask.width {
            if mask.get(gx, gy) {
                dist[gy * mask.width + gx] = 0.0;
                queue.push_back((gx, gy));
            }
        }
    }
    while let Some((gx, gy)) = queue.pop_front() {
        let d = dist[gy * mask.width + gx];
        for (nx, ny) in neighbors8(gx, gy, mask.width, mask.height) {
            let idx = ny * mask.width + nx;
            if dist[idx] > d + 1.0 {
                dist[idx] = d + 1.0;
                queue.push_back((nx, ny));
            }
        }
    }
    dist
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Resample the whole grid by `scale` (bilinear) — `scale < 1` shrinks the
/// world's total extent (fewer cells, same `cell_size_m`), `scale == 1.0` is
/// a no-op, matching `StylizeConfig::default`.
pub fn compress_horizontal(grid: &Grid, scale: f32) -> Grid {
    if scale == 1.0 {
        return grid.clone();
    }
    let new_width = ((grid.width as f32) * scale).round().max(1.0) as usize;
    let new_height = ((grid.height as f32) * scale).round().max(1.0) as usize;
    let mut out = Grid::new(new_width, new_height, grid.cell_size_m);
    for gy in 0..new_height {
        for gx in 0..new_width {
            let src_x = (gx as f32 / scale).min((grid.width - 1) as f32);
            let src_y = (gy as f32 / scale).min((grid.height - 1) as f32);
            out.set(gx, gy, bilinear_sample(grid, src_x, src_y));
        }
    }
    out
}

/// The mask counterpart to [`compress_horizontal`] — nearest-neighbor, not
/// bilinear (a water/land decision doesn't blend), so a mask computed before
/// stylization (the water-mask stage, #60) can be brought into alignment
/// with the now-compressed grid for export (#62) without re-running
/// threshold/open/close/flood-fill against the resampled heights.
pub fn compress_mask_horizontal(mask: &WaterMask, scale: f32) -> WaterMask {
    if scale == 1.0 {
        return mask.clone();
    }
    let new_width = ((mask.width as f32) * scale).round().max(1.0) as usize;
    let new_height = ((mask.height as f32) * scale).round().max(1.0) as usize;
    let mut out = WaterMask::new(new_width, new_height);
    for gy in 0..new_height {
        for gx in 0..new_width {
            let src_x = ((gx as f32 / scale).round() as usize).min(mask.width - 1);
            let src_y = ((gy as f32 / scale).round() as usize).min(mask.height - 1);
            out.set(gx, gy, mask.get(src_x, src_y));
        }
    }
    out
}

fn bilinear_sample(grid: &Grid, x: f32, y: f32) -> f32 {
    let x0 = (x.floor() as usize).min(grid.width - 1);
    let y0 = (y.floor() as usize).min(grid.height - 1);
    let x1 = (x0 + 1).min(grid.width - 1);
    let y1 = (y0 + 1).min(grid.height - 1);
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;
    let h00 = grid.get(x0, y0);
    let h10 = grid.get(x1, y0);
    let h01 = grid.get(x0, y1);
    let h11 = grid.get(x1, y1);
    let top = h00 + (h10 - h00) * fx;
    let bot = h01 + (h11 - h01) * fx;
    top + (bot - top) * fy
}

/// Reshape the distribution of positive relief with a power curve, then
/// apply a flat multiplier — design doc §5.3: `h' = (h/h_max)^exp * h_max`,
/// then `* scale`. Heights at or below zero (water/below-sea-level cells,
/// already settled by the water-mask stage) skip the curve — it's meant to
/// reshape *land* relief, and a non-integer power of a negative base isn't
/// even real-valued — but still get the flat `scale` multiply for unit
/// consistency with the rest of the grid.
pub fn apply_vertical_curve(grid: &mut Grid, curve_exp: f32, scale: f32) {
    let h_max = grid.heights.iter().cloned().fold(0.0f32, f32::max).max(1e-6);
    for h in grid.heights.iter_mut() {
        if *h > 0.0 {
            let normalized = *h / h_max;
            *h = normalized.powf(curve_exp) * h_max * scale;
        } else {
            *h *= scale;
        }
    }
}

/// Blend every cell toward one target height (the footprint's own average
/// natural height) — fully flattened inside the footprint, smoothly
/// (smoothstep, not linearly — and *definitely* not a hard cutover) back to
/// untouched natural terrain by `margin_m` away from its edge. A `None`/
/// empty footprint (no mask configured) is a no-op.
pub fn apply_capital_flatten(grid: &mut Grid, footprint: &FootprintMask, margin_m: f32) {
    if footprint.is_empty() {
        return;
    }
    let mut sum = 0.0f64;
    let mut count = 0u64;
    for gy in 0..grid.height.min(footprint.height) {
        for gx in 0..grid.width.min(footprint.width) {
            if footprint.get(gx, gy) {
                sum += grid.get(gx, gy) as f64;
                count += 1;
            }
        }
    }
    if count == 0 {
        return;
    }
    let target = (sum / count as f64) as f32;

    let dist_cells = distance_from_footprint(footprint);
    let margin_cells = (margin_m / grid.cell_size_m).max(1e-6);
    for gy in 0..grid.height {
        for gx in 0..grid.width {
            let d = dist_cells.get(gy * footprint.width + gx).copied().unwrap_or(f32::INFINITY);
            let t = smoothstep(1.0 - d / margin_cells);
            if t > 0.0 {
                let h = grid.get(gx, gy);
                grid.set(gx, gy, h * (1.0 - t) + target * t);
            }
        }
    }
}

/// The full stage: compress horizontally first (so a footprint mask is
/// painted at, and applies to, the *post-compression* game-world grid, not
/// the raw ingest resolution), then vertical curve + scale, then the
/// capital-flatten hand-mask blend.
pub fn run_stylize_stage(grid: &Grid, config: &StylizeConfig, footprint: &FootprintMask) -> Grid {
    let mut out = compress_horizontal(grid, config.horizontal_scale);
    apply_vertical_curve(&mut out, config.vertical_curve_exp, config.vertical_scale);
    apply_capital_flatten(&mut out, footprint, config.capital_flatten_margin_m);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertical_curve_matches_hand_computed_points() {
        // h_max = 100, exp = 2, scale = 2: h' = (h/100)^2 * 100 * 2.
        let mut grid = Grid::new(4, 1, 10.0);
        grid.set(0, 0, 100.0);
        grid.set(1, 0, 50.0);
        grid.set(2, 0, 25.0);
        grid.set(3, 0, 0.0);
        apply_vertical_curve(&mut grid, 2.0, 2.0);
        let expected = [200.0, 50.0, 12.5, 0.0];
        for (i, &e) in expected.iter().enumerate() {
            assert!((grid.get(i, 0) - e).abs() < 1e-3, "cell {i}: got {}, expected {e}", grid.get(i, 0));
        }
    }

    #[test]
    fn vertical_curve_is_a_no_op_at_default_config() {
        let mut grid = Grid::new(3, 1, 10.0);
        grid.set(0, 0, 12.3);
        grid.set(1, 0, -4.0);
        grid.set(2, 0, 287.0);
        let before = grid.clone();
        apply_vertical_curve(&mut grid, 1.0, 1.0);
        for i in 0..3 {
            assert!((grid.get(i, 0) - before.get(i, 0)).abs() < 1e-4);
        }
    }

    #[test]
    fn horizontal_compression_shrinks_extent_and_preserves_a_linear_ramp() {
        let mut grid = Grid::new(10, 4, 10.0);
        for gy in 0..4 {
            for gx in 0..10 {
                grid.set(gx, gy, gx as f32 * 10.0); // height = world x in meters
            }
        }
        let compressed = compress_horizontal(&grid, 0.5);
        assert_eq!(compressed.width, 5);
        assert_eq!(compressed.height, 2);
        // The ramp's shape survives resampling: still increasing west->east,
        // spanning roughly the same value range.
        assert!(compressed.get(0, 0) < compressed.get(4, 0));
    }

    #[test]
    fn horizontal_compression_is_a_no_op_at_scale_one() {
        let mut grid = Grid::new(3, 3, 10.0);
        grid.set(1, 1, 42.0);
        let out = compress_horizontal(&grid, 1.0);
        assert_eq!(out, grid);
    }

    #[test]
    fn mask_compression_matches_grid_compression_dimensions_and_stays_nearest_neighbor() {
        let mut mask = WaterMask::new(10, 4);
        for gy in 0..4 {
            for gx in 5..10 {
                mask.set(gx, gy, true); // east half is water
            }
        }
        let compressed = compress_mask_horizontal(&mask, 0.5);
        assert_eq!((compressed.width, compressed.height), (5, 2));
        // Still reads as land in the west, water in the east half.
        assert!(!compressed.get(0, 0));
        assert!(compressed.get(4, 0));
    }

    #[test]
    fn capital_flatten_variance_is_near_zero_inside_the_footprint() {
        let mut grid = Grid::new(20, 20, 10.0);
        for gy in 0..20 {
            for gx in 0..20 {
                grid.set(gx, gy, (gx * 7 + gy * 13) as f32 % 50.0); // noisy natural terrain
            }
        }
        let mut footprint = FootprintMask::none(20, 20);
        for gy in 8..12 {
            for gx in 8..12 {
                footprint.set(gx, gy, true);
            }
        }
        apply_capital_flatten(&mut grid, &footprint, 30.0);

        let heights: Vec<f32> = (8..12).flat_map(|gy| (8..12).map(move |gx| (gx, gy))).map(|(gx, gy)| grid.get(gx, gy)).collect();
        let mean = heights.iter().sum::<f32>() / heights.len() as f32;
        let variance = heights.iter().map(|h| (h - mean).powi(2)).sum::<f32>() / heights.len() as f32;
        assert!(variance < 1e-4, "footprint variance {variance} should be ~0 after flattening");
    }

    #[test]
    fn capital_flatten_blends_smoothly_not_as_a_stepped_cliff() {
        let mut grid = Grid::new(20, 1, 10.0);
        for gx in 0..20 {
            grid.set(gx, 0, 100.0); // uniform natural height, footprint target will differ
        }
        grid.set(2, 0, 40.0); // the footprint's own (lower) natural height
        let mut footprint = FootprintMask::none(20, 1);
        footprint.set(2, 0, true);
        apply_capital_flatten(&mut grid, &footprint, 50.0); // margin_m=50 -> 5 cells

        // Walking away from the footprint, height should rise *monotonically*
        // and *gradually* back toward 100 — no jump straight from the target
        // to the untouched natural value partway through the margin.
        let values: Vec<f32> = (2..10).map(|gx| grid.get(gx, 0)).collect();
        for w in values.windows(2) {
            assert!(w[1] >= w[0] - 1e-4, "should rise monotonically moving away from the footprint: {values:?}");
        }
        // No single step accounts for more than half the total rise (i.e. not
        // a cliff — the design doc's actual complaint).
        let total_rise = values.last().unwrap() - values[0];
        for w in values.windows(2) {
            let step = w[1] - w[0];
            assert!(step < total_rise * 0.5 + 1e-4, "step {step} too large a fraction of total rise {total_rise}: {values:?}");
        }
        // Well beyond the margin, it's back to fully natural.
        assert!((grid.get(19, 0) - 100.0).abs() < 1e-3);
    }

    #[test]
    fn capital_flatten_is_a_no_op_with_no_footprint() {
        let mut grid = Grid::new(5, 5, 10.0);
        grid.set(2, 2, 55.0);
        let before = grid.clone();
        let footprint = FootprintMask::none(5, 5);
        apply_capital_flatten(&mut grid, &footprint, 100.0);
        assert_eq!(grid, before);
    }
}
