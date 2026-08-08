//! `zones.toml` — authored **interior zones** (mine epic #164, issue #165).
//!
//! ## What an interior is, and why it needed a new kind
//!
//! Every zone until now owned a `Region { x0, y0, x1, y1 }`: a rectangle of the
//! 25600×25600 world. The gateway routes a player to a zone **by geometry**,
//! splits regions under load, merges them back, and hands players across
//! boundaries — all of which assume a zone's coordinates *are* world
//! coordinates.
//!
//! An interior has its own space. It is not a rectangle of the world, it must
//! never be handed a share of the world by a split, and a player standing in it
//! is at a position that means nothing on the surface map. So:
//!
//! * **Interiors are unreachable by geometry.** `zone_at` only ever considers
//!   surface zones, which makes an explicit portal the *only* way in or out.
//!   That is a feature: it means no amount of walking, dying or region
//!   reshuffling can drop somebody into the mine by accident.
//! * **A position without a zone is meaningless.** Two players at (100, 100) in
//!   different zones are not in the same place, and every proximity gate has to
//!   know that — see `EntityCache`'s `zone` field in the gateway.
//!
//! ## Geometry is a seam, not a format
//!
//! Phase 1 describes the mine as a handful of boxes, because the server already
//! validates positions against rectangles and "three short galleries" is about a
//! dozen of them. That skips a build pipeline, a binary format, a version
//! fallback and a Godot export step, none of which have to exist before ore can
//! be mined.
//!
//! The part worth getting right *now* is that every caller asks
//! [`InteriorZone::contains`] rather than reaching for the volume list. Swapping
//! boxes for a real navmesh blob later is then a change to this file and nothing
//! else.
//!
//! ## Same file, both binaries
//!
//! The gateway and the zone process both load this, exactly as they both load
//! `world::capital()`. The zone needs the volumes to validate movement; the
//! gateway needs the portals and anchors to route transitions. One file, one
//! parse, no wire format to keep in step.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

/// An axis-aligned box of walkable interior floor.
///
/// Half-open on the upper bound (`x0 <= x < x1`), matching `Region::contains` on
/// the surface so the two kinds of geometry can never disagree about an edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Volume {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl Volume {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }

    fn valid(&self) -> bool {
        self.x1 > self.x0 && self.y1 > self.y0
    }
}

/// A doorway between the surface world and an interior.
///
/// One entry describes both directions: `world` is where it stands outside,
/// `inside` is where you arrive. Keeping them together is deliberate — a portal
/// authored as two independent halves is a portal that can be authored
/// one-way by accident, which strands players.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Portal {
    pub id: String,
    /// Where it stands in the world.
    pub world: (i32, i32),
    /// Where you arrive inside.
    pub inside: (i32, i32),
    /// How close you must stand to use it, from either side. Enforced
    /// server-side against the gateway's position cache, like every other
    /// proximity gate here.
    #[serde(default = "default_portal_radius")]
    pub radius: i32,
}

fn default_portal_radius() -> i32 {
    40
}

/// One placed deposit (#166). The behaviour lives in `crafting.toml`; this is
/// only where it sits.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DepositPlacement {
    pub id: String,
    /// A key into `crafting.toml`'s `[deposit.*]`.
    #[serde(rename = "type")]
    pub kind: String,
    pub pos: (i32, i32),
}

/// Where a crafting station stands (#167). Behaviour lives in `crafting.toml`;
/// this is placement, the same split deposits use.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StationPlacement {
    pub id: String,
    /// A key into `crafting.toml`'s `[station.*]`.
    #[serde(rename = "type")]
    pub kind: String,
    pub pos: (i32, i32),
    /// The interior this stands in, or `None` for a surface fixture.
    ///
    /// A position alone is not a location: since #165 the same coordinates
    /// exist both underground and above it, so a station that only knew its
    /// (x, y) could be used from the wrong side of the rock.
    #[serde(default)]
    pub interior: Option<String>,
}

/// One authored interior.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteriorZone {
    pub display_name: String,
    /// Where a player lands with no better answer: a fresh entry, or a login
    /// whose stored position no longer fits the geometry.
    pub spawn_anchor: (i32, i32),
    /// Bumped by hand whenever the volumes change shape. A stored position from
    /// an older layout is not trusted — the player is placed at the anchor
    /// instead. Cheap insurance against loading someone inside a wall.
    #[serde(default = "default_geometry_version")]
    pub geometry_version: i64,
    /// Client hint only; the server has no opinion about light.
    #[serde(default)]
    pub ambient_light: f32,
    pub volumes: Vec<Volume>,
    pub portals: Vec<Portal>,
    /// Deposits placed in this interior (#166).
    #[serde(default)]
    pub deposits: Vec<DepositPlacement>,
}

