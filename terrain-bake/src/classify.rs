//! Classification (design doc §5.6): a biome id and a nav-flag bitfield for
//! every cell, from height/slope/distance-to-water plus an ordered rule
//! list — then a hand-painted override gets the final word. Landed after
//! export (#62) already shipped a placeholder biome/nav-from-mask-only
//! implementation; this replaces that with the real thing.

use std::collections::VecDeque;

use terrain_common::nav;

use crate::config::{ClassifyConfig, ClassifyRule};
use crate::grid::Grid;
use crate::water::WaterMask;

/// The v1 biome registry (design doc §5.6's exact example set). A named,
/// closed set for now — not user-extensible via config — since nothing
/// downstream (the classification rules, the debug-dump palette, the
/// server/client) knows about biomes it can't already name.
pub const BIOME_WATER: u8 = 0;
pub const BIOME_MANGROVE: u8 = 1;
pub const BIOME_BEACH: u8 = 2;
pub const BIOME_FOREST: u8 = 3;
pub const BIOME_PLAINS: u8 = 4;
/// One past the last valid id — also the boundary [`BiomeOverrideMask`]'s
/// PNG convention uses to mean "no override" (see its own doc comment).
pub const BIOME_COUNT: u8 = 5;

fn biome_id(name: &str) -> Option<u8> {
    match name {
        "water" => Some(BIOME_WATER),
        "mangrove" => Some(BIOME_MANGROVE),
        "beach" => Some(BIOME_BEACH),
        "forest" => Some(BIOME_FOREST),
        "plains" => Some(BIOME_PLAINS),
        _ => None,
    }
}

/// Nav flags (bitfield) and biome id for every cell — the metadata-tile
/// payload (design doc §6), before it's packed into `terrain_common::MetaTile`s.
#[derive(Debug, Clone, PartialEq)]
pub struct Classification {
    pub width: usize,
    pub height: usize,
    pub biome: Vec<u8>,
    pub nav_flags: Vec<u8>,
}

impl Classification {
    pub fn biome_at(&self, gx: usize, gy: usize) -> u8 {
        self.biome[gy * self.width + gx]
    }

    pub fn nav_flags_at(&self, gx: usize, gy: usize) -> u8 {
        self.nav_flags[gy * self.width + gx]
    }

    /// A `Classification` that just mirrors a water mask (biome water/plains
    /// only, nav flags water vs. walkable+buildable) — for callers that
    /// don't need real classification (mainly export's own tests, from
    /// before this stage existed).
    pub fn from_water_mask(mask: &WaterMask) -> Classification {
        let mut biome = vec![BIOME_PLAINS; mask.width * mask.height];
        let mut nav_flags = vec![0u8; mask.width * mask.height];
        for gy in 0..mask.height {
            for gx in 0..mask.width {
                let idx = gy * mask.width + gx;
                if mask.get(gx, gy) {
                    biome[idx] = BIOME_WATER;
                    nav_flags[idx] = nav::WATER;
                } else {
                    nav_flags[idx] = nav::WALKABLE | nav::BUILDABLE;
                }
            }
        }
        Classification { width: mask.width, height: mask.height, biome, nav_flags }
    }
}

/// Hand-painted biome override (design doc's indexed-palette
/// `biome_overrides.png`, simplified to a grayscale convention: pixel value
/// `0..BIOME_COUNT` overrides to that biome id; anything else (including the
/// common `255` "unpainted" white) means no override).
#[derive(Debug, Clone, PartialEq)]
pub struct BiomeOverrideMask {
    pub width: usize,
    pub height: usize,
    cells: Vec<Option<u8>>,
}

impl BiomeOverrideMask {
    pub fn none(width: usize, height: usize) -> Self {
        BiomeOverrideMask { width, height, cells: vec![None; width * height] }
    }

    pub fn get(&self, gx: usize, gy: usize) -> Option<u8> {
        self.cells[gy * self.width + gx]
    }

