//! The authored Capital — Phase 1 world content (issue #4).
//!
//! The capital is **authored data**, not code that runs a simulation. It defines
//! the named districts that tile the world, the starter plot grid, and the
//! town-centre spawn anchor. Crucially this identity is keyed to *regions of the
//! world*, independent of how many zone processes back them (a busy district may be
//! split across several sims, or several districts may share one) — the gateway
//! maps a point/region to its district by geometry.
//!
//! The capital starts **empty**: this module authors the ground (district rects)
//! and the plot grid, but **no buildings and no roads**. Structures — including
//! roads — only appear as players complete build orders (commissioned at runtime
//! by the mayor, see `mayor.build_create`) and build homes (M2/M3). See
//! phase1.md §3.1-3.2.
//!
//! `WORLD_SIZE` lives here; the gateway/zone binaries import it.

/// Edge length of the (square) world, in world units (1 unit = 1 meter).
/// 25600x25600 = ~655 km²: the near-full extent of the real Brisbane DEM
/// (the v3 bake, see the repo-root `terrain.toml`) — exactly 4x the linear
/// size of the original 6400 world, so all authored coordinates scaled by 4.
pub const WORLD_SIZE: i32 = 25600;

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

/// Grid resolution the server samples the loaded terrain artifact at to
/// build the `terrain.data` wire message — decoupled from the baked
/// artifact's own internal tile/cell resolution (see [`loaded_terrain`]'s
/// doc comment for why that decoupling is the whole point). Mirrored by the
/// client's ground mesh — see `docs/protocol.md`'s `terrain.*` section.
///
/// 384 keeps the backdrop at ~66m per cell on the 25600 world — twice the
/// per-cell fidelity the original 6400 world had at 48 — so distant terrain
/// (everything beyond the streamed fine-tile ring) reads as real hills
/// with ridgelines, and together with the client's distance fog the
/// fine-to-coarse transition stops reading as "leftover placeholder
/// terrain". The one-time `terrain.data` message grows to (384+1)² ≈ 148k
/// height samples (~1.5MB of JSON), still a single push at session start.
pub const TERRAIN_RESOLUTION: i32 = 384;

/// Where the baked terrain artifact (issue #56's terrain pipeline; produced
/// by `terrain-bake`, see the repo-root `terrain.toml`) lives, unless
/// overridden by `TERRAIN_DATA_DIR`. Resolved at compile time relative to
/// this crate's own manifest directory so it doesn't depend on the process's
/// current working directory (which varies: the README's own instructions
/// run the server from inside `rust_server/`, but a workspace-wide `cargo
/// run -p proxy` from the repo root works too).
///
/// `world_v3`: the near-full-extent 25.6km Brisbane bake (1600 tiles) —
/// materially different from `world_v2`'s 6.4km crop (16x the area, plot
/// field moved to the west band), hence the new directory name rather than
/// overwriting `world_v2` in place.
const DEFAULT_TERRAIN_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../artifacts/world_v3");

static TERRAIN: std::sync::OnceLock<std::sync::Arc<terrain_common::Terrain>> = std::sync::OnceLock::new();

