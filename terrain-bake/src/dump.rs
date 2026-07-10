//! `--debug-dump`: writes a hillshade PNG of a working grid for visual
//! review between stages (design doc §4, §8).

use std::path::Path;

use crate::classify::{self, Classification};
use crate::grid::Grid;
use crate::water::WaterMask;

/// Fixed light direction for the shading, normalized: mostly overhead with a
/// bit of raking angle from the northwest so relief actually reads. Not
/// configurable — this is a debug aid, not rendered output.
const LIGHT: (f32, f32, f32) = (-0.5, -0.5, 0.7);

pub fn write_hillshade_png(grid: &Grid, path: &Path) -> Result<(), image::ImageError> {
    let light_len = (LIGHT.0 * LIGHT.0 + LIGHT.1 * LIGHT.1 + LIGHT.2 * LIGHT.2).sqrt();
    let light = (LIGHT.0 / light_len, LIGHT.1 / light_len, LIGHT.2 / light_len);

    let mut img = image::GrayImage::new(grid.width as u32, grid.height as u32);
    for gy in 0..grid.height {
        for gx in 0..grid.width {
            let h = grid.get(gx, gy);
            let hx = grid.get((gx + 1).min(grid.width - 1), gy);
            let hy = grid.get(gx, (gy + 1).min(grid.height - 1));
            let dzdx = (hx - h) / grid.cell_size_m;
            let dzdy = (hy - h) / grid.cell_size_m;

            // Surface normal of the plane implied by the two forward
            // differences, normalized.
            let nx = -dzdx;
            let ny = -dzdy;
            let nz = 1.0;
            let n_len = (nx * nx + ny * ny + nz * nz).sqrt();
            let (nx, ny, nz) = (nx / n_len, ny / n_len, nz / n_len);

            let dot = (nx * light.0 + ny * light.1 + nz * light.2).max(0.0);
            let shade = (dot * 255.0).round().clamp(0.0, 255.0) as u8;
            img.put_pixel(gx as u32, gy as u32, image::Luma([shade]));
        }
    }
    img.save(path)
}

/// Binary black/white water-mask dump (design doc §8): black = water,
/// white = land — the same convention `water::OverrideMask::from_luma_png`
/// reads hand-painted masks with, so a dumped mask and a hand-authored one
/// look the same way round in an image editor.
pub fn write_water_mask_png(mask: &WaterMask, path: &Path) -> Result<(), image::ImageError> {
    let mut img = image::GrayImage::new(mask.width as u32, mask.height as u32);
    for gy in 0..mask.height {
        for gx in 0..mask.width {
            let v = if mask.get(gx, gy) { 0 } else { 255 };
            img.put_pixel(gx as u32, gy as u32, image::Luma([v]));
        }
    }
    img.save(path)
}

/// Palette-colored biome-map dump (design doc §5.6/§8): one flat color per
/// biome id, matching the registry order in `classify.rs`, so a glance at the
/// image tells you which rule fired without cross-referencing the legend.
fn biome_color(biome: u8) -> [u8; 3] {
    match biome {
        classify::BIOME_WATER => [40, 90, 200],
        classify::BIOME_MANGROVE => [80, 120, 70],
        classify::BIOME_BEACH => [230, 210, 150],
        classify::BIOME_FOREST => [30, 110, 40],
        classify::BIOME_PLAINS => [170, 200, 110],
        _ => [255, 0, 255], // unregistered id — should never happen, magenta so it's obvious if it does
    }
}

pub fn write_biome_map_png(classification: &Classification, path: &Path) -> Result<(), image::ImageError> {
    let mut img = image::RgbImage::new(classification.width as u32, classification.height as u32);
    for gy in 0..classification.height {
        for gx in 0..classification.width {
            let color = biome_color(classification.biome_at(gx, gy));
            img.put_pixel(gx as u32, gy as u32, image::Rgb(color));
        }
    }
    img.save(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_png_the_right_size() {
        let mut grid = Grid::new(8, 6, 10.0);
        for gy in 0..6 {
            for gx in 0..8 {
                grid.set(gx, gy, (gx + gy) as f32 * 3.0);
            }
        }
        let path = std::env::temp_dir().join(format!("terrain-bake-dump-test-{}.png", std::process::id()));
        write_hillshade_png(&grid, &path).unwrap();
        let img = image::open(&path).unwrap();
        assert_eq!((img.width(), img.height()), (8, 6));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn flat_ground_shades_uniformly() {
        // A perfectly flat grid has a straight-up normal everywhere, so
        // every pixel should shade identically (no false relief from a
        // degenerate slope calculation).
        let grid = Grid::new(4, 4, 10.0); // all zero heights
        let path = std::env::temp_dir().join(format!("terrain-bake-dump-flat-{}.png", std::process::id()));
        write_hillshade_png(&grid, &path).unwrap();
        let img = image::open(&path).unwrap().to_luma8();
        let first = img.get_pixel(0, 0)[0];
        assert!(img.pixels().all(|p| p[0] == first));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn water_mask_dump_is_binary_black_and_white() {
        let mut mask = WaterMask::new(3, 2);
        mask.set(1, 0, true);
        let path = std::env::temp_dir().join(format!("terrain-bake-dump-water-{}.png", std::process::id()));
        write_water_mask_png(&mask, &path).unwrap();
        let img = image::open(&path).unwrap().to_luma8();
        assert_eq!(img.get_pixel(1, 0)[0], 0, "water cell must be black");
        assert_eq!(img.get_pixel(0, 0)[0], 255, "land cell must be white");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn biome_map_dump_uses_a_distinct_color_per_biome() {
        let mask = WaterMask::new(2, 1);
        let mut classification = Classification::from_water_mask(&mask);
        classification.biome[0] = classify::BIOME_WATER;
        classification.biome[1] = classify::BIOME_FOREST;
        let path = std::env::temp_dir().join(format!("terrain-bake-dump-biome-{}.png", std::process::id()));
        write_biome_map_png(&classification, &path).unwrap();
        let img = image::open(&path).unwrap().to_rgb8();
        assert_ne!(img.get_pixel(0, 0), img.get_pixel(1, 0), "different biomes must render distinct colors");
        std::fs::remove_file(&path).ok();
    }
}
