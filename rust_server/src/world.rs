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

/// Edge length of the (square) world, in world units. Mirrors the same constant in
/// `proxy.rs` / `zone_server.rs`.
pub const WORLD_SIZE: i32 = 1200;

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
    ]
}

/// Look up an item definition by id.
pub fn item(id: &str) -> Option<Item> {
    items().into_iter().find(|i| i.id == id)
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

/// The whole authored capital.
#[derive(Debug, Clone)]
pub struct Capital {
    pub districts: Vec<District>,
    pub roads: Vec<RoadSegment>,
    pub town_centre: (i32, i32),
    pub build_orders: Vec<SeedBuildOrder>,
    pub resource_nodes: Vec<ResourceNodeSpawn>,
    pub storage_points: Vec<StoragePoint>,
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
}

/// The Phase 1 capital: three named districts tiling the 1200x1200 world as
/// vertical bands, a main avenue connecting their centres, a starter plot grid in
/// the suburbs, a town-centre spawn at the world centre, and the first build order
/// (the Town Well) on the civic centre board.
pub fn capital() -> Capital {
    let third = WORLD_SIZE / 3; // 400

    let market = District {
        id: "market",
        name: "Market District",
        region: Rect::new(0, 0, third, WORLD_SIZE),
        safety: Safety::Safe,
        plot_grid: None,
    };
    let civic = District {
        id: "civic",
        name: "Civic Centre",
        region: Rect::new(third, 0, 2 * third, WORLD_SIZE),
        safety: Safety::Safe,
        plot_grid: None,
    };
    let suburbs = District {
        id: "suburbs",
        name: "Starter Suburbs",
        region: Rect::new(2 * third, 0, WORLD_SIZE, WORLD_SIZE),
        safety: Safety::Safe,
        // A generous starter grid: 3 columns x 8 rows = 24 plots, inset from the
        // band edges. plot 80 + gap 40 -> 120 per cell; 3 cols span 320 < 400.
        plot_grid: Some(PlotGrid {
            cols: 3,
            rows: 8,
            margin: 40,
            plot_w: 80,
            plot_h: 80,
            gap: 40,
            tier: 0,
        }),
    };

    // Town centre at the world centre, inside the Civic Centre band. This is the
    // spawn anchor and where the first build-order board lives.
    let town_centre = (WORLD_SIZE / 2, WORLD_SIZE / 2);

    // Main avenue: connects the three district centres at the world's mid-latitude.
    let mid_y = WORLD_SIZE / 2;
    let roads = vec![
        RoadSegment { x0: market.region.centre().0, y0: mid_y, x1: suburbs.region.centre().0, y1: mid_y },
        // A civic cross-street north-south through the town centre.
        RoadSegment { x0: town_centre.0, y0: 0, x1: town_centre.0, y1: WORLD_SIZE },
    ];

    let build_orders = vec![SeedBuildOrder {
        district: civic.id,
        kind: "town_well",
        required_json: r#"{"wood":20,"stone":10}"#,
    }];

    // Gatherable nodes. A grove of trees ringing the town centre (so a fresh
    // spawn finds wood immediately) plus wood/stone scattered through the
    // districts. Ids are stable so a node keeps its identity across respawns.
    let (tcx, tcy) = town_centre;
    let resource_nodes = vec![
        ResourceNodeSpawn { id: "node_civic_tree_0", district: "civic", item_id: "wood", x: tcx - 60, y: tcy - 60, qty: 5 },
        ResourceNodeSpawn { id: "node_civic_tree_1", district: "civic", item_id: "wood", x: tcx + 60, y: tcy - 60, qty: 5 },
        ResourceNodeSpawn { id: "node_civic_tree_2", district: "civic", item_id: "wood", x: tcx - 60, y: tcy + 60, qty: 5 },
        ResourceNodeSpawn { id: "node_civic_tree_3", district: "civic", item_id: "wood", x: tcx + 60, y: tcy + 60, qty: 5 },
        ResourceNodeSpawn { id: "node_civic_rock_0", district: "civic", item_id: "stone", x: tcx, y: tcy - 110, qty: 5 },
        ResourceNodeSpawn { id: "node_market_tree_0", district: "market", item_id: "wood", x: 180, y: 400, qty: 5 },
        ResourceNodeSpawn { id: "node_market_tree_1", district: "market", item_id: "wood", x: 250, y: 760, qty: 5 },
        ResourceNodeSpawn { id: "node_market_rock_0", district: "market", item_id: "stone", x: 120, y: 600, qty: 5 },
        ResourceNodeSpawn { id: "node_suburbs_tree_0", district: "suburbs", item_id: "wood", x: 1000, y: 300, qty: 5 },
        ResourceNodeSpawn { id: "node_suburbs_tree_1", district: "suburbs", item_id: "wood", x: 1050, y: 900, qty: 5 },
        ResourceNodeSpawn { id: "node_suburbs_rock_0", district: "suburbs", item_id: "stone", x: 1120, y: 600, qty: 5 },
    ];

    // A public town storehouse beside the town centre (the M2 stash). Per-plot
    // home storage (#12) will add more storage points using the same protocol.
    let storage_points = vec![StoragePoint {
        id: "storehouse_town",
        district: "civic",
        x: tcx + 30,
        y: tcy + 10,
    }];

    Capital {
        districts: vec![market, civic, suburbs],
        roads,
        town_centre,
        build_orders,
        resource_nodes,
        storage_points,
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
        assert_eq!(c.district_at(600, 600).unwrap().id, "civic");
        assert_eq!(c.district_at(1100, 600).unwrap().id, "suburbs");
        assert!(c.district_at(WORLD_SIZE, 0).is_none()); // outside (half-open)
        // Region centre routing survives shard geometry.
        let r = Rect::new(800, 0, 1200, 600);
        assert_eq!(c.district_for_region(r).unwrap().id, "suburbs");
    }

    #[test]
    fn starter_plots_are_authored_and_inside_the_suburbs() {
        let c = capital();
        let suburbs = c.districts.iter().find(|d| d.id == "suburbs").unwrap();
        let cells = suburbs.plots();
        assert_eq!(cells.len(), 24); // 3 x 8
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
        assert_eq!(c.starter_plots().len(), 24);
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
    fn capital_has_roads_and_a_first_build_order() {
        let c = capital();
        assert!(!c.roads.is_empty(), "the capital should have an authored road graph");
        assert_eq!(c.build_orders.len(), 1);
        let well = &c.build_orders[0];
        assert_eq!(well.kind, "town_well");
        assert_eq!(well.district, "civic");
        // required_json is valid JSON.
        let v: serde_json::Value = serde_json::from_str(well.required_json).unwrap();
        assert!(v.get("wood").is_some());
    }
}