fn default_geometry_version() -> i64 {
    1
}

impl InteriorZone {
    /// **The geometry seam.** Every caller asks this; nobody reads `volumes`.
    ///
    /// Swapping the boxes for an exported navmesh blob later is a change here
    /// and nowhere else, which is the whole reason this is a method rather than
    /// an open field.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        self.volumes.iter().any(|v| v.contains(x, y))
    }

    /// The nearest walkable point to (x, y), for putting somebody back on solid
    /// floor rather than refusing to place them at all. Falls back to the spawn
    /// anchor when nothing is close.
    pub fn nearest_walkable(&self, x: i32, y: i32) -> (i32, i32) {
        if self.contains(x, y) {
            return (x, y);
        }
        self.spawn_anchor
    }
}

/// Every authored interior, keyed by zone id.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZoneConfig {
    #[serde(default)]
    pub interior: BTreeMap<String, InteriorZone>,
    /// Where the world's crafting stations physically stand (#167). Top-level
    /// rather than nested under an interior because most stations are surface
    /// fixtures — the mine's furnace sits in the yard outside the adit, not
    /// down the tunnel.
    #[serde(default)]
    pub station: Vec<StationPlacement>,
}

impl ZoneConfig {
    pub fn interior(&self, zone_id: &str) -> Option<&InteriorZone> {
        self.interior.get(zone_id)
    }

    pub fn is_interior(&self, zone_id: &str) -> bool {
        self.interior.contains_key(zone_id)
    }

    /// The interior a world-side portal at (x, y) leads to, if one is in reach.
    pub fn portal_from_world(&self, x: i32, y: i32) -> Option<(&str, &Portal)> {
        self.interior.iter().find_map(|(id, z)| {
            z.portals
                .iter()
                .find(|p| near(p.world, x, y, p.radius))
                .map(|p| (id.as_str(), p))
        })
    }

    /// The portal out of `zone_id` that (x, y) is standing at, if any.
    pub fn portal_from_inside(&self, zone_id: &str, x: i32, y: i32) -> Option<&Portal> {
        self.interior
            .get(zone_id)?
            .portals
            .iter()
            .find(|p| near(p.inside, x, y, p.radius))
    }

