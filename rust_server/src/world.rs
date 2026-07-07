//! The authored Capital — Phase 1 world content (issue #4).
//!
//! The capital is **authored data**, not code that runs a simulation. It defines
//! the named districts that tile the world, the road graph, the starter plot grid,
//! the town-centre spawn anchor, and the first build order. Crucially this identity
//! is keyed to *regions of the world*, independent of how many zone processes back
//! them (a busy district may be split across several sims, or several districts may
//! share one) — the gateway maps a point/region to its district by geometry.
//!
//! The capital starts **empty**: this module authors the ground (district rects),
//! the road graph, and the plot grid, but **no buildings**. Structures only appear
//! as players complete build orders and build homes (M2/M3). See phase1.md §3.1-3.2.
//!
//! `WORLD_SIZE` mirrors the gateway/zone constant; keep them in sync.

/// Edge length of the (square) world, in world units (1 unit = 1 meter). Mirrors
/// the same constant in `proxy.rs` / `zone_server.rs`. 6400x6400 = ~41 km²,
/// matching the design's ~40 km² capital footprint (`MMO.md` §7).
pub const WORLD_SIZE: i32 = 6400;

/// A half-open rectangle of the world: `[x0, x1) x [y0, y1)`. (Mirror of the
/// gateway's private `Region`, exposed here as authored geometry.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl Rect {
    pub const fn new(x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        Rect { x0, y0, x1, y1 }
    }
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }
    pub fn centre(&self) -> (i32, i32) {
        ((self.x0 + self.x1) / 2, (self.y0 + self.y1) / 2)
    }
    pub fn area(&self) -> i64 {
        (self.x1 - self.x0) as i64 * (self.y1 - self.y0) as i64
    }
    /// Whether this rect shares any area with `other` (both half-open). Unlike
    /// [`Capital::district_for_region`] (which picks the *one* district a region's
    /// *centre* falls in, for labeling), this is for "does any part of this zone's
    /// region fall in this district at all" — needed when a single zone spans
    /// multiple districts (e.g. the default whole-world zone before any split).
    pub fn overlaps(&self, other: Rect) -> bool {
        self.x0 < other.x1 && other.x0 < self.x1 && self.y0 < other.y1 && other.y0 < self.y1
    }
}

/// Whether a district is a safe hub (no PvP / mob aggression) or open wilds. The
/// whole Phase 1 capital is `Safe`; the flag is authored here, but its *enforcement*
/// (disabling damage) lands in #5. `Wilds` exists only to reserve the concept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Safety {
    Safe,
    Wilds,
}

/// Parameters for a district's authored plot grid. World coordinates of a plot are
/// derived from these so seeding (which stores grid indices) and rendering (which
/// needs world positions) share one source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlotGrid {
    pub cols: i32,
    pub rows: i32,
    pub margin: i32, // inset from the district's top-left origin
    pub plot_w: i32,
    pub plot_h: i32,
    pub gap: i32, // spacing between adjacent plots
    pub tier: i64,
}

/// One authored plot cell: its grid indices (durably stored) plus the world-space
/// top-left it maps to (derived; handy for the client and for spawn framing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlotCell {
    pub grid_x: i32,
    pub grid_y: i32,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub tier: i64,
}

impl PlotCell {
    /// This cell's world-space bounds as a [`Rect`].
    pub fn rect(&self) -> Rect {
        Rect::new(self.x, self.y, self.x + self.w, self.y + self.h)
    }
}

/// A named district: a region of the world with an identity and (optionally) a
/// starter plot grid.
#[derive(Debug, Clone)]
pub struct District {
    pub id: &'static str,
    pub name: &'static str,
    pub region: Rect,
    pub safety: Safety,
    pub plot_grid: Option<PlotGrid>,
}

impl District {
    /// The authored plot cells for this district (empty if it has no grid).
    pub fn plots(&self) -> Vec<PlotCell> {
        let Some(g) = self.plot_grid else { return Vec::new() };
        let mut cells = Vec::with_capacity((g.cols * g.rows) as usize);
        for gy in 0..g.rows {
            for gx in 0..g.cols {
                cells.push(PlotCell {
                    grid_x: gx,
                    grid_y: gy,
                    x: self.region.x0 + g.margin + gx * (g.plot_w + g.gap),
                    y: self.region.y0 + g.margin + gy * (g.plot_h + g.gap),
                    w: g.plot_w,
                    h: g.plot_h,
                    tier: g.tier,
                });
            }
        }
        cells
    }
}

/// A straight authored road segment (the road *graph* is the set of these). Data
/// only — drawn by the client; the server doesn't simulate roads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoadSegment {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

/// A seed build order: the city quests that exist the moment the capital boots.
#[derive(Debug, Clone, Copy)]
pub struct SeedBuildOrder {
    pub district: &'static str,
    pub kind: &'static str,
    pub required_json: &'static str,
    /// The build-order kind that must be `completed` before this one unlocks. `None`
    /// for orders that are open from the start; `Some(kind)` seeds this order `locked`
    /// until `kind` completes (the tech-tree edge).
    pub prereq: Option<&'static str>,
    /// The structure this order spawns on completion, and where it appears (world
    /// coords). City structures are authored here — the completed `build_order` row is
    /// their durable source of truth (no `structure` table row in Phase 1/M2).
    pub structure_kind: &'static str,
    pub structure_x: i32,
    pub structure_y: i32,
    /// Skill gate: a contributor must have levelled `required_skill` to at least
    /// `required_level` before this order accepts their contributions. `None`/0 means
    /// ungated. Distinct from `prereq` (a tech-tree edge): a skill-gated order can be
    /// `open` from boot yet show greyed until the player is skilled enough.
    pub required_skill: Option<&'static str>,
    pub required_level: i64,
}

/// A static item definition (the item registry). Gathered resources, crafted
/// goods, and build-order costs all reference these by `id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item {
    pub id: &'static str,
    pub name: &'static str,
    pub stack_size: i64,
    pub category: &'static str, // "wood" | "stone" | ...
}

