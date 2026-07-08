//! Offline terrain bake CLI (terrain pipeline epic, issue tracker #56).
//!
//! `--stage ingest`/`water`/`stylize`/`detail`/`erode`/`classify`/`export`
//! do something; `all` runs every stage. Ingest uses a real DEM when
//! `[source].dem_path` is set, the synthetic placeholder otherwise (#69) —
//! see `terrain_bake::ingest`.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

use terrain_bake::{
    cache,
    classify::{self, BiomeOverrideMask, Classification},
    config::Config,
    detail, dump, erosion, export,
    grid::Grid,
    ingest,
    stylize::{self, FootprintMask},
    water::{self, OverrideMask, WaterMask},
};

#[derive(Parser)]
#[command(name = "terrain-bake", about = "Offline terrain bake pipeline")]
struct Cli {
    #[arg(long)]
    config: PathBuf,
    #[arg(long, value_enum, default_value_t = Stage::All)]
    stage: Stage,
    #[arg(long)]
    debug_dump: Option<PathBuf>,
    /// Bypass the stage cache and recompute even if a matching entry exists.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Copy, Clone, ValueEnum)]
enum Stage {
    All,
    Ingest,
    Water,
    Stylize,
    Detail,
    Erode,
    Classify,
    Export,
}

fn main() {
    let cli = Cli::parse();
    let config = match Config::load(&cli.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[terrain-bake] failed to load {}: {e}", cli.config.display());
            std::process::exit(1);
        }
    };
    let debug_dump = cli.debug_dump.as_deref();

    match cli.stage {
        Stage::Ingest => {
            run_ingest(&config, cli.force, debug_dump);
        }
        Stage::Water => {
            let grid = run_ingest(&config, cli.force, debug_dump);
            run_water(&config, grid, debug_dump);
        }
        Stage::Stylize => {
            let grid = run_ingest(&config, cli.force, debug_dump);
            let (grid, _mask) = run_water(&config, grid, debug_dump);
            run_stylize(&config, grid, debug_dump);
        }
        Stage::Detail => {
            let grid = run_ingest(&config, cli.force, debug_dump);
            let (grid, mask) = run_water(&config, grid, debug_dump);
            let (grid, footprint) = run_stylize(&config, grid, debug_dump);
            let stylized_mask = stylize::compress_mask_horizontal(&mask, config.stylize.horizontal_scale);
            run_detail(&config, grid, stylized_mask, footprint, debug_dump);
        }
        Stage::Erode => {
            let grid = run_ingest(&config, cli.force, debug_dump);
            let (grid, mask) = run_water(&config, grid, debug_dump);
            let (grid, footprint) = run_stylize(&config, grid, debug_dump);
            let stylized_mask = stylize::compress_mask_horizontal(&mask, config.stylize.horizontal_scale);
            let (detailed, _mask) = run_detail(&config, grid, stylized_mask, footprint, debug_dump);
            run_erode(&config, detailed, debug_dump);
        }
        Stage::Classify => {
            let grid = run_ingest(&config, cli.force, debug_dump);
            let (grid, mask) = run_water(&config, grid, debug_dump);
            let (grid, footprint) = run_stylize(&config, grid, debug_dump);
            let stylized_mask = stylize::compress_mask_horizontal(&mask, config.stylize.horizontal_scale);
            let (detailed, detailed_mask) = run_detail(&config, grid, stylized_mask, footprint, debug_dump);
            let eroded = run_erode(&config, detailed, debug_dump);
            run_classify(&config, &eroded, detailed_mask, debug_dump);
        }
        Stage::All | Stage::Export => {
            let grid = run_ingest(&config, cli.force, debug_dump);
            let (grid, mask) = run_water(&config, grid, debug_dump);
            let (grid, footprint) = run_stylize(&config, grid, debug_dump);
            let stylized_mask = stylize::compress_mask_horizontal(&mask, config.stylize.horizontal_scale);
            let (detailed, detailed_mask) = run_detail(&config, grid, stylized_mask, footprint, debug_dump);
            let eroded = run_erode(&config, detailed, debug_dump);
            let classification = run_classify(&config, &eroded, detailed_mask, debug_dump);
            run_export(&config, eroded, classification);
        }
    }
}

