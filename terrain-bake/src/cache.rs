//! Per-stage output caching: each stage's result is cached under
//! `<out_dir>/.cache/<stage>_<hash>.bin`, keyed by a content hash of (its own
//! config section + upstream hash). Re-running with only a late-stage param
//! change skips every earlier stage's recomputation; `--force` bypasses the
//! cache entirely (design doc §4).

use std::path::{Path, PathBuf};

use crate::grid::Grid;

pub fn stage_cache_path(out_dir: &Path, stage: &str, hash: &str) -> PathBuf {
    out_dir.join(".cache").join(format!("{stage}_{hash}.bin"))
}

/// Outcome of [`cached_stage`] — the grid, and whether it came from cache.
pub struct StageResult {
    pub grid: Grid,
    pub cache_hit: bool,
}

/// Run `compute` for `stage`, keyed by `input_hash`. Loads the cached grid
/// instead of recomputing if a matching cache file already exists (unless
/// `force`); always (re)writes the cache file after computing.
pub fn cached_stage(
    out_dir: &Path,
    stage: &str,
    input_hash: &str,
    force: bool,
    compute: impl FnOnce() -> Grid,
) -> StageResult {
    let path = stage_cache_path(out_dir, stage, input_hash);
    if !force {
        if let Ok(bytes) = std::fs::read(&path) {
            if let Some(grid) = Grid::decode(&bytes) {
                return StageResult { grid, cache_hit: true };
            }
        }
    }
    let grid = compute();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, grid.encode());
    StageResult { grid, cache_hit: false }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("terrain-bake-cache-test-{name}-{}", std::process::id()))
    }

    #[test]
    fn second_call_with_same_hash_is_a_cache_hit_and_skips_compute() {
        let dir = temp_dir("hit");
        std::fs::create_dir_all(&dir).unwrap();
        let calls = AtomicUsize::new(0);

        let r1 = cached_stage(&dir, "ingest", "abc", false, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Grid::new(2, 2, 10.0)
        });
        assert!(!r1.cache_hit);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let r2 = cached_stage(&dir, "ingest", "abc", false, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Grid::new(2, 2, 10.0)
        });
        assert!(r2.cache_hit);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "compute must not run again on a cache hit");
        assert_eq!(r1.grid, r2.grid);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn force_bypasses_the_cache() {
        let dir = temp_dir("force");
        std::fs::create_dir_all(&dir).unwrap();
        let calls = AtomicUsize::new(0);

        cached_stage(&dir, "ingest", "xyz", false, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Grid::new(2, 2, 10.0)
        });
        let r2 = cached_stage(&dir, "ingest", "xyz", true, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Grid::new(2, 2, 10.0)
        });
        assert!(!r2.cache_hit);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "--force must recompute even with a cached entry");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn different_hash_is_a_separate_cache_entry() {
        let dir = temp_dir("distinct");
        std::fs::create_dir_all(&dir).unwrap();
        let calls = AtomicUsize::new(0);
        cached_stage(&dir, "ingest", "hash-a", false, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Grid::new(2, 2, 10.0)
        });
        cached_stage(&dir, "ingest", "hash-b", false, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Grid::new(2, 2, 10.0)
        });
        assert_eq!(calls.load(Ordering::SeqCst), 2, "a different input hash must not hit the other's cache entry");

        std::fs::remove_dir_all(&dir).ok();
    }
}