/// Load the baked terrain artifact once per process (subsequent calls clone
/// the cheap `Arc`, not the underlying tiles) and cache it.
///
/// This *is* the fix for the old client/server terrain mismatch class of bug
/// (#54): `sample_height` here is the exact same code path, reading the
/// exact same artifact, that the bake tool's own tests validate — there's no
/// second, independently-computed heightmap to disagree with. The wire
/// format sent to clients (`terrain.data`, see `proxy.rs::send_terrain`)
/// stays a flat `(TERRAIN_RESOLUTION+1)^2` grid exactly like the old
/// synthetic generator sent — deliberately decoupled from the artifact's own
/// internal resolution, so this flat backdrop grid never has to grow just
/// because the bake gets more detailed; it only changes what `sample_height`
/// returns at the same fixed sampling grid. This backdrop is the permanent,
/// zero-latency fallback terrain now that real high-resolution terrain
/// streaming exists (`terrain.tile_request`/`terrain.tile_data`,
/// `proxy.rs::send_terrain_tile`) — the client renders this coarse grid
/// everywhere, and layers genuinely native-resolution tiles on top near the
/// player, streamed in/out as they move (see
/// `client_godot/world/TerrainStreamer.gd`).
///
/// **Terrain-editing caveat (#72/#80)**: this is the immutable *base*.
/// Hand-authored edits live as deltas in the `terrain_delta` table and are
/// composited separately (`Terrain::sample_height_with_delta`; the client
/// composites onto streamed chunks). If you add a server-side gameplay
/// consumer of ground height, do **not** call `sample_height` directly --
/// go through `proxy.rs::composited_ground_height` (or replicate its
/// base-plus-delta composition), or edited terrain will be invisible to
/// your feature. The #80 audit confirmed no such consumer exists today.
pub fn loaded_terrain() -> std::sync::Arc<terrain_common::Terrain> {
    TERRAIN
        .get_or_init(|| {
            let dir = std::env::var("TERRAIN_DATA_DIR").unwrap_or_else(|_| DEFAULT_TERRAIN_DIR.to_string());
            let terrain = terrain_common::Terrain::load_dir(std::path::Path::new(&dir)).unwrap_or_else(|e| {
                panic!(
                    "failed to load terrain artifact from {dir} ({e}) — run `cargo run -p terrain-bake -- \
                     --config terrain.toml` from the repo root to (re)generate it, or set TERRAIN_DATA_DIR"
                )
            });
            std::sync::Arc::new(terrain)
        })
        .clone()
}

