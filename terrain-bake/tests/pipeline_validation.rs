//! Full-pipeline validation harness (issue #68, design doc §8). Unlike the
//! per-stage unit tests scattered through `src/*.rs` (which each exercise
//! their own stage against hand-built fixtures), this drives every stage in
//! sequence from a single config the way `main.rs`'s `Stage::All` does, so a
//! regression that only shows up from a *real* stage's output feeding the
//! next stage (as opposed to a hand-built fixture) has somewhere to be
//! caught. Four checks, matching issue #68's acceptance criteria exactly:
//! determinism, golden samples, seam, nav sanity — plus a fifth
//! (`real_dem_ingest_flows_through_every_stage_unmodified`) added for #69,
//! proving a real DEM needs zero changes to any of these stages.

use terrain_bake::classify::{self, BiomeOverrideMask};
use terrain_bake::config::{
    ClassifyConfig, ClassifyRule, Config, DetailConfig, ErosionConfig, ExportConfig, SourceConfig, StylizeConfig, WaterConfig,
};
use terrain_bake::grid::Grid;
use terrain_bake::water::OverrideMask;
use terrain_bake::{detail, dump, erosion, export, stylize, synth, water};

fn no_op_rule(biome: &str) -> ClassifyRule {
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

/// Small enough to run fast, but with more than one tile in each direction
/// (5x4 tiles) so the seam check is real, and a `sea_level_m` that's
/// guaranteed to intersect the synthetic profile's low east end (design doc
/// says it slopes to ~0 there) so there's real water to check nav sanity
/// against.
fn test_config(out_dir: &str) -> Config {
    Config {
        source: SourceConfig { dem_path: None, bounds_utm: [0.0, 0.0, 400.0, 200.0], working_res_m: 20.0, target_res_m: 20.0, seed: 99 },
        export: ExportConfig { tile_size: 5, out_dir: out_dir.to_string() },
        water: WaterConfig { sea_level_m: 0.0, open_close_radius: 1, min_river_width_m: 20.0, clamp_epsilon_m: 0.2, override_mask: None },
        stylize: StylizeConfig::default(),
        detail: DetailConfig { octaves: 2, base_amp_m: 0.5, slope_amp_curve: 1.5, lacunarity: 2.0, gain: 0.5, base_wavelength_m: 40.0, seed: 7 },
        erosion: ErosionConfig { enabled: true, max_natural_slope: 45.0, thermal_iters: 5, hydraulic_iters: 0, hydraulic_strength: 0.1 },
        classify: ClassifyConfig {
            rules: vec![ClassifyRule { max_height: Some(0.0), ..no_op_rule("water") }, no_op_rule("plains")],
            override_map: None,
            walkable_max_slope: 60.0,
        },
    }
}

/// Runs every stage in sequence, exactly like `main.rs`'s `Stage::All`, and
/// returns the final grid (for direct sampling) plus the exported artifact.
fn run_pipeline(config: &Config) -> (Grid, export::ExportedArtifact) {
    run_pipeline_from_grid(config, synth::synthesize(&config.source))
}

/// Same as [`run_pipeline`], but starting from an already-ingested grid —
/// lets the real-DEM test (#69) drive water/stylize/detail/erosion/
/// classify/export exactly the way every synthetic-fixture test here does,
/// proving those stages don't need to change for real data.
fn run_pipeline_from_grid(config: &Config, mut grid: Grid) -> (Grid, export::ExportedArtifact) {
    let overrides = OverrideMask::none(grid.width, grid.height);
    let mask = water::run_water_mask_stage(&mut grid, &config.water, &overrides);

    let footprint = stylize::FootprintMask::none(grid.width, grid.height);
    let grid = stylize::run_stylize_stage(&grid, &config.stylize, &footprint);
    let mask = stylize::compress_mask_horizontal(&mask, config.stylize.horizontal_scale);

    let detailed = detail::run_detail_stage(&grid, &mask, &footprint, &config.detail, config.source.target_res_m);
    let scale = grid.cell_size_m / config.source.target_res_m;
    let detailed_mask = stylize::compress_mask_horizontal(&mask, scale);

    let eroded = erosion::run_erosion_stage(&detailed, &config.erosion);

    let biome_overrides = BiomeOverrideMask::none(eroded.width, eroded.height);
    let classification = classify::run_classify_stage(&eroded, &detailed_mask, &config.classify, &biome_overrides);

    let artifact = export::export_artifact(&eroded, &classification, config);
    (eroded, artifact)
}

#[test]
fn full_pipeline_determinism() {
    // Design doc §8's determinism anchor, run end-to-end rather than at a
    // single stage: identical config in, identical bake_hash and
    // byte-identical tiles out.
    let dir_a = std::env::temp_dir().join(format!("terrain-bake-pv-det-a-{}", std::process::id()));
    let dir_b = std::env::temp_dir().join(format!("terrain-bake-pv-det-b-{}", std::process::id()));
    let config_a = test_config(dir_a.to_str().unwrap());
    let config_b = test_config(dir_b.to_str().unwrap());

    let (_, artifact_a) = run_pipeline(&config_a);
    let (_, artifact_b) = run_pipeline(&config_b);

    // bake_hash depends on `out_dir` (it's part of the serialized config),
    // so compare tile bytes and every other manifest field directly instead
    // of the raw hash string.
    assert_eq!(artifact_a.height_tiles.len(), artifact_b.height_tiles.len());
    for (a, b) in artifact_a.height_tiles.iter().zip(&artifact_b.height_tiles) {
        assert_eq!(a.encode(1), b.encode(1));
    }
    for (a, b) in artifact_a.meta_tiles.iter().zip(&artifact_b.meta_tiles) {
        assert_eq!(a.encode(1), b.encode(1));
    }
    assert_eq!(artifact_a.manifest.height_min_m, artifact_b.manifest.height_min_m);
    assert_eq!(artifact_a.manifest.height_max_m, artifact_b.manifest.height_max_m);
}

#[test]
fn golden_samples_lock_full_pipeline_output() {
    // A regression fixture, not a hand-derived table (unlike
    // terrain-common's own golden_fixture test) — this pipeline runs real
    // noise/erosion/classification stages, so "expected height" isn't
    // something a human re-derives by hand. These exact values were
    // captured from a real run of `test_config` and must not silently
    // drift: any future change to a stage this config exercises (synth,
    // water, stylize, detail, erosion, classify, export) that isn't a
    // deliberate, reviewed change will fail this test.
    let dir = std::env::temp_dir().join(format!("terrain-bake-pv-golden-{}", std::process::id()));
    let config = test_config(dir.to_str().unwrap());
    let (grid, artifact) = run_pipeline(&config);

    let cases: &[(usize, usize, f32)] = &[(0, 0, 314.44672), (10, 5, 102.2011), (19, 9, 41.226963)];
    for &(gx, gy, expected) in cases {
        let got = grid.get(gx, gy);
        assert!((got - expected).abs() < 0.01, "grid({gx},{gy}) = {got}, expected {expected}");
    }

    assert_eq!(artifact.manifest.tiles, (4, 2));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn seam_test_on_a_real_multi_tile_artifact() {
    // Same check as export.rs's own seam test, but against output that came
    // from the real pipeline (noise, erosion, classification all applied)
    // rather than a hand-built ramp grid — the acceptance criterion asks
    // for this run "against a real exported multi-tile artifact".
    let dir = std::env::temp_dir().join(format!("terrain-bake-pv-seam-{}", std::process::id()));
    let config = test_config(dir.to_str().unwrap());
    let (_, artifact) = run_pipeline(&config);

    let (tiles_x, tiles_y) = artifact.manifest.tiles;
    assert!(tiles_x > 1 && tiles_y > 1, "this test needs a real multi-tile grid to be meaningful");

    for ty in 0..tiles_y as i32 {
        for tx in 0..(tiles_x as i32 - 1) {
            let a = artifact.height_tiles.iter().find(|t| t.tile_x == tx && t.tile_y == ty).unwrap();
            let b = artifact.height_tiles.iter().find(|t| t.tile_x == tx + 1 && t.tile_y == ty).unwrap();
            for gy in 0..a.side {
                assert_eq!(a.get(a.side - 1, gy), b.get(0, gy), "height seam mismatch at tile ({tx},{ty})/({},{ty}) row {gy}", tx + 1);
            }
        }
    }
}

#[test]
fn nav_sanity_on_the_real_exported_and_reloaded_artifact() {
    // Round-trips through disk (write + `terrain_common::Terrain::load_dir`)
    // so this exercises exactly the path the server/client use, not just the
    // in-memory `Classification` classify.rs's own unit tests already cover.
    let dir = std::env::temp_dir().join(format!("terrain-bake-pv-nav-{}", std::process::id()));
    let config = test_config(dir.to_str().unwrap());
    let (_, artifact) = run_pipeline(&config);
    export::write_artifact(&artifact, &dir).unwrap();
    let terrain = terrain_common::Terrain::load_dir(&dir).unwrap();

    let (world_w, world_h) = artifact.manifest.world_size_m;
    let cell = artifact.manifest.cell_size_m;
    let cols = (world_w / cell).round() as usize;
    let rows = (world_h / cell).round() as usize;

    // Every water cell's biome must actually read back as the water biome
    // (the mask-wins-the-biome invariant `classify.rs` enforces) -- and the
    // config's sea_level_m guarantees at least one such cell exists on this
    // fixture's low east end.
    let mut is_water = vec![false; cols * rows];
    let mut water_cells = 0;
    for gy in 0..rows {
        for gx in 0..cols {
            let (x, y) = (gx as f32 * cell, gy as f32 * cell);
            if terrain.is_water(x, y) {
                water_cells += 1;
                is_water[gy * cols + gx] = true;
                assert_eq!(terrain.biome_at(x, y), classify::BIOME_WATER, "water cell at ({x},{y}) must have the water biome");
                assert!(!terrain.is_walkable(x, y), "water cell at ({x},{y}) must not be walkable");
            }
        }
    }
    assert!(water_cells > 0, "this fixture's sea_level_m should produce at least some water");

    // Coastline contiguity (design doc §8's "river/coastline contiguous ...
    // via flood-fill"): every water cell must be 8-connected-reachable from
    // an edge water cell -- no isolated inland puddle should have survived
    // the water stage's own edge flood-fill this far down the pipeline.
    let mut reached = vec![false; cols * rows];
    let mut queue = std::collections::VecDeque::new();
    for gx in 0..cols {
        for &gy in &[0usize, rows - 1] {
            if is_water[gy * cols + gx] && !reached[gy * cols + gx] {
                reached[gy * cols + gx] = true;
                queue.push_back((gx, gy));
            }
        }
    }
    for gy in 0..rows {
        for &gx in &[0usize, cols - 1] {
            if is_water[gy * cols + gx] && !reached[gy * cols + gx] {
                reached[gy * cols + gx] = true;
                queue.push_back((gx, gy));
            }
        }
    }
    while let Some((gx, gy)) = queue.pop_front() {
        for dy in -1i64..=1 {
            for dx in -1i64..=1 {
                if dx == 0 && dy == 0 {
                    continue;
                }
                let (nx, ny) = (gx as i64 + dx, gy as i64 + dy);
                if nx < 0 || ny < 0 || nx >= cols as i64 || ny >= rows as i64 {
                    continue;
                }
                let (nx, ny) = (nx as usize, ny as usize);
                if is_water[ny * cols + nx] && !reached[ny * cols + nx] {
                    reached[ny * cols + nx] = true;
                    queue.push_back((nx, ny));
                }
            }
        }
    }
    for gy in 0..rows {
        for gx in 0..cols {
            if is_water[gy * cols + gx] {
                assert!(reached[gy * cols + gx], "water cell ({gx},{gy}) is an isolated puddle, not edge-connected");
            }
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn debug_dump_writers_run_without_crashing_at_every_stage_boundary() {
    // "Visual review tooling" (issue #68): confirm every dump writer this
    // pipeline touches regenerates cleanly against real (not hand-built)
    // stage output. A human still eyeballs the PNGs; this just guarantees
    // the dump path itself never panics/errors as stages evolve.
    let dir = std::env::temp_dir().join(format!("terrain-bake-pv-dump-{}", std::process::id()));
    let config = test_config(dir.to_str().unwrap());

    let mut grid = synth::synthesize(&config.source);
    let overrides = OverrideMask::none(grid.width, grid.height);
    let mask = water::run_water_mask_stage(&mut grid, &config.water, &overrides);
    let biome_overrides = BiomeOverrideMask::none(grid.width, grid.height);
    let classification = classify::run_classify_stage(&grid, &mask, &config.classify, &biome_overrides);

    let dump_dir = dir.join("dumps");
    std::fs::create_dir_all(&dump_dir).unwrap();
    dump::write_hillshade_png(&grid, &dump_dir.join("hillshade.png")).unwrap();
    dump::write_water_mask_png(&mask, &dump_dir.join("water_mask.png")).unwrap();
    dump::write_biome_map_png(&classification, &dump_dir.join("biome_map.png")).unwrap();

    for name in ["hillshade.png", "water_mask.png", "biome_map.png"] {
        assert!(dump_dir.join(name).exists());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn real_dem_ingest_flows_through_every_stage_unmodified() {
    // Issue #69's own acceptance criterion, made durable: a real DEM (not
    // the synthetic placeholder) runs through water/stylize/detail/erosion/
    // classify/export with zero code differences from every other test in
    // this file. `testdata/brisbane_hills_demo.grid` is a real 320x320-cell
    // slice of the D'Aguilar Range foothills, pre-converted by
    // `tools/convert_dem.py` from a real Geoscience Australia 5m DTM (see
    // `README.md`).
    let fixture = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/brisbane_hills_demo.grid"));
    let grid = terrain_bake::ingest::load_dem_grid(fixture).expect("committed DEM fixture must load");
    assert_eq!((grid.width, grid.height), (320, 320));

    let dir = std::env::temp_dir().join(format!("terrain-bake-pv-real-dem-{}", std::process::id()));
    let mut config = test_config(dir.to_str().unwrap());
    config.source.working_res_m = grid.cell_size_m;
    config.source.target_res_m = grid.cell_size_m; // no upsampling -- this test is about real data flowing through, not detail synthesis
    config.water.sea_level_m = -10.0; // this AOI's real height range is ~29m..411m, comfortably above

    let (final_grid, artifact) = run_pipeline_from_grid(&config, grid);

    // Real terrain, not the synthetic placeholder's shape: sanity-check the
    // height range is plausible hill country, not some degenerate all-zero
    // or NaN-contaminated output.
    let (mut min, mut max) = (f32::INFINITY, f32::NEG_INFINITY);
    for &h in &final_grid.heights {
        assert!(h.is_finite(), "real-DEM ingest produced a non-finite height");
        min = min.min(h);
        max = max.max(h);
    }
    assert!(min > 0.0 && max < 500.0, "height range {min}..{max} isn't plausible for this AOI");
    assert!(!artifact.height_tiles.is_empty());
    assert!(!artifact.meta_tiles.is_empty());
}
