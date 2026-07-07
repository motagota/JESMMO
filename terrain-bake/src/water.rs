//! Coastline & water mask (design doc §5.2): threshold at sea level, clean
//! up shoreline noise, separate real ocean/bay from inland depressions via
//! edge flood-fill, then let a hand-painted override mask have the final
//! word (forcing water for a river the algorithm wouldn't otherwise keep at
//! its full navigable width, or forcing land to simplify mangrove sprawl
//! into gameplay-sized islands).

use crate::grid::Grid;

#[derive(Debug, Clone, PartialEq)]
pub struct WaterMask {
    pub width: usize,
    pub height: usize,
    pub cells: Vec<bool>,
}

impl WaterMask {
    pub fn new(width: usize, height: usize) -> Self {
        WaterMask { width, height, cells: vec![false; width * height] }
    }

    pub fn get(&self, gx: usize, gy: usize) -> bool {
        self.cells[gy * self.width + gx]
    }

    pub fn set(&mut self, gx: usize, gy: usize, v: bool) {
        self.cells[gy * self.width + gx] = v;
    }

    /// A cell is provisionally water if its height is at or below sea level.
    pub fn threshold(grid: &Grid, sea_level_m: f32) -> WaterMask {
        let mut mask = WaterMask::new(grid.width, grid.height);
        for gy in 0..grid.height {
            for gx in 0..grid.width {
                mask.set(gx, gy, grid.get(gx, gy) <= sea_level_m);
            }
        }
        mask
    }

    fn neighbors8(&self, gx: usize, gy: usize) -> impl Iterator<Item = (usize, usize)> + '_ {
        let (w, h) = (self.width as i64, self.height as i64);
        let (x, y) = (gx as i64, gy as i64);
        (-1..=1).flat_map(move |dy| (-1..=1).map(move |dx| (dx, dy))).filter_map(move |(dx, dy)| {
            if dx == 0 && dy == 0 {
                return None;
            }
            let (nx, ny) = (x + dx, y + dy);
            if nx >= 0 && nx < w && ny >= 0 && ny < h {
                Some((nx as usize, ny as usize))
            } else {
                None
            }
        })
    }

    /// Grow the water region by one cell wherever a non-water cell touches a
    /// water one (8-connected).
    fn dilate_once(&self) -> WaterMask {
        let mut out = self.clone();
        for gy in 0..self.height {
            for gx in 0..self.width {
                if !self.get(gx, gy) && self.neighbors8(gx, gy).any(|(nx, ny)| self.get(nx, ny)) {
                    out.set(gx, gy, true);
                }
            }
        }
        out
    }

    fn inverted(&self) -> WaterMask {
        WaterMask { width: self.width, height: self.height, cells: self.cells.iter().map(|c| !c).collect() }
    }

    fn dilate(&self, radius: u32) -> WaterMask {
        let mut cur = self.clone();
        for _ in 0..radius {
            cur = cur.dilate_once();
        }
        cur
    }

    /// Erosion is dilation of the complement, inverted back — shrinks water
    /// by `radius` cells instead of growing it.
    fn erode(&self, radius: u32) -> WaterMask {
        self.inverted().dilate(radius).inverted()
    }

    /// Opening (erode then dilate): removes water regions narrower than
    /// `radius` — kills single-cell water speckles.
    pub fn open(&self, radius: u32) -> WaterMask {
        self.erode(radius).dilate(radius)
    }

    /// Closing (dilate then erode): fills land gaps narrower than
    /// `radius` inside a water body — kills single-cell land speckles
    /// along an otherwise-clean shoreline.
    pub fn close(&self, radius: u32) -> WaterMask {
        self.dilate(radius).erode(radius)
    }

    /// Flood-fill from every edge cell that's water: only edge-reachable
    /// water survives as "ocean/bay". A water cell that's *not* reachable
    /// from any edge is an inland depression, not open water, and gets
    /// dropped here (the caller then clamps its height so it doesn't render
    /// as an accidental sub-sea-level lake).
    pub fn flood_fill_from_edges(&self) -> WaterMask {
        let mut reached = vec![false; self.width * self.height];
        let mut stack = Vec::new();
        for gx in 0..self.width {
            for gy in [0, self.height.saturating_sub(1)] {
                if self.get(gx, gy) {
                    stack.push((gx, gy));
                }
            }
        }
        for gy in 0..self.height {
            for gx in [0, self.width.saturating_sub(1)] {
                if self.get(gx, gy) {
                    stack.push((gx, gy));
                }
            }
        }
        while let Some((gx, gy)) = stack.pop() {
            let idx = gy * self.width + gx;
            if reached[idx] {
                continue;
            }
            reached[idx] = true;
            for (nx, ny) in self.neighbors8(gx, gy) {
                if self.get(nx, ny) && !reached[ny * self.width + nx] {
                    stack.push((nx, ny));
                }
            }
        }
        WaterMask { width: self.width, height: self.height, cells: reached }
    }

    /// Force cells to water/land per `overrides`, winning over whatever the
    /// derived mask says (design doc §5.2 hand-mask).
    pub fn apply_override(&mut self, overrides: &OverrideMask) {
        for gy in 0..self.height {
            for gx in 0..self.width {
                match overrides.get(gx, gy) {
                    OverrideCell::ForceWater => self.set(gx, gy, true),
                    OverrideCell::ForceLand => self.set(gx, gy, false),
                    OverrideCell::None => {}
                }
            }
        }
    }

    /// Any cell the mask calls land, but whose height is still at/below sea
    /// level (an inland depression that lost the flood-fill), gets pulled
    /// up to `sea_level_m + epsilon` — no accidental below-sea-level lakes.
    pub fn clamp_land_heights(&self, grid: &mut Grid, sea_level_m: f32, epsilon: f32) {
        for gy in 0..self.height {
            for gx in 0..self.width {
                if !self.get(gx, gy) && grid.get(gx, gy) <= sea_level_m {
                    grid.set(gx, gy, sea_level_m + epsilon);
                }
            }
        }
    }
}