/// The Phase 1 item registry. Look up by id with [`item`].
pub fn items() -> Vec<Item> {
    vec![
        Item { id: "wood", name: "Wood", stack_size: 100, category: "wood" },
        Item { id: "stone", name: "Stone", stack_size: 100, category: "stone" },
        Item { id: "plank", name: "Plank", stack_size: 100, category: "crafted" },
        Item { id: "tool_kit", name: "Tool Kit", stack_size: 100, category: "crafted" },
    ]
}

/// Look up an item definition by id.
pub fn item(id: &str) -> Option<Item> {
    items().into_iter().find(|i| i.id == id)
}

/// A crafting recipe: `inputs` (each `item_id -> qty`) consumed to produce
/// `output_qty` of `output_item`. Crafting is instant (no timer) — issue #12's
/// "can craft a basic item" acceptance needs only a couple of these.
#[derive(Debug, Clone, Copy)]
pub struct Recipe {
    pub id: &'static str,
    pub name: &'static str,
    pub inputs: &'static [(&'static str, i64)],
    pub output_item: &'static str,
    pub output_qty: i64,
}

/// The Phase 1 recipe registry. Look up by id with [`recipe`].
pub fn recipes() -> Vec<Recipe> {
    vec![
        Recipe { id: "plank", name: "Plank", inputs: &[("wood", 2)], output_item: "plank", output_qty: 2 },
        Recipe {
            id: "tool_kit", name: "Tool Kit", inputs: &[("wood", 1), ("stone", 1)],
            output_item: "tool_kit", output_qty: 1,
        },
    ]
}

/// Look up a recipe definition by id.
pub fn recipe(id: &str) -> Option<Recipe> {
    recipes().into_iter().find(|r| r.id == id)
}

/// Fixed footprint (world units) for a home structure kind, used both by
/// placement validation (bounds/overlap) and the client's ghost preview. `None`
/// for anything that isn't a placeable home structure (#12).
pub fn structure_footprint(kind: &str) -> Option<(i32, i32)> {
    match kind {
        "bed" => Some((20, 20)),
        "storage" => Some((16, 16)),
        "crafting" => Some((20, 20)),
        _ => None,
    }
}

/// An authored gatherable node: a fixed spawn that yields `item_id` until its
/// `qty` is exhausted, then respawns. Node *runtime* state (current qty, respawn
/// timer) is cache-only in the owning zone; this is just the authored spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceNodeSpawn {
    pub id: &'static str,
    pub district: &'static str,
    pub item_id: &'static str,
    pub x: i32,
    pub y: i32,
    pub qty: i64,
}

/// An authored storage access point — a place a player can stand near to deposit
/// to / withdraw from the safe home stash. For M2 this is a public town storehouse;
/// in M3 (#12) per-plot home `storage` structures become additional storage points
/// using the same protocol and server ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoragePoint {
    pub id: &'static str,
    pub district: &'static str,
    pub x: i32,
    pub y: i32,
}

/// An authored build-order board — a place a player stands near to contribute to the
/// district's city build orders. For M2 there is one at the town centre; more can be
/// authored per district later. Synced to clients as a `build_board` entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildBoard {
    pub id: &'static str,
    pub district: &'static str,
    pub x: i32,
    pub y: i32,
}

/// Grid cells per axis for the authored terrain heightmap, spanning the whole
/// `WORLD_SIZE` square (so each cell is `WORLD_SIZE / TERRAIN_RESOLUTION` units
/// wide). Mirrored by the client's ground mesh — see `docs/protocol.md`'s
/// `terrain.*` section.
pub const TERRAIN_RESOLUTION: i32 = 48;
/// Peak height of the terrain's gentle rolling hills, in the same units as
/// world x/y (purely cosmetic — nothing gameplay-relevant reads this).
pub const TERRAIN_AMPLITUDE: f32 = 4.0;
/// Coarse control-grid resolution the fine grid is upsampled from (divides
/// `TERRAIN_RESOLUTION` evenly: `48 / 8 = 6`) — only these 81 points are
/// independently random, so hills stay broad rather than a jagged sheet.
const TERRAIN_COARSE_RESOLUTION: i32 = 8;
const TERRAIN_SEED: u32 = 1337;

/// A grid of authored terrain heights (purely cosmetic — the server has no
/// other concept of height/elevation; nothing gameplay-relevant reads this).
/// `resolution` cells per axis; `heights` is `(resolution+1)^2` values,
/// row-major/y-major: `heights[gy * (resolution+1) + gx]`.
#[derive(Debug, Clone)]
pub struct Terrain {
    pub resolution: i32,
    pub heights: Vec<f32>,
}

/// A small deterministic integer hash (splitmix-style: multiply/XOR/shift),
/// mapped to `[-1, 1]`. Deliberately not `rand` — that's for non-reproducible
/// runtime randomness (bot wander, gather rolls); this needs the *same*
/// output every boot, and ideally would be trivial to reproduce in another
/// language if ever needed, unlike a full noise library.
fn hash_corner(gx: i32, gy: i32, seed: u32) -> f32 {
    let mut h = (gx as u32)
        .wrapping_mul(374_761_393)
        .wrapping_add((gy as u32).wrapping_mul(668_265_263))
        .wrapping_add(seed.wrapping_mul(2_246_822_519));
    h = (h ^ (h >> 15)).wrapping_mul(2_246_822_519);
    h = (h ^ (h >> 13)).wrapping_mul(3_266_489_917);
    h ^= h >> 16;
    (h as f32 / u32::MAX as f32) * 2.0 - 1.0
}

