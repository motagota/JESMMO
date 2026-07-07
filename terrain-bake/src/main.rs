//! Offline terrain bake CLI (terrain pipeline epic, issue tracker #56).
//!
//! `--stage ingest` and `--stage water` do something (#59, #60); `all` runs
//! every stage that exists so far. `stylize`/`detail`/`erode`/`classify`/
//! `export` are reserved for #61/#65/#66/#67/#62.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

use terrain_bake::{
    cache,
    config::Config,
    dump,
    grid::Grid,
    synth,
    water::{self, OverrideMask},
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
        Stage::All | Stage::Water => {
            let grid = run_ingest(&config, cli.force, debug_dump);
            run_water(&config, grid, debug_dump);
        }
        other => {
            eprintln!("[terrain-bake] --stage {other:?} isn't implemented yet — see the terrain pipeline epic (#56)");
            std::process::exit(1);
        }
    }
}

fn run_ingest(config: &Config, force: bool, debug_dump: Option<&std::path::Path>) -> Grid {
    let hash = config.source.content_hash();
    let out_dir = PathBuf::from(&config.export.out_dir);
    let result = cache::cached_stage(&out_dir, "ingest", &hash, force, || synth::synthesize(&config.source));

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

fn run_water(config: &Config, mut grid: Grid, debug_dump: Option<&std::path::Path>) {
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
}
