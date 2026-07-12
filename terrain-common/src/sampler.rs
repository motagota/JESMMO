//! `Terrain`: the loaded, queryable form of a baked artifact. `sample_height`
//! here is *the* answer to "how high is the ground at (x,y)" — the server
//! (movement validation, mob ground-snap) and the bake tool's own validation
//! tests both go through this, so they can never disagree (design doc §6-7).

use std::collections::HashMap;
use std::path::Path;

use crate::delta::SparseHeightDelta;
use crate::manifest::{Manifest, ManifestError};
use crate::tile::{decode_height, nav, HeightTile, MetaTile, TileError};

#[derive(Debug)]
pub enum LoadError {
    Manifest(ManifestError),
    Io(std::io::Error),
    Tile(TileError),
}
impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Manifest(e) => write!(f, "{e}"),
            LoadError::Io(e) => write!(f, "{e}"),
            LoadError::Tile(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for LoadError {}
impl From<ManifestError> for LoadError {
    fn from(e: ManifestError) -> Self { LoadError::Manifest(e) }
}
impl From<std::io::Error> for LoadError {
    fn from(e: std::io::Error) -> Self { LoadError::Io(e) }
}
impl From<TileError> for LoadError {
    fn from(e: TileError) -> Self { LoadError::Tile(e) }
}

#[derive(Debug)]
pub struct Terrain {
    manifest: Manifest,
    height_tiles: HashMap<(i32, i32), HeightTile>,
    meta_tiles: HashMap<(i32, i32), MetaTile>,
}

impl Terrain {
    pub fn new(manifest: Manifest) -> Self {
        Terrain { manifest, height_tiles: HashMap::new(), meta_tiles: HashMap::new() }
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn insert_height_tile(&mut self, tile: HeightTile) {
        self.height_tiles.insert((tile.tile_x, tile.tile_y), tile);
    }

    pub fn insert_meta_tile(&mut self, tile: MetaTile) {
        self.meta_tiles.insert((tile.tile_x, tile.tile_y), tile);
    }

    /// Read-only access to a single loaded height tile by tile-grid
    /// coordinate — the terrain-streaming wire-serving path (issue: terrain
    /// streaming) needs this to answer a client's `terrain.tile_request` by
    /// forwarding the tile's own `encode()` bytes directly, without decoding
    /// and re-encoding. `None` if `(tx, ty)` is outside `manifest().tiles` or
    /// otherwise unloaded.
    pub fn height_tile(&self, tx: i32, ty: i32) -> Option<&HeightTile> {
        self.height_tiles.get(&(tx, ty))
    }

    /// Same, for metadata tiles — symmetric with [`Terrain::height_tile`];
    /// not yet consumed by any client (nav/biome data stays server-only for
    /// now) but free to expose alongside it.
    pub fn meta_tile(&self, tx: i32, ty: i32) -> Option<&MetaTile> {
        self.meta_tiles.get(&(tx, ty))
    }

    /// Which tile owns world point `(x, y)` — the public half of
    /// [`Terrain::locate`], for callers (like a tile-request handler) that
    /// need to map a position to a tile coordinate without duplicating the
    /// edge-clamping logic themselves.
    pub fn tile_at(&self, x: f32, y: f32) -> (i32, i32) {
        let (tx, ty, _, _) = self.locate(x, y);
        (tx, ty)
    }

    /// Load a full baked artifact directory (`manifest.toml` + `tiles/`) —
    /// the shape the export stage (#62) writes and the server (#63) reads.
    pub fn load_dir(dir: &Path) -> Result<Terrain, LoadError> {
        let manifest = Manifest::load(&dir.join("manifest.toml"))?;
        let side = manifest.tile_size as usize + 1;
        let mut terrain = Terrain::new(manifest.clone());
        for ty in 0..manifest.tiles.1 as i32 {
            for tx in 0..manifest.tiles.0 as i32 {
                let h_bytes = std::fs::read(dir.join("tiles").join(format!("h_x{tx}_y{ty}.bin")))?;
                terrain.insert_height_tile(HeightTile::decode(&h_bytes, side)?);
                // Metadata tiles are optional until the classification stage (#67) lands.
                if let Ok(m_bytes) = std::fs::read(dir.join("tiles").join(format!("m_x{tx}_y{ty}.bin"))) {
                    terrain.insert_meta_tile(MetaTile::decode(&m_bytes, manifest.tile_size as usize)?);
                }
            }
        }
        Ok(terrain)
    }

    /// Which tile owns world point `(x, y)`, and the point's position within
    /// that tile in fractional cell units (`[0, tile_size]`).
    ///
    /// The tile index is clamped to the manifest's actual tile grid, so a
    /// point exactly on the world's outer edge (`x == world_size_m.0`, say)
    /// resolves to the last tile's far edge instead of spilling into a
    /// tile-that-doesn't-exist one step past the last valid index — floor
    /// division alone would otherwise treat the world's max edge as the
    /// start of a nonexistent next tile.
    fn locate(&self, x: f32, y: f32) -> (i32, i32, f32, f32) {
        let extent = self.manifest.tile_extent_m();
        let max_tx = self.manifest.tiles.0 as i32 - 1;
        let max_ty = self.manifest.tiles.1 as i32 - 1;
        let tx = ((x / extent).floor() as i32).clamp(0, max_tx.max(0));
        let ty = ((y / extent).floor() as i32).clamp(0, max_ty.max(0));
        let local_x = x - tx as f32 * extent;
        let local_y = y - ty as f32 * extent;
        let gxf = (local_x / self.manifest.cell_size_m).clamp(0.0, self.manifest.tile_size as f32);
        let gyf = (local_y / self.manifest.cell_size_m).clamp(0.0, self.manifest.tile_size as f32);
        (tx, ty, gxf, gyf)
    }

    /// The canonical height query: bilinear interpolation between the 4
    /// corner samples surrounding `(x, y)`, always resolved from a single
    /// tile (see the edge-duplication convention in `tile.rs`). Returns
    /// `0.0` for a point whose tile isn't loaded (mirrors the client's
    /// existing flat-fallback-until-data-arrives convention, `Protocol.gd`'s
    /// `terrain_height`) — a caller that needs to distinguish "no data" from
    /// "genuinely at sea level" should check tile presence itself.
    pub fn sample_height(&self, x: f32, y: f32) -> f32 {
        self.sample_height_with_delta(x, y, None)
    }

    /// [`Terrain::sample_height`] with a hand-authored edit layer composited
    /// in (terrain-editing epic #72): each of the 4 corner samples gets its
    /// [`SparseHeightDelta`] offset added *before* bilinear interpolation, so
    /// an edited slope interpolates exactly like a baked one.
    ///
    /// `delta` must be the delta **for the tile that owns `(x, y)`** — the
    /// caller resolves ownership via [`Terrain::tile_at`] and looks up its
    /// own per-chunk delta store. `None` (or an empty delta) composes to
    /// bit-exactly the base height: absent blocks contribute a literal
    /// `+ 0.0` to each corner.
    pub fn sample_height_with_delta(&self, x: f32, y: f32, delta: Option<&SparseHeightDelta>) -> f32 {
        let (tx, ty, gxf, gyf) = self.locate(x, y);
        let Some(tile) = self.height_tiles.get(&(tx, ty)) else { return 0.0 };
        // Corner index one-past-the-last-cell is the shared/duplicated edge
        // sample (side = tile_size + 1) — clamp so gx0+1 stays in bounds even
        // when `x` lands exactly on the tile's far edge.
        let gx0 = (gxf.floor() as usize).min(tile.side - 2);
        let gy0 = (gyf.floor() as usize).min(tile.side - 2);
        let fx = gxf - gx0 as f32;
        let fy = gyf - gy0 as f32;

        let (min, max) = (self.manifest.height_min_m, self.manifest.height_max_m);
        let corner = |gx: usize, gy: usize| -> f32 {
            let base = decode_height(tile.get(gx, gy), min, max);
            match delta {
                Some(d) => base + d.offset_m(gx, gy),
                None => base,
            }
        };
        let h00 = corner(gx0, gy0);
        let h10 = corner(gx0 + 1, gy0);
        let h01 = corner(gx0, gy0 + 1);
        let h11 = corner(gx0 + 1, gy0 + 1);
        let h0 = h00 + (h10 - h00) * fx;
        let h1 = h01 + (h11 - h01) * fx;
        h0 + (h1 - h0) * fy
    }

    /// Nearest-cell nav-flag bitfield (no interpolation — categorical data).
    /// `0` (no flags set) for a point whose tile has no metadata yet.
    pub fn nav_flags(&self, x: f32, y: f32) -> u8 {
        self.meta_cell(x, y).map(|(t, gx, gy)| t.nav_flags(gx, gy)).unwrap_or(0)
    }

    pub fn is_water(&self, x: f32, y: f32) -> bool {
        self.nav_flags(x, y) & nav::WATER != 0
    }

    pub fn is_walkable(&self, x: f32, y: f32) -> bool {
        self.nav_flags(x, y) & nav::WALKABLE != 0
    }

    pub fn biome_at(&self, x: f32, y: f32) -> u8 {
        self.meta_cell(x, y).map(|(t, gx, gy)| t.biome(gx, gy)).unwrap_or(0)
    }

    fn meta_cell(&self, x: f32, y: f32) -> Option<(&MetaTile, usize, usize)> {
        let (tx, ty, gxf, gyf) = self.locate(x, y);
        let tile = self.meta_tiles.get(&(tx, ty))?;
        let gx = (gxf.floor() as usize).min(self.manifest.tile_size as usize - 1);
        let gy = (gyf.floor() as usize).min(self.manifest.tile_size as usize - 1);
        Some((tile, gx, gy))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::HeightEncoding;
    use crate::tile::encode_height;

    /// A small hand-built two-tile world: `tile_size = 4` (side = 5 corners),
    /// `cell_size_m = 10`, height = world `x` in meters everywhere (constant
    /// in `y`) — chosen so every expected `sample_height` is just `x`,
    /// trivially hand-verifiable, while still exercising bilinear
    /// interpolation (non-corner points) and the cross-tile seam (tile 0's
    /// column `gx=4` at world x=40 must equal tile 1's column `gx=0`, same
    /// world x). This is the golden fixture the acceptance criteria for #58
    /// call for.
    fn golden_fixture() -> Terrain {
        let manifest = Manifest {
            format_version: 1,
            bake_hash: "golden-fixture".to_string(),
            world_size_m: (80.0, 40.0),
            tile_size: 4,
            cell_size_m: 10.0,
            tiles: (2, 1),
            height_encoding: HeightEncoding::U16,
            height_min_m: 0.0,
            height_max_m: 100.0,
            sea_level_m: 0.0,
        };
        let mut terrain = Terrain::new(manifest.clone());
        for tx in 0..2i32 {
            let mut tile = HeightTile::new(tx, 0, 5);
            for gy in 0..5 {
                for gx in 0..5 {
                    let world_x = tx as f32 * 40.0 + gx as f32 * 10.0;
                    tile.set(gx, gy, encode_height(world_x, manifest.height_min_m, manifest.height_max_m));
                }
            }
            terrain.insert_height_tile(tile);
        }
        terrain
    }

    #[test]
    fn golden_samples_match_expected_heights() {
        let terrain = golden_fixture();
        // (x, y, expected_height) — corners, cell-interior points, and a
        // point right on the tile seam, all should read back as `x`.
        let cases: &[(f32, f32, f32)] = &[
            (0.0, 0.0, 0.0),
            (10.0, 0.0, 10.0),
            (5.0, 5.0, 5.0),      // interior of a cell, bilinear, constant in y
            (35.0, 20.0, 35.0),   // last cell of tile 0, mid-cell in y
            (40.0, 0.0, 40.0),    // exactly on the tile 0/1 seam
            (40.0, 40.0, 40.0),   // seam, far corner in y
            (45.0, 15.0, 45.0),   // interior of tile 1's first cell
            (80.0, 40.0, 80.0),   // far corner of the whole fixture
        ];
        for &(x, y, expected) in cases {
            let got = terrain.sample_height(x, y);
            assert!((got - expected).abs() < 0.01, "sample_height({x},{y}) = {got}, expected {expected}");
        }
    }

    #[test]
    fn world_edge_resolves_to_the_last_tile_not_a_nonexistent_next_one() {
        // y=40 is exactly `world_size_m.1` with only one tile row (ty=0
        // valid) — must resolve to that row's far edge, not "tile row 1",
        // which doesn't exist and would silently fall back to 0.0.
        let terrain = golden_fixture();
        assert!((terrain.sample_height(40.0, 40.0) - 40.0).abs() < 0.01);
        assert!((terrain.sample_height(80.0, 40.0) - 80.0).abs() < 0.01);
    }

    #[test]
    fn adjacent_tiles_agree_on_their_shared_edge() {
        // The seam test (design doc §8): tile 0's rightmost corner column
        // must be bit-identical to tile 1's leftmost corner column.
        let terrain = golden_fixture();
        let t0 = &terrain.height_tiles[&(0, 0)];
        let t1 = &terrain.height_tiles[&(1, 0)];
        for gy in 0..t0.side {
            assert_eq!(
                t0.get(t0.side - 1, gy),
                t1.get(0, gy),
                "seam mismatch at row {gy}"
            );
        }
    }

    #[test]
    fn height_tile_accessor_returns_the_loaded_tile_or_none() {
        let terrain = golden_fixture();
        assert!(terrain.height_tile(0, 0).is_some());
        assert!(terrain.height_tile(1, 0).is_some());
        assert!(terrain.height_tile(2, 0).is_none(), "out-of-range tile must be None, not panic");
        assert!(terrain.height_tile(0, 1).is_none());
    }

    #[test]
    fn meta_tile_accessor_mirrors_height_tile_accessor() {
        let terrain = golden_fixture();
        // The golden fixture never inserts meta tiles (classification is a
        // separate stage) -- every coordinate, in-range or not, is None.
        assert!(terrain.meta_tile(0, 0).is_none());
        assert!(terrain.meta_tile(1, 0).is_none());
        assert!(terrain.meta_tile(5, 5).is_none());
    }

    #[test]
    fn tile_at_matches_locate_for_interior_and_edge_points() {
        let terrain = golden_fixture();
        assert_eq!(terrain.tile_at(5.0, 5.0), (0, 0), "interior of tile 0");
        assert_eq!(terrain.tile_at(45.0, 15.0), (1, 0), "interior of tile 1");
        assert_eq!(terrain.tile_at(40.0, 0.0), (1, 0), "exactly on the tile 0/1 seam belongs to tile 1");
        assert_eq!(
            terrain.tile_at(80.0, 40.0),
            (1, 0),
            "world's far edge clamps to the last valid tile, not a nonexistent one past it"
        );
    }

    #[test]
    fn empty_or_absent_delta_composes_to_exactly_the_base_height() {
        let terrain = golden_fixture();
        let empty = SparseHeightDelta::new(5);
        for &(x, y) in &[(0.0f32, 0.0f32), (5.0, 5.0), (35.0, 20.0), (40.0, 40.0), (80.0, 40.0)] {
            let base = terrain.sample_height(x, y);
            // Bit-exact, not epsilon-close: unedited terrain must not drift.
            assert_eq!(terrain.sample_height_with_delta(x, y, None), base);
            assert_eq!(terrain.sample_height_with_delta(x, y, Some(&empty)), base);
        }
    }

    #[test]
    fn delta_offsets_composite_through_bilinear_interpolation() {
        let terrain = golden_fixture();
        // Raise all 4 corners of tile 0's first cell (world x,y in [0,10])
        // by exactly 1m (100cm): every interior point of that cell must read
        // exactly 1m above base, and points outside it must be untouched.
        let mut d = SparseHeightDelta::new(5);
        for (gx, gy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            d.set_offset_cm(gx, gy, 100);
        }
        let raised = terrain.sample_height_with_delta(5.0, 5.0, Some(&d));
        assert!((raised - (5.0 + 1.0)).abs() < 0.01, "cell interior should be base+1m, got {raised}");
        let corner = terrain.sample_height_with_delta(0.0, 0.0, Some(&d));
        assert!((corner - 1.0).abs() < 0.01, "corner should be 0+1m, got {corner}");
        // (15, 5) is in the next cell over: corners (1,0)/(1,1) are raised
        // but (2,0)/(2,1) aren't, so at its midpoint the lift is half.
        let half = terrain.sample_height_with_delta(15.0, 5.0, Some(&d));
        assert!((half - (15.0 + 0.5)).abs() < 0.01, "edited-slope midpoint should be base+0.5m, got {half}");
        // Well clear of the edit: no effect at all.
        let far = terrain.sample_height_with_delta(35.0, 20.0, Some(&d));
        assert_eq!(far, terrain.sample_height(35.0, 20.0));
    }

    #[test]
    fn negative_delta_lowers_terrain() {
        let terrain = golden_fixture();
        let mut d = SparseHeightDelta::new(5);
        for (gx, gy) in [(2, 2), (3, 2), (2, 3), (3, 3)] {
            d.set_offset_cm(gx, gy, -250); // dig 2.5m
        }
        let dug = terrain.sample_height_with_delta(25.0, 25.0, Some(&d));
        assert!((dug - (25.0 - 2.5)).abs() < 0.01, "expected base-2.5m, got {dug}");
    }

    #[test]
    fn load_dir_round_trips_a_written_artifact() {
        let dir = std::env::temp_dir().join(format!("terrain-common-test-{}", std::process::id()));
        let tiles_dir = dir.join("tiles");
        std::fs::create_dir_all(&tiles_dir).unwrap();

        let manifest = Manifest {
            format_version: 1,
            bake_hash: "test".to_string(),
            world_size_m: (40.0, 40.0),
            tile_size: 4,
            cell_size_m: 10.0,
            tiles: (1, 1),
            height_encoding: HeightEncoding::U16,
            height_min_m: 0.0,
            height_max_m: 100.0,
            sea_level_m: 0.0,
        };
        std::fs::write(dir.join("manifest.toml"), toml::to_string(&manifest).unwrap()).unwrap();
        let mut tile = HeightTile::new(0, 0, 5);
        for gy in 0..5 {
            for gx in 0..5 {
                tile.set(gx, gy, encode_height((gx * 10) as f32, 0.0, 100.0));
            }
        }
        std::fs::write(tiles_dir.join("h_x0_y0.bin"), tile.encode(1)).unwrap();

        let terrain = Terrain::load_dir(&dir).unwrap();
        assert!((terrain.sample_height(25.0, 5.0) - 25.0).abs() < 0.01);

        std::fs::remove_dir_all(&dir).ok();
    }
}
