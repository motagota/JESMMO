//! Offline terrain bake pipeline (terrain pipeline epic, issue tracker #56).
//! Library half of the crate — plain, testable functions; `main.rs` is a
//! thin CLI wrapper around these.
//!
//! Ingest (#59, against a synthetic placeholder rather than a real GeoTIFF
//! DEM — that's #69) and the water mask (#60) exist so far. Every later
//! stage (stylization #61, export #62, detail #65, erosion #66,
//! classification #67) plugs in here the same way.

pub mod cache;
pub mod config;
pub mod dump;
pub mod grid;
pub mod hash;
pub mod synth;
pub mod water;