/// A hand-painted override decision for one cell (design doc §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideCell {
    None,
    ForceWater,
    ForceLand,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OverrideMask {
    pub width: usize,
    pub height: usize,
    pub cells: Vec<OverrideCell>,
}

impl OverrideMask {
    pub fn none(width: usize, height: usize) -> Self {
        OverrideMask { width, height, cells: vec![OverrideCell::None; width * height] }
    }

    pub fn get(&self, gx: usize, gy: usize) -> OverrideCell {
        self.cells[gy * self.width + gx]
    }

    pub fn set(&mut self, gx: usize, gy: usize, v: OverrideCell) {
        self.cells[gy * self.width + gx] = v;
    }

    /// Load from a grayscale PNG: `255` = force land, `0` = force water,
    /// anything else = no override. Documented convention (design doc §5.2's
    /// `bay_cleanup.png`) — keep any future hand-authored mask consistent
    /// with it.
    pub fn from_luma_png(path: &std::path::Path) -> Result<OverrideMask, image::ImageError> {
        let img = image::open(path)?.to_luma8();
        let (width, height) = (img.width() as usize, img.height() as usize);
        let mut mask = OverrideMask::none(width, height);
        for gy in 0..height {
            for gx in 0..width {
                let v = img.get_pixel(gx as u32, gy as u32)[0];
                let cell = match v {
                    255 => OverrideCell::ForceLand,
                    0 => OverrideCell::ForceWater,
                    _ => OverrideCell::None,
                };
                mask.set(gx, gy, cell);
            }
        }
        Ok(mask)
    }
}