/// The whole authored capital.
#[derive(Debug, Clone)]
pub struct Capital {
    pub districts: Vec<District>,
    pub town_centre: (i32, i32),
    pub build_orders: Vec<SeedBuildOrder>,
    pub resource_nodes: Vec<ResourceNodeSpawn>,
    pub storage_points: Vec<StoragePoint>,
    pub build_boards: Vec<BuildBoard>,
    /// Authoritative heights — loaded once from the baked artifact (issue
    /// #63), not generated in-process. See [`loaded_terrain`].
    pub terrain: std::sync::Arc<terrain_common::Terrain>,
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

/// The Phase 1 capital: five named districts tiling the 25600x25600 (~655 km²)
/// world in a plus/cross layout — a central Civic Centre with Suburbs/Market
/// bands to its west/east and Craftworks/Old Quarter bands to its north/south —
/// a starter plot grid in the suburbs, a town-centre spawn at the world centre,
/// and a civic build board. No roads and no build orders are authored; both
/// start empty and are built at runtime (roads via mayor-issued build orders).
///
/// The suburbs (and their starter-plot field) sit in the WEST band: in the v3
/// full-extent Brisbane bake the east band reaches the river mouth and Moreton
/// Bay (flat sea-filled ground), while the west band is real inland hillside —
/// the bake's `capital_flatten_mask` placement (see `terrain.toml`) matches.
pub fn capital() -> Capital {
    // A plus/cross tiling: west/east bands span the full height; the middle column
    // (between them) splits into north/centre/south. Exact tiling, verified in
    // `districts_tile_the_world_without_gaps_or_overlap`.
    let side = WORLD_SIZE / 4; // 6400 — west/east band width, north/south band height
    let mid0 = side; // 6400
    let mid1 = WORLD_SIZE - side; // 19200

    let market = District {
        id: "market",
        name: "Market District",
        region: Rect::new(mid1, 0, WORLD_SIZE, WORLD_SIZE),
        safety: Safety::Safe,
        plot_grid: None,
    };
    let suburbs = District {
        id: "suburbs",
        name: "Starter Suburbs",
        region: Rect::new(0, 0, side, WORLD_SIZE),
        safety: Safety::Safe,
        // A generous starter grid: 12 columns x 20 rows = 240 plots — plots stay
        // scarce/premium, not an attempt at the design doc's long-term ~100k-plot
        // figure. plot 80 + gap 40 -> 120 per cell; 12 cols span 1400 < 6400,
        // 20 rows span 2360 < 25600. Anchored at the band's top-left (NW corner
        // of the world) — the terrain there is real, bake-flattened hillside.
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

    // Terrain (heights) is loaded from the baked artifact (issue #63), not
    // generated here — including the suburbs plot field's flattening (#55),
    // which is now authored once at bake time via the checked-in
    // `capital_flatten_mask` rather than computed on every boot (see the
    // repo-root `terrain.toml` and `loaded_terrain`'s doc comment).
    let terrain = loaded_terrain();

    // Town centre at the world centre, inside the Civic Centre band. This is the
    // spawn anchor and where the first build-order board lives.
    let town_centre = (WORLD_SIZE / 2, WORLD_SIZE / 2);
    let (tcx, tcy) = town_centre;

    // No authored build orders: the capital starts with none. City work (starting
    // with dirt paths) is commissioned at runtime by the mayor via `mayor.build_create`
    // rather than authored here — see `Db::insert_build_order`'s placement fields.
    let build_orders: Vec<SeedBuildOrder> = vec![];

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
        ResourceNodeSpawn { id: "node_market_tree_0", district: "market", item_id: "wood", x: 20800, y: 2800, qty: 5 },
        ResourceNodeSpawn { id: "node_market_tree_1", district: "market", item_id: "wood", x: 23600, y: 8800, qty: 5 },
        ResourceNodeSpawn { id: "node_market_rock_0", district: "market", item_id: "stone", x: 21600, y: 14400, qty: 5 },
        ResourceNodeSpawn { id: "node_market_tree_2", district: "market", item_id: "wood", x: 24000, y: 20000, qty: 5 },
        // (20400, 24400) drowned when #84's real water mask landed — the map's
        // SE corner is genuinely Moreton Bay. Relocated to the nearest dry
        // market-district ground: the Toohey-forest hillside at the band's
        // west edge (h ~140m — a rock node on a hill reads fine).
        ResourceNodeSpawn { id: "node_market_rock_1", district: "market", item_id: "stone", x: 19300, y: 22600, qty: 5 },
        ResourceNodeSpawn { id: "node_suburbs_tree_0", district: "suburbs", item_id: "wood", x: 1600, y: 3200, qty: 5 },
        ResourceNodeSpawn { id: "node_suburbs_tree_1", district: "suburbs", item_id: "wood", x: 4000, y: 9600, qty: 5 },
        ResourceNodeSpawn { id: "node_suburbs_rock_0", district: "suburbs", item_id: "stone", x: 2400, y: 16000, qty: 5 },
        ResourceNodeSpawn { id: "node_suburbs_tree_2", district: "suburbs", item_id: "wood", x: 4800, y: 20800, qty: 5 },
        ResourceNodeSpawn { id: "node_suburbs_rock_1", district: "suburbs", item_id: "stone", x: 1200, y: 24000, qty: 5 },
        ResourceNodeSpawn { id: "node_craftworks_tree_0", district: "craftworks", item_id: "wood", x: 8000, y: 1600, qty: 5 },
        ResourceNodeSpawn { id: "node_craftworks_rock_0", district: "craftworks", item_id: "stone", x: 12800, y: 3600, qty: 5 },
        ResourceNodeSpawn { id: "node_craftworks_tree_1", district: "craftworks", item_id: "wood", x: 17600, y: 2000, qty: 5 },
        ResourceNodeSpawn { id: "node_craftworks_rock_1", district: "craftworks", item_id: "stone", x: 11200, y: 5200, qty: 5 },
        ResourceNodeSpawn { id: "node_old_quarter_tree_0", district: "old_quarter", item_id: "wood", x: 8000, y: 20800, qty: 5 },
        ResourceNodeSpawn { id: "node_old_quarter_rock_0", district: "old_quarter", item_id: "stone", x: 12800, y: 23600, qty: 5 },
        ResourceNodeSpawn { id: "node_old_quarter_tree_1", district: "old_quarter", item_id: "wood", x: 17600, y: 21600, qty: 5 },
        ResourceNodeSpawn { id: "node_old_quarter_rock_1", district: "old_quarter", item_id: "stone", x: 11200, y: 24400, qty: 5 },
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
        assert_eq!(c.district_at(10, 10).unwrap().id, "suburbs");
        assert_eq!(c.district_at(12800, 12800).unwrap().id, "civic");
        assert_eq!(c.district_at(22000, 12800).unwrap().id, "market");
        assert_eq!(c.district_at(12800, 3200).unwrap().id, "craftworks");
        assert_eq!(c.district_at(12800, 22400).unwrap().id, "old_quarter");
        assert!(c.district_at(WORLD_SIZE, 0).is_none()); // outside (half-open)
        // Region centre routing survives shard geometry.
        let r = Rect::new(200, 0, 1600, 2400);
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

    /// #84 made the river/bay a real water mask (`sea_level_m = 0`). Two
    /// invariants any future rebake must preserve: the mask is genuinely
    /// non-empty (drowning needs real water), and every authored gameplay
    /// anchor — spawn, plots, resource nodes, storage points — is on dry
    /// land. The v3 crop was *placed* so these hold (see terrain.toml's
    /// header); this asserts it instead of assuming it.
    #[test]
    fn water_mask_is_real_and_authored_gameplay_points_stay_dry() {
        let c = capital();
        let t = loaded_terrain();

        let (tx, ty) = c.town_centre;
        assert!(!t.is_water(tx as f32, ty as f32), "spawn/town centre is underwater");

        for n in &c.resource_nodes {
            assert!(!t.is_water(n.x as f32, n.y as f32), "resource node {} is underwater", n.id);
        }
        for s in &c.storage_points {
            assert!(!t.is_water(s.x as f32, s.y as f32), "storage point {} is underwater", s.id);
        }
        for (_, cell) in c.starter_plots() {
            // Corners and centre — a plot partially in the river is still broken.
            for (px, py) in [
                (cell.x, cell.y),
                (cell.x + cell.w - 1, cell.y),
                (cell.x, cell.y + cell.h - 1),
                (cell.x + cell.w - 1, cell.y + cell.h - 1),
                (cell.x + cell.w / 2, cell.y + cell.h / 2),
            ] {
                assert!(
                    !t.is_water(px as f32, py as f32),
                    "plot ({},{}) is underwater at ({px},{py})",
                    cell.grid_x, cell.grid_y
                );
            }
        }

        // The mask is non-empty: a coarse 100m scan must find the river/bay
        // (~10% of the world). Guards against silently rebaking with the old
        // empty-mask sea level.
        let mut water_samples = 0u32;
        let mut total = 0u32;
        let step = 100;
        let mut y = step / 2;
        while y < WORLD_SIZE {
            let mut x = step / 2;
            while x < WORLD_SIZE {
                total += 1;
                if t.is_water(x as f32, y as f32) {
                    water_samples += 1;
                }
                x += step;
            }
            y += step;
        }
        let frac = water_samples as f64 / total as f64;
        assert!(
            frac > 0.05,
            "water mask looks empty ({water_samples}/{total} coarse samples) — was the bake run with the old sea_level_m = -25?"
        );
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
    fn capital_authors_no_roads_or_build_orders() {
        let c = capital();
        // Roads and city work are commissioned at runtime by the mayor now, not authored.
        assert!(c.build_orders.is_empty(), "the capital should start with no seeded build orders");
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
    fn terrain_loads_from_the_baked_artifact_deterministically_and_in_bounds() {
        let t1 = capital().terrain;
        let t2 = capital().terrain;
        // Cached via `loaded_terrain`'s `OnceLock` — literally the same `Arc`
        // (loaded from disk once per process), not just equal content.
        assert!(std::sync::Arc::ptr_eq(&t1, &t2), "the terrain artifact should be loaded once and cached");

        let manifest = t1.manifest();
        assert_eq!(manifest.world_size_m, (WORLD_SIZE as f32, WORLD_SIZE as f32));

        // Every sampled corner of the wire-format grid stays within the
        // artifact's own declared height range (a little slack for the u16
        // encoding's quantization).
        let step = WORLD_SIZE as f32 / TERRAIN_RESOLUTION as f32;
        for gy in 0..=TERRAIN_RESOLUTION {
            for gx in 0..=TERRAIN_RESOLUTION {
                let h = t1.sample_height(gx as f32 * step, gy as f32 * step);
                assert!(
                    h >= manifest.height_min_m - 0.05 && h <= manifest.height_max_m + 0.05,
                    "corner ({gx},{gy}) height {h} outside the artifact's declared [{}, {}] range",
                    manifest.height_min_m,
                    manifest.height_max_m
                );
            }
        }
    }

    #[test]
    fn terrain_is_smooth_not_a_jagged_sheet() {
        // Adjacent wire-grid corners should differ by a modest fraction of
        // the artifact's whole height range -- confirms the checked-in bake
        // is genuinely broad rolling hills, not a jagged sheet (a property
        // of the *data*, since the generator that used to live in this crate
        // is now `terrain-bake`'s job — see its own `synth`/`stylize` tests
        // for the generation-time guarantees).
        let t = capital().terrain;
        let manifest = t.manifest();
        let range = (manifest.height_max_m - manifest.height_min_m).max(0.001);
        let max_step = range * 0.5; // generous, well under the whole-range worst case
        let step = WORLD_SIZE as f32 / TERRAIN_RESOLUTION as f32;
        for gy in 0..=TERRAIN_RESOLUTION {
            for gx in 0..TERRAIN_RESOLUTION {
                let a = t.sample_height(gx as f32 * step, gy as f32 * step);
                let b = t.sample_height((gx + 1) as f32 * step, gy as f32 * step);
                assert!(
                    (a - b).abs() <= max_step,
                    "horizontal neighbours at ({gx},{gy}) differ by {} > {max_step}",
                    (a - b).abs()
                );
            }
        }
        for gy in 0..TERRAIN_RESOLUTION {
            for gx in 0..=TERRAIN_RESOLUTION {
                let a = t.sample_height(gx as f32 * step, gy as f32 * step);
                let b = t.sample_height(gx as f32 * step, (gy + 1) as f32 * step);
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
        // #55/#63: the suburbs starter plot field (240 plots) should be a
        // level plateau in the baked terrain artifact -- entities/markers are
        // placed via `Protocol.w2v`, which follows this same heightmap, so a
        // sloped plot field would clip structures on one side and float them
        // on the other. Flattening now happens once at bake time (the
        // repo-root `terrain.toml`'s `capital_flatten_mask`/
        // `capital_flatten_margin_m`), not in this crate — this test just
        // confirms the checked-in artifact actually has that property.
        const BAKED_FLATTEN_MARGIN: f32 = 100.0; // must match terrain.toml's capital_flatten_margin_m

        let c = capital();
        let suburbs = c.districts.iter().find(|d| d.id == "suburbs").unwrap();
        let cells = suburbs.plots();
        let x0 = cells.iter().map(|cell| cell.x).min().unwrap();
        let y0 = cells.iter().map(|cell| cell.y).min().unwrap();
        let x1 = cells.iter().map(|cell| cell.x + cell.w).max().unwrap();
        let y1 = cells.iter().map(|cell| cell.y + cell.h).max().unwrap();

        let t = &c.terrain;
        let step = WORLD_SIZE as f32 / TERRAIN_RESOLUTION as f32;

        // Every sampled corner well inside the plot field's bounding box (a
        // full margin in from every edge, so we're only sampling points
        // guaranteed to be at full flatten weight) must be level.
        let mut interior_heights = Vec::new();
        for gy in 0..=TERRAIN_RESOLUTION {
            for gx in 0..=TERRAIN_RESOLUTION {
                let wx = gx as f32 * step;
                let wy = gy as f32 * step;
                if wx >= x0 as f32 + BAKED_FLATTEN_MARGIN
                    && wx <= x1 as f32 - BAKED_FLATTEN_MARGIN
                    && wy >= y0 as f32 + BAKED_FLATTEN_MARGIN
                    && wy <= y1 as f32 - BAKED_FLATTEN_MARGIN
                {
                    interior_heights.push(t.sample_height(wx, wy));
                }
            }
        }
        assert!(
            !interior_heights.is_empty(),
            "expected at least one sampled corner well inside the plot field"
        );
        let first = interior_heights[0];
        for h in &interior_heights {
            assert!(
                (h - first).abs() < 0.05,
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

