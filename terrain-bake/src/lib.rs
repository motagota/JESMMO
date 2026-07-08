//! Offline terrain bake pipeline (terrain pipeline epic, issue tracker #56).
//! Library half of the crate — plain, testable functions; `main.rs` is a
//! thin CLI wrapper around these.
//!
//! Ingest (#59, against a synthetic placeholder rather than a real GeoTIFF
//! DEM — that's #69), the water mask (#60), stylization (#61), export
//! (#62), and detail synthesis (#65) exist so far. Erosion (#66) and
//! classification (#67) plug in here the same way.

pub mod cache;
pub mod config;
pub mod detail;
pub mod dump;
pub mod export;
pub mod grid;
pub mod hash;
pub mod stylize;
pub mod synth;
pub mod water;
