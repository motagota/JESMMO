//! Offline terrain bake pipeline (terrain pipeline epic, issue tracker #56).
//! Library half of the crate — plain, testable functions; `main.rs` is a
//! thin CLI wrapper around these.
//!
//! Only the ingest stage exists so far (#59), running against a synthetic
//! placeholder height source (#59) rather than a real GeoTIFF DEM (#69).
//! Every later stage (water mask #60, stylization #61, export #62, detail
//! #65, erosion #66, classification #67) plugs in here the same way.

pub mod cache;
pub mod config;
pub mod dump;
pub mod grid;
pub mod hash;
pub mod synth;