/// Build the fine `(resolution+1)^2` terrain grid: hash a coarse control grid,
/// then bilinearly upsample it. Only the coarse-generation method needs to
/// live here — once generated, the fine grid is just sent to clients as plain
/// numbers, so nothing about *how* it was made needs to be reproduced
/// client-side, only the final values.
fn generate_terrain(resolution: i32, coarse_resolution: i32, amplitude: f32, seed: u32) -> Terrain {
    let coarse_n = coarse_resolution + 1;
    let mut coarse = vec![0.0f32; (coarse_n * coarse_n) as usize];
    for cy in 0..coarse_n {
        for cx in 0..coarse_n {
            coarse[(cy * coarse_n + cx) as usize] = hash_corner(cx, cy, seed) * amplitude;
        }
    }

    let fine_n = resolution + 1;
    let mut heights = vec![0.0f32; (fine_n * fine_n) as usize];
    for gy in 0..fine_n {
        for gx in 0..fine_n {
            // Map this fine corner into coarse-grid space and bilinearly
            // sample. Plain bilinear (not the triangle-planar split the
            // client uses for the fine grid) is fine here — only the
            // resulting fine-grid values are ever transmitted or compared.
            let cxf = gx as f32 * coarse_resolution as f32 / resolution as f32;
            let cyf = gy as f32 * coarse_resolution as f32 / resolution as f32;
            let cx0 = (cxf.floor() as i32).min(coarse_resolution - 1).max(0);
            let cy0 = (cyf.floor() as i32).min(coarse_resolution - 1).max(0);
            let fx = cxf - cx0 as f32;
            let fy = cyf - cy0 as f32;
            let h00 = coarse[(cy0 * coarse_n + cx0) as usize];
            let h10 = coarse[(cy0 * coarse_n + cx0 + 1) as usize];
            let h01 = coarse[((cy0 + 1) * coarse_n + cx0) as usize];
            let h11 = coarse[((cy0 + 1) * coarse_n + cx0 + 1) as usize];
            let h0 = h00 + (h10 - h00) * fx;
            let h1 = h01 + (h11 - h01) * fx;
            heights[(gy * fine_n + gx) as usize] = h0 + (h1 - h0) * fy;
        }
    }
    Terrain { resolution, heights }
}

/// World-unit margin around a district's plot field over which the terrain
/// blends from the flattened plateau back to its natural rolling-hill
/// height (#55) — plots need level ground to build on (a sloped plot would
/// clip its flat markers/structures into the hill on one side and leave
/// them floating on the other), and a hard step at the plot field's outer
/// edge would look worse than a gentle blend.
const PLOT_FLATTEN_MARGIN: f32 = 100.0;

/// Distance from world point `(px, py)` to the nearest edge of `r` (0.0 if
/// the point is inside it).
fn dist_to_rect(px: f32, py: f32, r: Rect) -> f32 {
    let dx = (r.x0 as f32 - px).max(px - r.x1 as f32).max(0.0);
    let dy = (r.y0 as f32 - py).max(py - r.y1 as f32).max(0.0);
    (dx * dx + dy * dy).sqrt()
}

/// Bilinearly sample the fine terrain grid at an arbitrary world point.
/// Used only to pick a natural-looking flat target height for a plot field
/// (matching the surrounding rolling-hill trend at its centre) — not the
/// client-facing interpolation (that's `Protocol.terrain_height`'s
/// triangle-planar split on the final, already-flattened grid).
fn sample_height_bilinear(heights: &[f32], resolution: i32, world_size: f32, wx: f32, wy: f32) -> f32 {
    let fine_n = resolution + 1;
    let step = world_size / resolution as f32;
    let gxf = (wx / step).clamp(0.0, resolution as f32);
    let gyf = (wy / step).clamp(0.0, resolution as f32);
    let gx0 = (gxf.floor() as i32).min(resolution - 1).max(0);
    let gy0 = (gyf.floor() as i32).min(resolution - 1).max(0);
    let fx = gxf - gx0 as f32;
    let fy = gyf - gy0 as f32;
    let h00 = heights[(gy0 * fine_n + gx0) as usize];
    let h10 = heights[(gy0 * fine_n + gx0 + 1) as usize];
    let h01 = heights[((gy0 + 1) * fine_n + gx0) as usize];
    let h11 = heights[((gy0 + 1) * fine_n + gx0 + 1) as usize];
    let h0 = h00 + (h10 - h00) * fx;
    let h1 = h01 + (h11 - h01) * fx;
    h0 + (h1 - h0) * fy
}

/// Flatten the terrain under every district's starter plot grid, plus a
/// smoothing margin around it (#55), so plots sit on level ground instead of
/// whatever slope the rolling hills happen to leave there. One flat target
/// height per *district* (not per individual plot) — sampled from the
/// natural terrain at the whole plot field's centre — so neighbouring plots
/// share exactly the same plateau with no seams between them; only the
/// field's outer boundary blends back into the natural terrain.
fn flatten_terrain_for_plots(heights: &mut [f32], resolution: i32, world_size: f32, districts: &[District]) {
    struct Field {
        rect: Rect,
        target: f32,
    }

    let base = heights.to_vec(); // pre-flatten, so each field's target reflects the *natural* terrain
    let mut fields = Vec::new();
    for d in districts {
        let cells = d.plots();
        if cells.is_empty() {
            continue;
        }
        let x0 = cells.iter().map(|c| c.x).min().unwrap();
        let y0 = cells.iter().map(|c| c.y).min().unwrap();
        let x1 = cells.iter().map(|c| c.x + c.w).max().unwrap();
        let y1 = cells.iter().map(|c| c.y + c.h).max().unwrap();
        let rect = Rect::new(x0, y0, x1, y1);
        let (cx, cy) = rect.centre();
        let target = sample_height_bilinear(&base, resolution, world_size, cx as f32, cy as f32);
        fields.push(Field { rect, target });
    }
    if fields.is_empty() {
        return;
    }

    let fine_n = resolution + 1;
    let step = world_size / resolution as f32;
    for gy in 0..fine_n {
        for gx in 0..fine_n {
            let wx = gx as f32 * step;
            let wy = gy as f32 * step;
            let mut best_weight = 0.0f32;
            let mut best_target = 0.0f32;
            for field in &fields {
                let d = dist_to_rect(wx, wy, field.rect);
                let t = (1.0 - d / PLOT_FLATTEN_MARGIN).clamp(0.0, 1.0);
                let weight = t * t * (3.0 - 2.0 * t); // smoothstep
                if weight > best_weight {
                    best_weight = weight;
                    best_target = field.target;
                }
            }
            if best_weight > 0.0 {
                let idx = (gy * fine_n + gx) as usize;
                heights[idx] = heights[idx] * (1.0 - best_weight) + best_target * best_weight;
            }
        }
    }
}

