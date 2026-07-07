//! Offline terrain bake CLI (terrain pipeline epic, issue tracker #56).
//!
//! Only `--stage ingest` (and `all`, currently equivalent since no other
//! stage exists yet) does anything (#59); `stylize`/`detail`/`erode`/
//! `classify`/`export` are reserved for #60-#67.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

use terrain_bake::{cache, config::Config, dump, synth};

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

    match cli.stage {
        Stage::All | Stage::Ingest => run_ingest(&config, cli.force, cli.debug_dump.as_deref()),
        other => {
            eprintln!("[terrain-bake] --stage {other:?} isn't implemented yet — see the terrain pipeline epic (#56)");
            std::process::exit(1);
        }
    }
}

fn run_ingest(config: &Config, force: bool, debug_dump: Option<&std::path::Path>) {
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
}