fn run_ingest(config: &Config, force: bool, debug_dump: Option<&std::path::Path>) -> Grid {
    let hash = config.source.content_hash();
    let out_dir = PathBuf::from(&config.export.out_dir);
    let result = cache::cached_stage(&out_dir, "ingest", &hash, force, || match ingest::run_ingest(&config.source) {
        Ok(grid) => grid,
        Err(e) => {
            eprintln!("[terrain-bake] ingest failed: {e}");
            std::process::exit(1);
        }
    });

    println!(
        "[terrain-bake] ingest: {} ({}x{} cells at {}m, hash {})",
        if result.cache_hit { "cache hit" } else { "computed" },
        result.grid.width,
        result.grid.height,
        result.grid.cell_size_m,
        &hash[..12],
    );

    if let Some(dir) = debug_dump {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("[terrain-bake] failed to create {}: {e}", dir.display());
            std::process::exit(1);
        }
        let path = dir.join("ingest_hillshade.png");
        match dump::write_hillshade_png(&result.grid, &path) {
            Ok(()) => println!("[terrain-bake] wrote {}", path.display()),
            Err(e) => {
                eprintln!("[terrain-bake] failed to write {}: {e}", path.display());
                std::process::exit(1);
            }
        }
    }
    result.grid
}

fn run_water(config: &Config, mut grid: Grid, debug_dump: Option<&std::path::Path>) -> (Grid, WaterMask) {
    let overrides = match &config.water.override_mask {
        Some(path) => match OverrideMask::from_luma_png(std::path::Path::new(path)) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[terrain-bake] failed to load override mask {path}: {e}");
                std::process::exit(1);
            }
        },
        None => OverrideMask::none(grid.width, grid.height),
    };
    let mask = water::run_water_mask_stage(&mut grid, &config.water, &overrides);
    let water_cells = mask.cells.iter().filter(|&&w| w).count();
    println!(
        "[terrain-bake] water: {water_cells}/{} cells are water (sea level {}m)",
        mask.width * mask.height,
        config.water.sea_level_m,
    );

    if let Some(dir) = debug_dump {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("[terrain-bake] failed to create {}: {e}", dir.display());
            std::process::exit(1);
        }
        let path = dir.join("water_mask.png");
        match dump::write_water_mask_png(&mask, &path) {
            Ok(()) => println!("[terrain-bake] wrote {}", path.display()),
            Err(e) => {
                eprintln!("[terrain-bake] failed to write {}: {e}", path.display());
                std::process::exit(1);
            }
        }
    }
    (grid, mask)
}

fn run_stylize(config: &Config, grid: Grid, debug_dump: Option<&std::path::Path>) -> (Grid, FootprintMask) {
    let footprint = match &config.stylize.capital_flatten_mask {
        Some(path) => match FootprintMask::from_luma_png(std::path::Path::new(path)) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[terrain-bake] failed to load capital flatten mask {path}: {e}");
                std::process::exit(1);
            }
        },
        None => FootprintMask::none(grid.width, grid.height),
    };
    let out = stylize::run_stylize_stage(&grid, &config.stylize, &footprint);
    println!(
        "[terrain-bake] stylize: {}x{} cells (horizontal_scale {}, vertical_curve_exp {}, vertical_scale {})",
        out.width, out.height, config.stylize.horizontal_scale, config.stylize.vertical_curve_exp, config.stylize.vertical_scale,
    );

    if let Some(dir) = debug_dump {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("[terrain-bake] failed to create {}: {e}", dir.display());
            std::process::exit(1);
        }
        let path = dir.join("stylize_hillshade.png");
        match dump::write_hillshade_png(&out, &path) {
            Ok(()) => println!("[terrain-bake] wrote {}", path.display()),
            Err(e) => {
                eprintln!("[terrain-bake] failed to write {}: {e}", path.display());
                std::process::exit(1);
            }
        }
    }
    // The footprint mask is already at the stylized grid's resolution (built
    // from a PNG hand-painted for that resolution) — no resampling needed,
    // unlike the water mask (computed *before* stylization's compression).
    (out, footprint)
}