/// The full stage (design doc §5.2), run in place on `grid`: threshold,
/// open+close to clean shoreline noise, flood-fill from the edges to drop
/// inland depressions, apply hand-mask overrides, then clamp the heights of
/// whatever's left as land but still at/below sea level. Returns the final
/// mask.
pub fn run_water_mask_stage(grid: &mut Grid, config: &crate::config::WaterConfig, overrides: &OverrideMask) -> WaterMask {
    let raw = WaterMask::threshold(grid, config.sea_level_m);
    let cleaned = raw.open(config.open_close_radius).close(config.open_close_radius);
    let mut mask = cleaned.flood_fill_from_edges();
    mask.apply_override(overrides);
    mask.clamp_land_heights(grid, config.sea_level_m, config.clamp_epsilon_m);
    mask
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 7x7 grid: all above sea level except a single below-sea-level
    /// interior cell (an inland depression, not connected to any edge).
    fn grid_with_inland_depression() -> Grid {
        let mut g = Grid::new(7, 7, 10.0);
        for gy in 0..7 {
            for gx in 0..7 {
                g.set(gx, gy, 10.0);
            }
        }
        g.set(3, 3, -2.0); // isolated depression
        g
    }

    #[test]
    fn isolated_inland_depression_is_clamped_not_left_as_water() {
        let mut grid = grid_with_inland_depression();
        let config = crate::config::WaterConfig::default();
        let overrides = OverrideMask::none(grid.width, grid.height);
        let mask = run_water_mask_stage(&mut grid, &config, &overrides);

        assert!(!mask.get(3, 3), "an unreachable depression must not read as water");
        assert!(grid.get(3, 3) > config.sea_level_m, "its height must be clamped above sea level");
        assert!((grid.get(3, 3) - (config.sea_level_m + config.clamp_epsilon_m)).abs() < 1e-6);
    }

    #[test]
    fn edge_connected_water_survives_as_ocean() {
        // A channel from the top edge to the bottom edge — must survive
        // flood-fill as real (edge-connected) water, unlike the isolated
        // depression above. Three cells wide (not one) so the open/close
        // shoreline-noise cleanup (which *should* erase a single-cell-wide
        // speckle — that's its job, see `hand_mask_guarantees_a_minimum_
        // river_width` for the "but I actually wanted this river" case)
        // doesn't erase it too; this test is specifically about
        // edge-connectivity surviving flood-fill, not about width.
        let mut g = Grid::new(7, 5, 10.0);
        for gy in 0..5 {
            for gx in 0..7 {
                g.set(gx, gy, 10.0);
            }
        }
        for gy in 0..5 {
            for gx in 2..5 {
                g.set(gx, gy, -1.0);
            }
        }
        let config = crate::config::WaterConfig::default();
        let overrides = OverrideMask::none(g.width, g.height);
        let mask = run_water_mask_stage(&mut g, &config, &overrides);
        for gy in 0..5 {
            for gx in 2..5 {
                assert!(mask.get(gx, gy), "edge-to-edge channel cell ({gx},{gy}) should be water");
            }
        }
    }

    #[test]
    fn hand_mask_override_wins_regardless_of_derived_mask() {
        let mut grid = grid_with_inland_depression(); // (3,3) would otherwise be clamped to land
        let config = crate::config::WaterConfig::default();
        let mut overrides = OverrideMask::none(grid.width, grid.height);
        overrides.set(3, 3, OverrideCell::ForceWater);
        // Also force an ordinarily-dry cell to land, to prove both directions.
        overrides.set(0, 0, OverrideCell::ForceLand);

        let mask = run_water_mask_stage(&mut grid, &config, &overrides);
        assert!(mask.get(3, 3), "ForceWater must override the flood-fill result");
        assert!(!mask.get(0, 0), "ForceLand must override an above-sea-level cell staying land (trivially true, but exercises the code path)");
    }

    #[test]
    fn hand_mask_guarantees_a_minimum_river_width() {
        // A single-cell-wide natural channel is narrower than
        // `min_river_width_m` — on real data this is exactly what the
        // design doc's hand-mask channel-widen pass exists to fix. Painting
        // a `min_river_width_m`-wide override band around the channel must
        // produce a contiguous water run of at least that width at every
        // row along the channel.
        let cell_size_m = 10.0;
        let cols = 9;
        let rows = 6;
        let mut g = Grid::new(cols, rows, cell_size_m);
        for gy in 0..rows {
            for gx in 0..cols {
                g.set(gx, gy, 10.0);
            }
        }
        let channel_x = 4;
        for gy in 0..rows {
            g.set(channel_x, gy, -1.0); // the narrow natural channel
        }

        let config = crate::config::WaterConfig { min_river_width_m: 30.0, ..crate::config::WaterConfig::default() };
        let half_width_cells = ((config.min_river_width_m / cell_size_m) / 2.0).ceil() as i64;
        let mut overrides = OverrideMask::none(cols, rows);
        for gy in 0..rows {
            for dx in -half_width_cells..=half_width_cells {
                let gx = channel_x as i64 + dx;
                if gx >= 0 && (gx as usize) < cols {
                    overrides.set(gx as usize, gy, OverrideCell::ForceWater);
                }
            }
        }

        let mask = run_water_mask_stage(&mut g, &config, &overrides);
        let min_width_cells = (config.min_river_width_m / cell_size_m).round() as usize;
        for gy in 0..rows {
            let run = (0..cols).filter(|&gx| mask.get(gx, gy)).count();
            assert!(run >= min_width_cells, "row {gy}: water run {run} cells, need >= {min_width_cells}");
        }
    }
}