/// The whole authored capital.
#[derive(Debug, Clone)]
pub struct Capital {
    pub districts: Vec<District>,
    pub roads: Vec<RoadSegment>,
    pub town_centre: (i32, i32),
    pub build_orders: Vec<SeedBuildOrder>,
    pub resource_nodes: Vec<ResourceNodeSpawn>,
    pub storage_points: Vec<StoragePoint>,
    pub build_boards: Vec<BuildBoard>,
    pub terrain: Terrain,
}

impl Capital {
    /// The district that owns a world point, if any.
    pub fn district_at(&self, x: i32, y: i32) -> Option<&District> {
        self.districts.iter().find(|d| d.region.contains(x, y))
    }

    /// The district that best owns a region — looked up by the region's centre, so
    /// district identity survives the gateway splitting/merging the sim shards.
    pub fn district_for_region(&self, r: Rect) -> Option<&District> {
        let (cx, cy) = r.centre();
        self.district_at(cx, cy)
    }

    /// Every authored starter plot across all districts, in (district_id, cell) form.
    pub fn starter_plots(&self) -> Vec<(&'static str, PlotCell)> {
        self.districts
            .iter()
            .flat_map(|d| d.plots().into_iter().map(move |c| (d.id, c)))
            .collect()
    }

    /// Authored plot cells whose world-space rect falls inside `r` — the set a
    /// zone owning that region should know about, purely as *geometry* (not
    /// ownership — that's the gateway/DB's job), so it can gate "is this point on
    /// some plot" for home-structure placement/crafting (#12).
    pub fn plots_in(&self, r: Rect) -> Vec<(&'static str, PlotCell)> {
        self.starter_plots()
            .into_iter()
            .filter(|(_, c)| r.contains(c.x, c.y) && r.contains(c.x + c.w - 1, c.y + c.h - 1))
            .collect()
    }

    /// Authored resource nodes whose position falls inside `r` — the set a zone
    /// owning that region should spawn and simulate.
    pub fn resource_nodes_in(&self, r: Rect) -> Vec<ResourceNodeSpawn> {
        self.resource_nodes
            .iter()
            .copied()
            .filter(|n| r.contains(n.x, n.y))
            .collect()
    }

    /// Authored storage points whose position falls inside `r`.
    pub fn storage_points_in(&self, r: Rect) -> Vec<StoragePoint> {
        self.storage_points
            .iter()
            .copied()
            .filter(|s| r.contains(s.x, s.y))
            .collect()
    }

    /// Authored build boards whose position falls inside `r` — the set a zone owning
    /// that region should spawn and gate `build.contribute` proximity against.
    pub fn build_boards_in(&self, r: Rect) -> Vec<BuildBoard> {
        self.build_boards
            .iter()
            .copied()
            .filter(|b| r.contains(b.x, b.y))
            .collect()
    }
}