fn run_detail(
    config: &Config,
    grid: Grid,
    mask: WaterMask,
    footprint: FootprintMask,
    debug_dump: Option<&std::path::Path>,
) -> (Grid, WaterMask) {
    let out = detail::run_detail_stage(&grid, &mask, &footprint, &config.detail, config.source.target_res_m);
    println!(
        "[terrain-bake] detail: {}x{} cells at {}m ({} octaves, base_amp {}m)",
        out.width, out.height, out.cell_size_m, config.detail.octaves, config.detail.base_amp_m,
    );

    if let Some(dir) = debug_dump {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("[terrain-bake] failed to create {}: {e}", dir.display());
            std::process::exit(1);
        }
        let path = dir.join("detail_hillshade.png");
        match dump::write_hillshade_png(&out, &path) {
            Ok(()) => println!("[terrain-bake] wrote {}", path.display()),
            Err(e) => {
                eprintln!("[terrain-bake] failed to write {}: {e}", path.display());
                std::process::exit(1);
            }
        }
    }
    // The mask must follow the grid to whatever resolution detail synthesis
    // upsampled to, same reasoning as stylize's own mask resampling.
    let scale = grid.cell_size_m / config.source.target_res_m;
    let out_mask = stylize::compress_mask_horizontal(&mask, scale);
    (out, out_mask)
}

fn run_erode(config: &Config, grid: Grid, debug_dump: Option<&std::path::Path>) -> Grid {
    let out = erosion::run_erosion_stage(&grid, &config.erosion);
    println!(
        "[terrain-bake] erode: {}x{} cells (enabled {}, max_natural_slope {}deg, thermal_iters {}, hydraulic_iters {})",
        out.width,
        out.height,
        config.erosion.enabled,
        config.erosion.max_natural_slope,
        config.erosion.thermal_iters,
        config.erosion.hydraulic_iters,
    );

    if let Some(dir) = debug_dump {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("[terrain-bake] failed to create {}: {e}", dir.display());
            std::process::exit(1);
        }
        let path = dir.join("erode_hillshade.png");
        match dump::write_hillshade_png(&out, &path) {
            Ok(()) => println!("[terrain-bake] wrote {}", path.display()),
            Err(e) => {
                eprintln!("[terrain-bake] failed to write {}: {e}", path.display());
                std::process::exit(1);
            }
        }
    }
    out
}

fn run_classify(config: &Config, grid: &Grid, mask: WaterMask, debug_dump: Option<&std::path::Path>) -> Classification {
    let overrides = match &config.classify.override_map {
        Some(path) => match BiomeOverrideMask::from_luma_png(std::path::Path::new(path)) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[terrain-bake] failed to load biome override map {path}: {e}");
                std::process::exit(1);
            }
        },
        None => BiomeOverrideMask::none(grid.width, grid.height),
    };
    let classification = classify::run_classify_stage(grid, &mask, &config.classify, &overrides);
    println!(
        "[terrain-bake] classify: {}x{} cells (walkable_max_slope {}deg, {} rules)",
        classification.width,
        classification.height,
        config.classify.walkable_max_slope,
        config.classify.rules.len(),
    );

    if let Some(dir) = debug_dump {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("[terrain-bake] failed to create {}: {e}", dir.display());
            std::process::exit(1);
        }
        let path = dir.join("biome_map.png");
        match dump::write_biome_map_png(&classification, &path) {
            Ok(()) => println!("[terrain-bake] wrote {}", path.display()),
            Err(e) => {
                eprintln!("[terrain-bake] failed to write {}: {e}", path.display());
                std::process::exit(1);
            }
        }
    }
    classification
}

fn run_export(config: &Config, grid: Grid, classification: Classification) {
    let artifact = export::export_artifact(&grid, &classification, config);
    let out_dir = PathBuf::from(&config.export.out_dir);
    if let Err(e) = export::write_artifact(&artifact, &out_dir) {
        eprintln!("[terrain-bake] failed to write artifact to {}: {e}", out_dir.display());
        std::process::exit(1);
    }
    println!(
        "[terrain-bake] export: wrote {} tiles ({}x{} tile grid) to {}, bake_hash {}",
        artifact.height_tiles.len(),
        artifact.manifest.tiles.0,
        artifact.manifest.tiles.1,
        out_dir.display(),
        &artifact.manifest.bake_hash[..12],
    );
}