    pub fn set(&mut self, gx: usize, gy: usize, biome: Option<u8>) {
        self.cells[gy * self.width + gx] = biome;
    }

    pub fn from_luma_png(path: &std::path::Path) -> Result<BiomeOverrideMask, image::ImageError> {
        let img = image::open(path)?.to_luma8();
        let (width, height) = (img.width() as usize, img.height() as usize);
        let mut mask = BiomeOverrideMask::none(width, height);
        for gy in 0..height {
            for gx in 0..width {
                let v = img.get_pixel(gx as u32, gy as u32)[0];
                if v < BIOME_COUNT {
                    mask.set(gx, gy, Some(v));
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

/// Multi-source BFS distance (in cells) from the nearest water cell — same
/// technique as `stylize::distance_from_footprint`, sourced from water
/// instead.
fn distance_from_water(mask: &WaterMask) -> Vec<f32> {
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

fn slope_deg_at(grid: &Grid, gx: usize, gy: usize) -> f32 {
    let h = grid.get(gx, gy);
    let hx = grid.get((gx + 1).min(grid.width - 1), gy);
    let hy = grid.get(gx, (gy + 1).min(grid.height - 1));
    let dzdx = (hx - h) / grid.cell_size_m;
    let dzdy = (hy - h) / grid.cell_size_m;
    (dzdx * dzdx + dzdy * dzdy).sqrt().atan().to_degrees()
}

fn rule_matches(rule: &ClassifyRule, height: f32, slope_deg: f32, water_dist_m: f32) -> bool {
    if let Some(v) = rule.min_height {
        if height < v {
            return false;
        }
    }
    if let Some(v) = rule.max_height {
        if height > v {
            return false;
        }
    }
    if let Some(v) = rule.min_slope {
        if slope_deg < v {
            return false;
        }
    }
    if let Some(v) = rule.max_slope {
        if slope_deg > v {
            return false;
        }
    }
    if let Some(v) = rule.min_water_dist {
        if water_dist_m < v {
            return false;
        }
    }
    if let Some(v) = rule.max_water_dist {
        if water_dist_m > v {
            return false;
        }
    }
    true
}

/// First matching rule wins; no match (including an empty rule list) falls
/// back to plains.
fn classify_cell(height: f32, slope_deg: f32, water_dist_m: f32, rules: &[ClassifyRule]) -> u8 {
    for rule in rules {
        if rule_matches(rule, height, slope_deg, water_dist_m) {
            return biome_id(&rule.biome).unwrap_or(BIOME_PLAINS);
        }
    }
    BIOME_PLAINS
}

/// The full stage: classify every cell from height/slope/water-distance via
/// the ordered rule list, force the water biome wherever `mask` says water
/// (see below), let `overrides` win where painted, and compute nav flags
/// from the water mask + `walkable_max_slope` (v1: buildable == walkable —
/// no separate buildability rule yet; that's the land/rent system's call to
/// refine later).
///
/// `mask` and `grid` can disagree about "is this cell water": the mask is
/// computed early (design doc's ingest -> water ordering), while `grid` is
/// whatever later stages (stylize/detail/erosion) leave behind, and erosion
/// in particular can raise a cell's height back above a rule's water
/// threshold by redistributing mass from a neighbor. Since the mask is nav's
/// source of truth (a mask-water cell is always nav-flagged `WATER`,
/// unconditionally, below), letting the rule list independently re-derive
/// "water" from post-erosion height risks a cell that's impassable water for
/// navigation but labeled a land biome for rendering — so the mask wins the
/// biome call too, before `overrides` gets the final word.
pub fn run_classify_stage(
    grid: &Grid,
    mask: &WaterMask,
    config: &ClassifyConfig,
    overrides: &BiomeOverrideMask,
) -> Classification {
    let water_dist_cells = distance_from_water(mask);
    let mut biome = vec![BIOME_PLAINS; grid.width * grid.height];
    let mut nav_flags = vec![0u8; grid.width * grid.height];

    for gy in 0..grid.height {
        for gx in 0..grid.width {
            let idx = gy * grid.width + gx;
            let height = grid.get(gx, gy);
            let slope_deg = slope_deg_at(grid, gx, gy);
            let water_dist_m = water_dist_cells[idx] * grid.cell_size_m;
            let is_water = mask.get(gx, gy);

            let mut b = if is_water { BIOME_WATER } else { classify_cell(height, slope_deg, water_dist_m, &config.rules) };
            if gx < overrides.width && gy < overrides.height {
                if let Some(ov) = overrides.get(gx, gy) {
                    b = ov;
                }
            }
            biome[idx] = b;

            let walkable = !is_water && slope_deg <= config.walkable_max_slope;
            let mut flags = 0u8;
            if is_water {
                flags |= nav::WATER;
            }
            if walkable {
                flags |= nav::WALKABLE | nav::BUILDABLE;
            }
            nav_flags[idx] = flags;
        }
    }

    Classification { width: grid.width, height: grid.height, biome, nav_flags }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(biome: &str) -> ClassifyRule {
        ClassifyRule {
            biome: biome.to_string(),
            min_height: None,
            max_height: None,
            min_slope: None,
            max_slope: None,
            min_water_dist: None,
            max_water_dist: None,
        }
    }

    fn design_doc_rules() -> Vec<ClassifyRule> {
        vec![
            ClassifyRule { max_height: Some(0.0), ..rule("water") },
            ClassifyRule { max_height: Some(4.0), max_water_dist: Some(400.0), ..rule("mangrove") },
            ClassifyRule { max_height: Some(6.0), max_water_dist: Some(150.0), ..rule("beach") },
            ClassifyRule { min_slope: Some(12.0), ..rule("forest") },
            rule("plains"),
        ]
    }

    #[test]
    fn each_rule_fires_in_priority_order() {
        let rules = design_doc_rules();
        // (height, slope_deg, water_dist_m, expected biome)
        let cases: &[(f32, f32, f32, u8)] = &[
            (-1.0, 0.0, 1000.0, BIOME_WATER),           // height rule wins regardless of distance
            (2.0, 0.0, 300.0, BIOME_MANGROVE),          // low + close to water
            (2.0, 0.0, 500.0, BIOME_PLAINS),            // low height but too far for mangrove, falls through to plains (slope too low for forest)
            (5.0, 0.0, 100.0, BIOME_BEACH),              // beach: taller than mangrove range, close to water
            (10.0, 20.0, 1000.0, BIOME_FOREST),          // steep, far from water
            (10.0, 0.0, 1000.0, BIOME_PLAINS),           // none of the special rules match -> default
        ];
        for &(height, slope, water_dist, expected) in cases {
            let got = classify_cell(height, slope, water_dist, &rules);
            assert_eq!(got, expected, "height={height} slope={slope} water_dist={water_dist}");
        }
    }

    #[test]
    fn empty_rules_falls_back_to_plains() {
        assert_eq!(classify_cell(-100.0, 90.0, 0.0, &[]), BIOME_PLAINS);
    }

    fn flat_grid(width: usize, height: usize, cell_size_m: f32, h: f32) -> Grid {
        let mut g = Grid::new(width, height, cell_size_m);
        for gy in 0..height {
            for gx in 0..width {
                g.set(gx, gy, h);
            }
        }
        g
    }

    #[test]
    fn mask_water_wins_the_biome_even_when_post_erosion_height_disagrees() {
        // Regression for a real full-pipeline finding (issue #68's
        // validation harness): erosion runs after the water mask is
        // computed and can raise a cell's height back above a rule's water
        // threshold, so classifying biome purely from height/slope/water-
        // dist can disagree with the mask -- leaving a nav-flagged-WATER
        // cell labeled a land biome. The mask must win the biome call too.
        let grid = flat_grid(4, 4, 10.0, 50.0); // well above any height-based "water" rule
        let mut mask = WaterMask::new(4, 4);
        mask.set(1, 1, true);
        let config = ClassifyConfig { rules: design_doc_rules(), override_map: None, walkable_max_slope: 60.0 };
        let overrides = BiomeOverrideMask::none(4, 4);

        let out = run_classify_stage(&grid, &mask, &config, &overrides);
        assert_eq!(out.biome_at(1, 1), BIOME_WATER, "mask-water cell must read back as the water biome regardless of height");
        assert_eq!(out.biome_at(0, 0), BIOME_PLAINS, "non-water cells still use the rule list");
    }

    #[test]
    fn biome_override_wins_over_the_rule_derived_biome() {
        let grid = flat_grid(4, 4, 10.0, 10.0); // height 10 -> "plains" under design_doc_rules with slope 0
        let mask = WaterMask::new(4, 4);
        let config = ClassifyConfig { rules: design_doc_rules(), override_map: None, walkable_max_slope: 60.0 };
        let mut overrides = BiomeOverrideMask::none(4, 4);
        overrides.set(1, 1, Some(BIOME_FOREST));

        let out = run_classify_stage(&grid, &mask, &config, &overrides);
        assert_eq!(out.biome_at(1, 1), BIOME_FOREST, "override must win");
        assert_eq!(out.biome_at(0, 0), BIOME_PLAINS, "unpainted cells still use the rule list");
    }

    #[test]
    fn nav_sanity_capital_footprint_is_fully_walkable_and_buildable() {
        // A flat footprint (as capital_flatten leaves it) with no water --
        // must classify 100% walkable + buildable, design doc §8's nav
        // sanity check.
        let grid = flat_grid(20, 20, 10.0, 5.0);
        let mask = WaterMask::new(20, 20);
        let config = ClassifyConfig::default();
        let overrides = BiomeOverrideMask::none(20, 20);
        let out = run_classify_stage(&grid, &mask, &config, &overrides);
        for gy in 5..15 {
            for gx in 5..15 {
                let flags = out.nav_flags_at(gx, gy);
                assert_eq!(
                    flags & (nav::WALKABLE | nav::BUILDABLE),
                    nav::WALKABLE | nav::BUILDABLE,
                    "footprint cell ({gx},{gy}) must be walkable+buildable"
                );
            }
        }
    }

    #[test]
    fn nav_sanity_water_stays_contiguous_through_classification() {
        // Mirrors water.rs's own edge-connectivity test, re-checked through
        // the full classified nav-flag output (design doc §8): an
        // edge-to-edge channel must read as WATER at every cell along it.
        let grid = flat_grid(7, 5, 10.0, 5.0);
        let mut mask = WaterMask::new(7, 5);
        for gy in 0..5 {
            for gx in 2..5 {
                mask.set(gx, gy, true);
            }
        }
        let config = ClassifyConfig::default();
        let overrides = BiomeOverrideMask::none(7, 5);
        let out = run_classify_stage(&grid, &mask, &config, &overrides);
        for gy in 0..5 {
            for gx in 2..5 {
                assert_ne!(out.nav_flags_at(gx, gy) & nav::WATER, 0, "({gx},{gy}) should be nav-flagged water");
            }
        }
    }

    #[test]
    fn classification_is_deterministic() {
        let grid = flat_grid(10, 10, 10.0, 8.0);
        let mask = WaterMask::new(10, 10);
        let config = ClassifyConfig { rules: design_doc_rules(), override_map: None, walkable_max_slope: 60.0 };
        let overrides = BiomeOverrideMask::none(10, 10);
        let a = run_classify_stage(&grid, &mask, &config, &overrides);
        let b = run_classify_stage(&grid, &mask, &config, &overrides);
        assert_eq!(a, b);
    }
}
