//! Small helpers shared by the binaries (and the lib itself) — previously
//! copy-pasted into `proxy`, `zone_server`, and `bots` separately.

use rand::Rng;

/// Squared distance between two grid points, in i64 to dodge i32 overflow on
/// world-scale coordinates.
pub fn dist2(ax: i32, ay: i32, bx: i32, by: i32) -> i64 {
    let dx = (ax - bx) as i64;
    let dy = (ay - by) as i64;
    dx * dx + dy * dy
}

/// Current unix time in seconds.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A random 8-direction heading (never standing still), for wandering bots.
pub fn random_heading() -> (i32, i32) {
    let dirs = [
        (1, 0), (-1, 0), (0, 1), (0, -1),
        (1, 1), (1, -1), (-1, 1), (-1, -1),
    ];
    dirs[rand::thread_rng().gen_range(0..dirs.len())]
}