/// The Phase 1 capital: five named districts tiling the 6400x6400 (~41 km²) world
/// in a plus/cross layout — a central Civic Centre with Market/Suburbs bands to its
/// west/east and Craftworks/Old Quarter bands to its north/south — a main avenue and
/// a cross-street connecting every district centre, a starter plot grid in the
/// suburbs, a town-centre spawn at the world centre, and the first build order
/// (the Town Well) on the civic centre board.
pub fn capital() -> Capital {
    // A plus/cross tiling: west/east bands span the full height; the middle column
    // (between them) splits into north/centre/south. Exact tiling, verified in
    // `districts_tile_the_world_without_gaps_or_overlap`.
    let side = WORLD_SIZE / 4; // 1600 — west/east band width, north/south band height
    let mid0 = side; // 1600
    let mid1 = WORLD_SIZE - side; // 4800

    let market = District {
        id: "market",
        name: "Market District",
        region: Rect::new(0, 0, side, WORLD_SIZE),
        safety: Safety::Safe,
        plot_grid: None,
    };
    let suburbs = District {
        id: "suburbs",
        name: "Starter Suburbs",
        region: Rect::new(mid1, 0, WORLD_SIZE, WORLD_SIZE),
        safety: Safety::Safe,
        // A generous starter grid: 12 columns x 20 rows = 240 plots (10x Phase 1's
        // original 24, for a ~28x bigger world — plots stay scarce/premium, not an
        // attempt at the design doc's long-term ~100k-plot figure). plot 80 + gap 40
        // -> 120 per cell; 12 cols span 1400 < 1600, 20 rows span 2360 < 6400.
        plot_grid: Some(PlotGrid {
            cols: 12,
            rows: 20,
            margin: 40,
            plot_w: 80,
            plot_h: 80,
            gap: 40,
            tier: 0,
        }),
    };
    let civic = District {
        id: "civic",
        name: "Civic Centre",
        region: Rect::new(mid0, mid0, mid1, mid1),
        safety: Safety::Safe,
        plot_grid: None,
    };
    let craftworks = District {
        id: "craftworks",
        name: "Craftworks Quarter",
        region: Rect::new(mid0, 0, mid1, side),
        safety: Safety::Safe,
        plot_grid: None,
    };
    let old_quarter = District {
        id: "old_quarter",
        name: "Old Quarter",
        region: Rect::new(mid0, mid1, mid1, WORLD_SIZE),
        safety: Safety::Safe,
        plot_grid: None,
    };

    // Cloned (not moved) so `market`/`civic`/`suburbs` stay available below
    // for roads/build orders, and the originals still move into `Capital`'s
    // own `districts` field at the end unaffected.
    let districts_for_terrain =
        vec![market.clone(), civic.clone(), suburbs.clone(), craftworks.clone(), old_quarter.clone()];
    let mut terrain = generate_terrain(TERRAIN_RESOLUTION, TERRAIN_COARSE_RESOLUTION, TERRAIN_AMPLITUDE, TERRAIN_SEED);
    flatten_terrain_for_plots(&mut terrain.heights, TERRAIN_RESOLUTION, WORLD_SIZE as f32, &districts_for_terrain);

    // Town centre at the world centre, inside the Civic Centre band. This is the
    // spawn anchor and where the first build-order board lives.
    let town_centre = (WORLD_SIZE / 2, WORLD_SIZE / 2);

    // Main avenue (market <-> suburbs, through the town centre's latitude) and a
    // civic cross-street (full height, through the town centre's longitude). Both
    // district centres of every band lie on one of these two lines (craftworks and
    // old_quarter's centres share the town centre's x; market and suburbs' centres
    // share its y), so all five districts read as connected with just these two
    // segments — verified in `capital_has_roads_and_the_build_order_tech_tree`.
    let mid_y = WORLD_SIZE / 2;
    let roads = vec![
        RoadSegment { x0: market.region.centre().0, y0: mid_y, x1: suburbs.region.centre().0, y1: mid_y },
        RoadSegment { x0: town_centre.0, y0: 0, x1: town_centre.0, y1: WORLD_SIZE },
    ];

    // The city tech tree, all in the Civic Centre so one town-centre board demonstrates
    // the whole loop: the Town Well is open from boot; finishing it unlocks the Wall
    // Section, which unlocks the Market Stall. Structures appear near the town centre on
    // completion. Costs are small so the headline demo fills quickly.
    let (tcx, tcy) = town_centre;
    let build_orders = vec![
        SeedBuildOrder {
            district: civic.id,
            kind: "town_well",
            required_json: r#"{"wood":20,"stone":10}"#,
            prereq: None,
            structure_kind: "well",
            structure_x: tcx,
            structure_y: tcy - 40,
            required_skill: None,
            required_level: 0,
        },
        SeedBuildOrder {
            district: civic.id,
            kind: "wall_section",
            required_json: r#"{"stone":30}"#,
            prereq: Some("town_well"),
            structure_kind: "wall",
            structure_x: tcx - 100,
            structure_y: tcy,
            required_skill: None,
            required_level: 0,
        },
        SeedBuildOrder {
            district: civic.id,
            kind: "market_stall",
            required_json: r#"{"wood":40}"#,
            prereq: Some("wall_section"),
            structure_kind: "stall",
            structure_x: tcx + 100,
            structure_y: tcy - 40,
            required_skill: None,
            required_level: 0,
        },
        // Skill-gated tier: open from boot but greyed until the contributor reaches
        // Building 1 — which a solo player earns by completing the Town Well. This is
        // the headline #10 demo: a threshold un-greys a previously locked structure,
        // independent of the well→wall→market prerequisite chain.
        SeedBuildOrder {
            district: civic.id,
            kind: "watchtower",
            required_json: r#"{"wood":30,"stone":20}"#,
            prereq: None,
            structure_kind: "watchtower",
            structure_x: tcx + 60,
            structure_y: tcy + 80,
            required_skill: Some("building"),
            required_level: 1,
        },
    ];

    // Gatherable nodes. A grove of trees ringing the town centre (so a fresh
    // spawn finds wood immediately) plus wood/stone spread through every
    // district's now much larger footprint. Ids are stable so a node keeps its
    // identity across respawns.
    let resource_nodes = vec![
        ResourceNodeSpawn { id: "node_civic_tree_0", district: "civic", item_id: "wood", x: tcx - 60, y: tcy - 60, qty: 5 },
        ResourceNodeSpawn { id: "node_civic_tree_1", district: "civic", item_id: "wood", x: tcx + 60, y: tcy - 60, qty: 5 },
        ResourceNodeSpawn { id: "node_civic_tree_2", district: "civic", item_id: "wood", x: tcx - 60, y: tcy + 60, qty: 5 },
        ResourceNodeSpawn { id: "node_civic_tree_3", district: "civic", item_id: "wood", x: tcx + 60, y: tcy + 60, qty: 5 },
        ResourceNodeSpawn { id: "node_civic_rock_0", district: "civic", item_id: "stone", x: tcx, y: tcy - 110, qty: 5 },
        ResourceNodeSpawn { id: "node_market_tree_0", district: "market", item_id: "wood", x: 400, y: 800, qty: 5 },
        ResourceNodeSpawn { id: "node_market_tree_1", district: "market", item_id: "wood", x: 1000, y: 2400, qty: 5 },
        ResourceNodeSpawn { id: "node_market_rock_0", district: "market", item_id: "stone", x: 600, y: 4000, qty: 5 },
        ResourceNodeSpawn { id: "node_market_tree_2", district: "market", item_id: "wood", x: 1200, y: 5200, qty: 5 },
        ResourceNodeSpawn { id: "node_market_rock_1", district: "market", item_id: "stone", x: 300, y: 6000, qty: 5 },
        ResourceNodeSpawn { id: "node_suburbs_tree_0", district: "suburbs", item_id: "wood", x: 5200, y: 700, qty: 5 },
        ResourceNodeSpawn { id: "node_suburbs_tree_1", district: "suburbs", item_id: "wood", x: 5900, y: 2200, qty: 5 },
        ResourceNodeSpawn { id: "node_suburbs_rock_0", district: "suburbs", item_id: "stone", x: 5400, y: 3600, qty: 5 },
        ResourceNodeSpawn { id: "node_suburbs_tree_2", district: "suburbs", item_id: "wood", x: 6000, y: 5000, qty: 5 },
        ResourceNodeSpawn { id: "node_suburbs_rock_1", district: "suburbs", item_id: "stone", x: 5100, y: 6100, qty: 5 },
        ResourceNodeSpawn { id: "node_craftworks_tree_0", district: "craftworks", item_id: "wood", x: 2000, y: 400, qty: 5 },
        ResourceNodeSpawn { id: "node_craftworks_rock_0", district: "craftworks", item_id: "stone", x: 3200, y: 900, qty: 5 },
        ResourceNodeSpawn { id: "node_craftworks_tree_1", district: "craftworks", item_id: "wood", x: 4400, y: 500, qty: 5 },
        ResourceNodeSpawn { id: "node_craftworks_rock_1", district: "craftworks", item_id: "stone", x: 2800, y: 1300, qty: 5 },
        ResourceNodeSpawn { id: "node_old_quarter_tree_0", district: "old_quarter", item_id: "wood", x: 2000, y: 5200, qty: 5 },
        ResourceNodeSpawn { id: "node_old_quarter_rock_0", district: "old_quarter", item_id: "stone", x: 3200, y: 5900, qty: 5 },
        ResourceNodeSpawn { id: "node_old_quarter_tree_1", district: "old_quarter", item_id: "wood", x: 4400, y: 5400, qty: 5 },
        ResourceNodeSpawn { id: "node_old_quarter_rock_1", district: "old_quarter", item_id: "stone", x: 2800, y: 6100, qty: 5 },
    ];

    // A public town storehouse beside the town centre (the M2 stash). Per-plot
    // home storage (#12) will add more storage points using the same protocol.
    let storage_points = vec![StoragePoint {
        id: "storehouse_town",
        district: "civic",
        x: tcx + 30,
        y: tcy + 10,
    }];

    // The city build-order board, at the town centre (opposite the storehouse) so a
    // fresh spawn can reach it. Contributions are gated on standing near this.
    let build_boards = vec![BuildBoard {
        id: "board_town",
        district: "civic",
        x: tcx - 30,
        y: tcy + 10,
    }];

    Capital {
        districts: vec![market, civic, suburbs, craftworks, old_quarter],
        roads,
        town_centre,
        build_orders,
        resource_nodes,
        storage_points,
        build_boards,
        terrain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn districts_tile_the_world_without_gaps_or_overlap() {
        let c = capital();
        // No two districts overlap.
        for (i, a) in c.districts.iter().enumerate() {
            for b in &c.districts[i + 1..] {
                let overlap_x = a.region.x0 < b.region.x1 && b.region.x0 < a.region.x1;
                let overlap_y = a.region.y0 < b.region.y1 && b.region.y0 < a.region.y1;
                assert!(!(overlap_x && overlap_y), "{} overlaps {}", a.id, b.id);
            }
        }
        // Areas sum to the whole world (full coverage of a clean tiling).
        let total: i64 = c.districts.iter().map(|d| d.region.area()).sum();
        assert_eq!(total, (WORLD_SIZE as i64) * (WORLD_SIZE as i64));
    }

    #[test]
    fn town_centre_is_inside_a_safe_district_and_is_the_spawn() {
        let c = capital();
        let (tx, ty) = c.town_centre;
        let d = c.district_at(tx, ty).expect("town centre lies in a district");
        assert_eq!(d.id, "civic");
        assert_eq!(d.safety, Safety::Safe);
        // The gateway's spawn constant must agree with the authored town centre.
        assert_eq!((tx, ty), (WORLD_SIZE / 2, WORLD_SIZE / 2));
    }

    #[test]
    fn district_lookup_by_point_and_region() {
        let c = capital();
        assert_eq!(c.district_at(10, 10).unwrap().id, "market");
        assert_eq!(c.district_at(3200, 3200).unwrap().id, "civic");
        assert_eq!(c.district_at(5500, 3200).unwrap().id, "suburbs");
        assert_eq!(c.district_at(3200, 800).unwrap().id, "craftworks");
        assert_eq!(c.district_at(3200, 5600).unwrap().id, "old_quarter");
        assert!(c.district_at(WORLD_SIZE, 0).is_none()); // outside (half-open)
        // Region centre routing survives shard geometry.
        let r = Rect::new(5000, 0, 5400, 600);
        assert_eq!(c.district_for_region(r).unwrap().id, "suburbs");
    }

    #[test]
    fn starter_plots_are_authored_and_inside_the_suburbs() {
        let c = capital();
        let suburbs = c.districts.iter().find(|d| d.id == "suburbs").unwrap();
        let cells = suburbs.plots();
        assert_eq!(cells.len(), 240); // 12 x 20
        for cell in &cells {
            assert!(
                suburbs.region.contains(cell.x, cell.y)
                    && suburbs.region.contains(cell.x + cell.w - 1, cell.y + cell.h - 1),
                "plot {:?} escapes the suburbs band",
                cell
            );
        }
        // Grid indices are unique.
        let mut seen = std::collections::HashSet::new();
        for cell in &cells {
            assert!(seen.insert((cell.grid_x, cell.grid_y)), "duplicate grid cell");
        }
        // Only the suburbs carry a grid; the capital total matches.
        assert_eq!(c.starter_plots().len(), 240);
    }

    #[test]
    fn item_registry_and_node_spawns_are_consistent() {
        let c = capital();
        // Every node references a real item and sits inside its district.
        assert!(!c.resource_nodes.is_empty());
        for n in &c.resource_nodes {
            assert!(item(n.item_id).is_some(), "node {} -> unknown item {}", n.id, n.item_id);
            let d = c.districts.iter().find(|d| d.id == n.district).expect("node district exists");
            assert!(d.region.contains(n.x, n.y), "node {} escapes {}", n.id, n.district);
            assert!(n.qty > 0);
        }
        // Node ids are unique.
        let mut seen = std::collections::HashSet::new();
        for n in &c.resource_nodes {
            assert!(seen.insert(n.id), "duplicate node id {}", n.id);
        }
        // A fresh spawn at the town centre finds wood nearby.
        let (tcx, tcy) = c.town_centre;
        let near = c.resource_nodes.iter().any(|n| {
            n.item_id == "wood" && ((n.x - tcx).pow(2) + (n.y - tcy).pow(2)) < 200 * 200
        });
        assert!(near, "no wood near the town centre");
    }

    #[test]
    fn resource_nodes_in_filters_by_region() {
        let c = capital();
        let civic = c.districts.iter().find(|d| d.id == "civic").unwrap().region;
        let in_civic = c.resource_nodes_in(civic);
        assert!(!in_civic.is_empty());
        assert!(in_civic.iter().all(|n| n.district == "civic"));
        // The whole world contains every node.
        assert_eq!(c.resource_nodes_in(Rect::new(0, 0, WORLD_SIZE, WORLD_SIZE)).len(), c.resource_nodes.len());
    }

    #[test]
    fn storage_point_is_in_the_civic_centre_near_spawn() {
        let c = capital();
        assert!(!c.storage_points.is_empty());
        let (tcx, tcy) = c.town_centre;
        for s in &c.storage_points {
            assert_eq!(c.district_at(s.x, s.y).map(|d| d.id), Some(s.district));
            // Near the town centre so a fresh spawn can reach it.
            assert!((s.x - tcx).pow(2) + (s.y - tcy).pow(2) < 100 * 100);
        }
        let civic = c.districts.iter().find(|d| d.id == "civic").unwrap().region;
        assert_eq!(c.storage_points_in(civic).len(), c.storage_points.len());
    }

    #[test]
    fn capital_has_roads_and_the_build_order_tech_tree() {
        let c = capital();
        assert!(!c.roads.is_empty(), "the capital should have an authored road graph");
        // The Town Well is open from boot; the rest of the chain is gated behind it.
        let well = c.build_orders.iter().find(|o| o.kind == "town_well").expect("town_well");
        assert_eq!(well.district, "civic");
        assert_eq!(well.prereq, None, "the first order must be open from boot");
        // required_json is valid JSON.
        let v: serde_json::Value = serde_json::from_str(well.required_json).unwrap();
        assert!(v.get("wood").is_some());
        // Every non-root order names a prereq that is itself an authored order, and every
        // order authors a structure spec. This keeps the unlock graph well-formed.
        for o in &c.build_orders {
            let _: serde_json::Value = serde_json::from_str(o.required_json).unwrap();
            assert!(!o.structure_kind.is_empty(), "{} has no structure", o.kind);
            if let Some(p) = o.prereq {
                assert!(c.build_orders.iter().any(|b| b.kind == p),
                    "{} depends on unknown order {}", o.kind, p);
            }
        }
        // town_well unlocks wall_section unlocks market_stall.
        assert!(c.build_orders.iter().any(|o| o.kind == "wall_section" && o.prereq == Some("town_well")));
        assert!(c.build_orders.iter().any(|o| o.kind == "market_stall" && o.prereq == Some("wall_section")));

        // A skill gate is `Some(skill)` iff its level is positive, and a positive gate is
        // reachable: the required level never exceeds what completing the Town Well grants
        // solo (so the headline demo — a threshold un-greying a structure — is achievable).
        let well_units: i64 = serde_json::from_str::<serde_json::Value>(well.required_json)
            .unwrap().as_object().unwrap().values().map(|v| v.as_i64().unwrap()).sum();
        let well_level = crate::persistence::level_for_xp(well_units * crate::persistence::BUILD_XP_PER_UNIT);
        for o in &c.build_orders {
            assert_eq!(o.required_skill.is_some(), o.required_level > 0,
                "{}: skill gate and level must agree", o.kind);
            assert!(o.required_level <= well_level,
                "{} gates at Building {} but the Town Well only grants Building {}",
                o.kind, o.required_level, well_level);
        }
        // The authored gated demo order exists: open from boot yet greyed until Building 1.
        let tower = c.build_orders.iter().find(|o| o.kind == "watchtower").expect("watchtower");
        assert_eq!(tower.prereq, None, "the gated demo is open from boot, gated only by skill");
        assert_eq!((tower.required_skill, tower.required_level), (Some("building"), 1));
    }

    #[test]
    fn build_board_is_in_the_civic_centre_near_spawn() {
        let c = capital();
        assert!(!c.build_boards.is_empty());
        let (tcx, tcy) = c.town_centre;
        for b in &c.build_boards {
            assert_eq!(c.district_at(b.x, b.y).map(|d| d.id), Some(b.district));
            assert!((b.x - tcx).pow(2) + (b.y - tcy).pow(2) < 100 * 100,
                "board should be reachable from a fresh spawn");
        }
        let civic = c.districts.iter().find(|d| d.id == "civic").unwrap().region;
        assert_eq!(c.build_boards_in(civic).len(), c.build_boards.len());
    }

    #[test]
    fn rect_overlaps_detects_any_shared_area() {
        let suburbs = Rect::new(800, 0, 1200, 1200);
        // A single whole-world zone overlaps every district, even though its
        // *centre* (600,600) falls only in civic — this is the case `overlaps`
        // exists for (#13).
        let whole_world = Rect::new(0, 0, WORLD_SIZE, WORLD_SIZE);
        assert!(whole_world.overlaps(suburbs));
        // Two districts that only share a boundary edge (half-open) don't overlap.
        let civic = Rect::new(400, 0, 800, 1200);
        assert!(!civic.overlaps(suburbs));
        // A region entirely inside the suburbs overlaps it.
        assert!(Rect::new(900, 100, 1000, 200).overlaps(suburbs));
        // A region entirely elsewhere does not.
        assert!(!Rect::new(0, 0, 100, 100).overlaps(suburbs));
    }

    #[test]
    fn recipe_registry_is_well_formed() {
        let all = recipes();
        assert!(!all.is_empty());
        for r in &all {
            assert!(!r.inputs.is_empty(), "{} has no inputs", r.id);
            for (input_item, qty) in r.inputs {
                assert!(item(input_item).is_some(), "{} needs unknown item {}", r.id, input_item);
                assert!(*qty > 0);
            }
            assert!(item(r.output_item).is_some(), "{} produces unknown item {}", r.id, r.output_item);
            assert!(r.output_qty > 0);
        }
        // Ids are unique and the lookup helper agrees with the list.
        let mut seen = std::collections::HashSet::new();
        for r in &all {
            assert!(seen.insert(r.id), "duplicate recipe id {}", r.id);
            assert_eq!(recipe(r.id).map(|found| found.id), Some(r.id));
        }
        assert!(recipe("nonexistent").is_none());
    }

    #[test]
    fn structure_footprints_cover_every_placeable_home_structure() {
        for kind in ["bed", "storage", "crafting"] {
            let (w, h) = structure_footprint(kind).expect(kind);
            assert!(w > 0 && h > 0, "{kind} has a degenerate footprint");
        }
        assert!(structure_footprint("wall").is_none(), "city structures aren't placeable homes");
    }

    #[test]
    fn terrain_is_the_right_shape_deterministic_and_in_bounds() {
        let t1 = capital().terrain;
        let expected_len = ((TERRAIN_RESOLUTION + 1) * (TERRAIN_RESOLUTION + 1)) as usize;
        assert_eq!(t1.heights.len(), expected_len, "(resolution+1)^2 grid corners");
        assert_eq!(t1.resolution, TERRAIN_RESOLUTION);

        // Deterministic: authoring the capital again produces byte-identical
        // terrain (same seed every boot) — every connected client, and the
        // server itself, must agree on the same surface.
        let t2 = capital().terrain;
        assert_eq!(t1.heights, t2.heights, "terrain must be identical across calls");

        // Every height stays within the authored amplitude.
        for (i, h) in t1.heights.iter().enumerate() {
            assert!(
                h.abs() <= TERRAIN_AMPLITUDE + 0.001,
                "corner {i} height {h} exceeds the +/-{TERRAIN_AMPLITUDE} amplitude"
            );
        }
    }

    #[test]
    fn terrain_is_smooth_not_a_jagged_sheet() {
        // Adjacent fine-grid corners should differ by a modest fraction of the
        // amplitude -- confirms the coarse-grid upsampling actually smoothed
        // things, rather than every corner being independently random (which
        // would let neighbours swing from -amplitude to +amplitude). Tests
        // `generate_terrain` directly (not `capital().terrain`) since that's
        // specifically what this property is about; the deliberately flat
        // plateaus `flatten_terrain_for_plots` carves into `capital().terrain`
        // are a separate, intentional feature covered by their own test below.
        let t = generate_terrain(TERRAIN_RESOLUTION, TERRAIN_COARSE_RESOLUTION, TERRAIN_AMPLITUDE, TERRAIN_SEED);
        let n = (t.resolution + 1) as usize;
        let max_step = TERRAIN_AMPLITUDE * 0.5; // generous, well under the 2*amplitude worst case
        for gy in 0..n {
            for gx in 0..n - 1 {
                let a = t.heights[gy * n + gx];
                let b = t.heights[gy * n + gx + 1];
                assert!(
                    (a - b).abs() <= max_step,
                    "horizontal neighbours at ({gx},{gy}) differ by {} > {max_step}",
                    (a - b).abs()
                );
            }
        }
        for gy in 0..n - 1 {
            for gx in 0..n {
                let a = t.heights[gy * n + gx];
                let b = t.heights[(gy + 1) * n + gx];
                assert!(
                    (a - b).abs() <= max_step,
                    "vertical neighbours at ({gx},{gy}) differ by {} > {max_step}",
                    (a - b).abs()
                );
            }
        }
    }

    #[test]
    fn plot_field_is_flattened_so_plots_sit_on_level_ground() {
        // #55: the suburbs starter plot field (240 plots) should be a level
        // plateau in `capital().terrain` -- entities/markers are placed via
        // `Protocol.w2v`, which follows this same heightmap, so a sloped plot
        // field would clip structures on one side and float them on the other.
        let c = capital();
        let suburbs = c.districts.iter().find(|d| d.id == "suburbs").unwrap();
        let cells = suburbs.plots();
        let x0 = cells.iter().map(|cell| cell.x).min().unwrap();
        let y0 = cells.iter().map(|cell| cell.y).min().unwrap();
        let x1 = cells.iter().map(|cell| cell.x + cell.w).max().unwrap();
        let y1 = cells.iter().map(|cell| cell.y + cell.h).max().unwrap();

        let t = &c.terrain;
        let n = t.resolution + 1;
        let step = WORLD_SIZE as f32 / t.resolution as f32;

        // Every fine-grid corner well inside the plot field's bounding box
        // (a full margin in from every edge, so we're only sampling corners
        // guaranteed to be at full flatten weight) must be *exactly* level.
        let mut interior_heights = Vec::new();
        for gy in 0..n {
            for gx in 0..n {
                let wx = gx as f32 * step;
                let wy = gy as f32 * step;
                if wx >= x0 as f32 + PLOT_FLATTEN_MARGIN
                    && wx <= x1 as f32 - PLOT_FLATTEN_MARGIN
                    && wy >= y0 as f32 + PLOT_FLATTEN_MARGIN
                    && wy <= y1 as f32 - PLOT_FLATTEN_MARGIN
                {
                    interior_heights.push(t.heights[(gy * n + gx) as usize]);
                }
            }
        }
        assert!(
            !interior_heights.is_empty(),
            "expected at least one fine-grid corner well inside the plot field"
        );
        let first = interior_heights[0];
        for h in &interior_heights {
            assert!(
                (h - first).abs() < 0.001,
                "plot field interior isn't flat: {first} vs {h}"
            );
        }
    }

    #[test]
    fn plots_in_filters_by_region_like_the_other_authored_fixtures() {
        let c = capital();
        let suburbs = c.districts.iter().find(|d| d.id == "suburbs").unwrap().region;
        let in_suburbs = c.plots_in(suburbs);
        assert_eq!(in_suburbs.len(), 240, "every starter plot sits in the suburbs");
        // A civic-only region (no plot grid there) has none.
        let civic = c.districts.iter().find(|d| d.id == "civic").unwrap().region;
        assert!(c.plots_in(civic).is_empty());
        // Each returned cell's rect() really is inside the queried region.
        for (_, cell) in &in_suburbs {
            let r = cell.rect();
            assert!(suburbs.contains(r.x0, r.y0) && suburbs.contains(r.x1 - 1, r.y1 - 1));
        }
    }
}
