//! Shared terrain tile format + canonical height sampler (terrain pipeline
//! epic, issue tracker #56). Consumed by both `rust_server` (authoritative
//! heights, movement validation) and the offline `terrain-bake` tool (writes
//! the tiles this crate reads) — the whole point is one height-at-(x,y)
//! answer, not two independently-implemented ones.
//!
//! Currently a scaffold (#57): the tile binary format, manifest parsing, and
//! `sample_height` land in #58.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_builds() {
        // Placeholder until #58 lands the real tile format + sampler tests.
    }
}
