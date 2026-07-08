//! Offline terrain bake pipeline (terrain pipeline epic, issue tracker #56).
//! Library half of the crate — plain, testable functions; `main.rs` is a
//! thin CLI wrapper around these.
//!
//! Every stage in the design doc's pipeline exists now: ingest (#59
//! synthetic placeholder, #69 real DEM), the water mask (#60), stylization
//! (#61), detail synthesis (#65), erosion (#66), classification (#67), and
//! export (#62). `tests/pipeline_validation.rs` (#68) drives all of them
//! end-to-end.

pub mod cache;
pub mod classify;
pub mod config;
pub mod detail;
pub mod dump;
pub mod erosion;
pub mod export;
pub mod grid;
pub mod hash;
pub mod ingest;
pub mod stylize;
pub mod synth;
pub mod water;