    pub fn parse(toml_text: &str) -> Result<ZoneConfig, ZoneConfigError> {
        let cfg: ZoneConfig = toml::from_str(toml_text).map_err(ZoneConfigError::Toml)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// A missing file means "no interiors", which is exactly what the world was
    /// before #165 — so a fresh clone with no `zones.toml` runs the old world
    /// unchanged. A *malformed* one refuses to boot, same contract as
    /// `market.toml` (#152): silently ignoring a typo is how a server ends up
    /// running a layout nobody authored.
    pub fn load(path: &Path) -> Result<ZoneConfig, ZoneConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => ZoneConfig::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ZoneConfig::default()),
            Err(e) => Err(ZoneConfigError::Io(e)),
        }
    }

    fn validate(&self) -> Result<(), ZoneConfigError> {
        for (id, z) in &self.interior {
            let bad = |why: &str| {
                Err(ZoneConfigError::Invalid { zone: id.clone(), why: why.to_string() })
            };
            if z.volumes.is_empty() {
                return bad("has no volumes — there would be nowhere to stand");
            }
            if z.volumes.iter().any(|v| !v.valid()) {
                return bad("has an inside-out or zero-area volume");
            }
            // The anchor is where players land when nothing else fits, so an
            // anchor outside the floor would strand everyone it catches.
            if !z.contains(z.spawn_anchor.0, z.spawn_anchor.1) {
                return bad("spawn_anchor is outside its own volumes");
            }
            if z.portals.is_empty() {
                return bad("has no portals — it could be entered but never left");
            }
            // A deposit in the rock is unreachable, and would be authored
            // exactly as easily as a good one.
            for d in &z.deposits {
                if !z.contains(d.pos.0, d.pos.1) {
                    return Err(ZoneConfigError::Invalid {
                        zone: id.clone(),
                        why: format!("deposit `{}` is outside the volumes", d.id),
                    });
                }
            }
            let mut seen: Vec<&str> = z.deposits.iter().map(|d| d.id.as_str()).collect();
            seen.sort_unstable();
            let before = seen.len();
            seen.dedup();
            if seen.len() != before {
                return bad("has two deposits sharing an id");
            }
            for p in &z.portals {
                if !z.contains(p.inside.0, p.inside.1) {
                    return Err(ZoneConfigError::Invalid {
                        zone: id.clone(),
                        why: format!("portal `{}` arrives outside the volumes", p.id),
                    });
                }
                if p.radius <= 0 {
                    return Err(ZoneConfigError::Invalid {
                        zone: id.clone(),
                        why: format!("portal `{}` has a non-positive radius", p.id),
                    });
                }
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for st in &self.station {
            let bad = |why: &str| {
                Err(ZoneConfigError::Invalid { zone: format!("station {}", st.id), why: why.to_string() })
            };
            if !seen.insert(st.id.clone()) {
                return bad("is a duplicate id — a station is addressed by id, so two would be one");
            }
            // A station inside an interior must be somewhere a player can stand,
            // or it is unreachable and silently so. Surface stations are checked
            // against the world bounds only; the terrain under them isn't ours
            // to validate here.
            match &st.interior {
                Some(zone) => match self.interior.get(zone) {
                    None => return bad("names an interior that doesn't exist"),
                    Some(z) if !z.contains(st.pos.0, st.pos.1) => {
                        return bad("stands inside solid rock — nobody could reach it")
                    }
                    Some(_) => {}
                },
                None => {
                    if st.pos.0 < 0 || st.pos.1 < 0 {
                        return bad("stands outside the world");
                    }
                }
            }
        }
        Ok(())
    }
}

fn near(at: (i32, i32), x: i32, y: i32, radius: i32) -> bool {
    let (dx, dy) = ((at.0 - x) as i64, (at.1 - y) as i64);
    dx * dx + dy * dy <= (radius as i64).pow(2)
}

#[derive(Debug)]
pub enum ZoneConfigError {
    Toml(toml::de::Error),
    Io(std::io::Error),
    Invalid { zone: String, why: String },
}

impl std::fmt::Display for ZoneConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ZoneConfigError::Toml(e) => write!(f, "zones.toml parse error: {e}"),
            ZoneConfigError::Io(e) => write!(f, "zones.toml read error: {e}"),
            ZoneConfigError::Invalid { zone, why } => {
                write!(f, "zones.toml [interior.{zone}]: {why}")
            }
        }
    }
}
impl std::error::Error for ZoneConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_mine() -> &'static str {
        r#"
        [interior.mine_starter]
        display_name = "Kedron Cut"
        spawn_anchor = [20, 20]
        geometry_version = 3
        ambient_light = 0.08

        [[interior.mine_starter.volumes]]
        x0 = 0
        y0 = 0
        x1 = 60
        y1 = 40

        [[interior.mine_starter.volumes]]
        x0 = 60
        y0 = 10
        x1 = 200
        y1 = 30

        [[interior.mine_starter.portals]]
        id = "adit_mouth"
        world = [12000, 12000]
        inside = [20, 20]
        radius = 40
        "#
    }

    /// A world with no `zones.toml` is exactly the world before #165 — no
    /// interiors, nothing changed. Config adds a place; it isn't required to
    /// have one.
    #[test]
    fn a_missing_file_means_no_interiors() {
        let cfg = ZoneConfig::load(Path::new("no_such_zones.toml")).unwrap();
        assert!(cfg.interior.is_empty());
        assert!(!cfg.is_interior("mine_starter"));
        assert!(cfg.portal_from_world(0, 0).is_none());
    }

    #[test]
    fn an_interior_parses_with_its_geometry_and_portals() {
        let cfg = ZoneConfig::parse(a_mine()).unwrap();
        let mine = cfg.interior("mine_starter").expect("the mine");
        assert_eq!(mine.display_name, "Kedron Cut");
        assert_eq!(mine.geometry_version, 3);
        assert_eq!(mine.volumes.len(), 2);
        assert!(cfg.is_interior("mine_starter"));
        assert!(!cfg.is_interior("zone_a"), "a surface zone is not an interior");
    }

    /// The seam every caller uses. Boxes today, a navmesh blob later — the
    /// question asked is the same either way.
    #[test]
    fn contains_answers_for_the_whole_floor_and_refuses_the_rock() {
        let cfg = ZoneConfig::parse(a_mine()).unwrap();
        let mine = cfg.interior("mine_starter").unwrap();
        // Inside the entrance chamber.
        assert!(mine.contains(0, 0));
        assert!(mine.contains(59, 39));
        // Inside the gallery.
        assert!(mine.contains(150, 20));
        // Solid rock between and around them.
        assert!(!mine.contains(150, 5), "above the gallery is rock");
        assert!(!mine.contains(60, 39), "the chamber's corner is not the gallery");
        assert!(!mine.contains(-1, 0));
        assert!(!mine.contains(200, 20), "half-open on the upper bound, like Region");
    }

    /// Off the floor, a player goes to the anchor rather than being refused a
    /// position altogether — being stuck outside geometry is worse than being
    /// moved.
    #[test]
    fn nearest_walkable_keeps_you_on_the_floor() {
        let cfg = ZoneConfig::parse(a_mine()).unwrap();
        let mine = cfg.interior("mine_starter").unwrap();
        assert_eq!(mine.nearest_walkable(30, 30), (30, 30));
        assert_eq!(mine.nearest_walkable(9999, 9999), mine.spawn_anchor);
    }

    #[test]
    fn portals_resolve_from_both_sides_and_respect_their_radius() {
        let cfg = ZoneConfig::parse(a_mine()).unwrap();
        let (zone, portal) = cfg.portal_from_world(12010, 12010).expect("in reach outside");
        assert_eq!(zone, "mine_starter");
        assert_eq!(portal.id, "adit_mouth");
        assert_eq!(portal.inside, (20, 20));
        assert!(cfg.portal_from_world(12200, 12000).is_none(), "out of reach");

        assert!(cfg.portal_from_inside("mine_starter", 22, 22).is_some());
        assert!(cfg.portal_from_inside("mine_starter", 190, 20).is_none(), "deep in the gallery");
        assert!(cfg.portal_from_inside("zone_a", 20, 20).is_none(), "not an interior");
    }

    /// Every one of these strands somebody. A malformed layout refuses the boot
    /// rather than being discovered by the first player it swallows.
    #[test]
    fn a_layout_that_would_strand_a_player_refuses_to_boot() {
        let cases = [
            // No floor at all.
            (
                r#"[interior.m]
                   display_name = "M"
                   spawn_anchor = [0, 0]
                   volumes = []
                   portals = []"#,
                "volumes",
            ),
            // Anchor in the rock: everyone who falls back to it is stuck.
            (
                r#"[interior.m]
                   display_name = "M"
                   spawn_anchor = [500, 500]
                   [[interior.m.volumes]]
                   x0 = 0
                   y0 = 0
                   x1 = 10
                   y1 = 10
                   [[interior.m.portals]]
                   id = "p"
                   world = [1, 1]
                   inside = [5, 5]"#,
                "spawn_anchor",
            ),
            // Enterable but not leavable.
            (
                r#"[interior.m]
                   display_name = "M"
                   spawn_anchor = [5, 5]
                   portals = []
                   [[interior.m.volumes]]
                   x0 = 0
                   y0 = 0
                   x1 = 10
                   y1 = 10"#,
                "portals",
            ),
            // Arrive inside a wall.
            (
                r#"[interior.m]
                   display_name = "M"
                   spawn_anchor = [5, 5]
                   [[interior.m.volumes]]
                   x0 = 0
                   y0 = 0
                   x1 = 10
                   y1 = 10
                   [[interior.m.portals]]
                   id = "p"
                   world = [1, 1]
                   inside = [900, 900]"#,
                "arrives outside",
            ),
            // Inside-out volume.
            (
                r#"[interior.m]
                   display_name = "M"
                   spawn_anchor = [5, 5]
                   [[interior.m.volumes]]
                   x0 = 10
                   y0 = 0
                   x1 = 0
                   y1 = 10
                   [[interior.m.portals]]
                   id = "p"
                   world = [1, 1]
                   inside = [5, 5]"#,
                "volume",
            ),
        ];
        for (text, want) in cases {
            let err = match ZoneConfig::parse(text) {
                Ok(_) => panic!("should have been refused: {text}"),
                Err(e) => e.to_string(),
            };
            assert!(err.contains(want), "error should mention `{want}`: {err}");
        }
    }

    #[test]
    fn a_misspelled_key_refuses_to_boot_and_names_it() {
        let err = ZoneConfig::parse(
            r#"[interior.m]
               display_name = "M"
               spwan_anchor = [5, 5]"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("spwan_anchor"), "unhelpful error: {err}");
    }
}
