// Zone server (spatial-partition model).
//
// The world is a single WORLD_SIZE x WORLD_SIZE space. Each zone owns a
// rectangular sub-region of it and holds the players currently inside that
// region. Entities use WORLD coordinates.
//
// Zones know nothing about their neighbours: when a player moves out of this
// zone's region, the zone asks the gateway to hand it off, and the gateway
// (the authority on the partition) routes it to whichever zone owns the
// destination point. The gateway also shrinks a zone's region on a split via
// `set_region`.
//
// Usage: zone_server <zone_id> <port> [proxy_uri] [--region x0 y0 x1 y1]
//   The default region is the whole world; the gateway carves it up by splitting.

use std::collections::{HashMap, HashSet};
use std::env;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::protocol::Message;

use mmo::util::dist2;
use mmo::world::WORLD_SIZE;

// --- Simulation / combat tuning ------------------------------------------------
const TICK_MS: u64 = 50; // 20 Hz authoritative simulation
const PLAYER_MAX_HP: i32 = 100;

const MOBS_PER_ZONE: usize = 8;
const MOB_MAX_HP: i32 = 40;
const MOB_SPEED: i32 = 3; // world units per tick when moving
const MOB_WANDER_SPEED: i32 = 2;
const AGGRO_RADIUS: i32 = 180; // start chasing a player within this range
const MOB_ATTACK_RANGE: i32 = 18; // contact range to damage a player
const MOB_DAMAGE: i32 = 6;
const MOB_ATTACK_COOLDOWN: i32 = 8; // ticks between a mob's hits (~0.4s)

const MELEE_RANGE: i32 = 60; // how far a swing reaches
const MELEE_ARC_COS: f64 = 0.0; // cos(90deg): hit within +/-90deg of facing
// Melee damage moved to `world::melee_damage` in #160: it depends on the
// equipped weapon, which lives in the gateway's DB, so the zone is told rather
// than deciding. `world::MELEE_DAMAGE_BARE` is the unarmed value and the
// default a freshly created entity carries.
const PLAYER_ATTACK_COOLDOWN: i32 = 6; // ticks between swings (~0.3s)

// --- Territory control (capture bar) ------------------------------------------
const MOB_RESPAWN_TICKS: i32 = 40; // a killed mob trickles back ~every 2s
/// How long an authored creature (#158) stays dead before returning to its
/// authored spot. Much slower than the ambient trickle: clearing a pack should
/// feel like an accomplishment that lasts a little while, and a bounty that
/// respawned under your feet would be farming rather than hunting.
const AUTHORED_MOB_RESPAWN_TICKS: i32 = 600; // ~30s at 20 Hz
const CAPTURE_MOB_THRESHOLD: usize = 2; // capture only progresses at/below this many mobs
const CAPTURE_RATE: f32 = 1.0; // bar units/tick while capturing (~5s to take a zone)
const CAPTURE_DECAY: f32 = 0.5; // bar units/tick lost when a capture stalls

// --- Resource nodes -------------------------------------------------------
const NODE_RESPAWN_TICKS: i32 = 200; // a depleted node refills after ~10s

// --- Abilities (mining/abilities epic #123, #117; generalized in #125) --------
/// A swing must be this close to its target node — a deliberate melee-ish
/// action, not the old channel's "stand near and wait". Shared by every
/// harvesting ability (Pick, Chop, ...), not pick-specific despite the name
/// history — kept generic on purpose since #125 added a second ability.
const SWING_RANGE: i32 = 8;
/// Must be this close to an NPC to talk to it — tighter than a swing,
/// since talking is a deliberate face-to-face action.
const NPC_TALK_RANGE: i32 = 10;

// --- Environmental vitals (player-attributes epic #83, #87) --------------------
// The zone knows no terrain: "underwater" arrives from the gateway as an
// `env_state` flag (~1/s, computed against composited ground height vs sea
// level). These consts turn that flag into drain/damage in the tick.
/// Full lungs: ~10s of submersion before suffocation starts (20 Hz ticks).
const BREATH_MAX_TICKS: i32 = 200;
/// Breath regained per tick while not submerged — empty to full in ~3.3s.
const BREATH_REFILL_PER_TICK: i32 = 3;
/// Suffocation once breath is gone: `DROWN_DAMAGE` hp every
/// `DROWN_PERIOD_TICKS` = 15 hp/s — dead from full hp in ~6.7s. Deliberately
/// faster than any mob: deep water is the pen around the starting area, not
/// a place to linger.
const DROWN_DAMAGE: i32 = 3;
const DROWN_PERIOD_TICKS: i32 = 4;
/// Poison buildup (#88) accrues while near poison trees (`poison_sources`
/// > 0 in the gateway's env_state) and PROCS at this threshold.
const POISON_PROC_AT: i32 = 100;
/// Buildup per tick near one tree — 5s from clean to proc, so the forest
/// EDGE telegraphs (buildup visibly climbing) with time to turn around.
/// Each additional 2 trees in range add +1/tick: a dense forest interior
/// procs in a second or two ("mildly with source count").
const POISON_BUILDUP_PER_TICK: i32 = 1;
/// Buildup shed per tick once clear of every tree (~2.5s from the brink
/// back to clean) — escaping the edge in time really does save you.
const POISON_DECAY_PER_TICK: i32 = 2;
/// Once procced, poison deals 1 hp/tick = 20 hp/s — dead from full hp in
/// 5s, and there is no cure in v1: the proc IS the death sentence, only
/// respawn clears it. "Dies quick", per the design.
const POISON_DAMAGE_PER_TICK: i32 = 1;

// --- Storage ------------------------------------------------------------------
const STORAGE_RANGE: i32 = 60; // must be within this of a storage point to use it

// --- Home structures (#13) -----------------------------------------------------
const HOME_STRUCTURE_RANGE: i32 = 60; // must be within this of a placed bed/storage/crafting

type Tx = mpsc::UnboundedSender<Message>;

#[derive(Clone, Copy, PartialEq, Debug)]
enum EntityKind {
    Player,
    Mob,
}

impl EntityKind {
    fn as_str(&self) -> &'static str {
        match self {
            EntityKind::Player => "player",
            EntityKind::Mob => "mob",
        }
    }
}

/// A half-open rectangular region of the world: [x0, x1) x [y0, y1).
#[derive(Clone, Copy)]
struct Region {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

impl Region {
    fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }
    fn random_point(&self) -> (i32, i32) {
        let mut rng = rand::thread_rng();
        (
            rng.gen_range(self.x0..self.x1.max(self.x0 + 1)),
            rng.gen_range(self.y0..self.y1.max(self.y0 + 1)),
        )
    }
}

/// Runtime state for an authored creature (#158). The position is authored and
/// immutable — a killed dog comes back where it was put, not wherever the RNG
/// felt like, so a pack stays a landmark across a session.
struct AuthoredMob {
    species: &'static str,
    x: i32,
    y: i32,
    /// Ticks until it comes back; 0 while it is alive.
    respawn_timer: i32,
}

struct Entity {
    x: i32,
    y: i32,
    hp: i32,
    max_hp: i32,
    kind: EntityKind,
    /// Last non-zero movement direction; melee swings are aimed along this.
    facing: (i32, i32),
    /// Ticks remaining before this entity may attack again.
    attack_cooldown: i32,
    /// Player flagged as swinging this tick (resolved + cleared in the tick).
    swinging: bool,
    /// Mob wander heading, re-rolled periodically when no player is in range.
    wander: (i32, i32),
    /// Underwater, per the gateway's latest `env_state` push (#87) — the zone
    /// knows no terrain, so this is the gateway's verdict, not a computation.
    /// Defaults to false on (re)creation; the gateway re-pushes every player's
    /// flags ~1/s, so a migrated/respawned entity reconverges within a tick of
    /// that cadence.
    submerged: bool,
    /// Breath remaining, in ticks (players only; full at spawn). Drains while
    /// `submerged`, refills fast on surfacing; at 0 suffocation damage starts.
    breath: i32,
    /// Cycles 0..DROWN_PERIOD_TICKS while suffocating, so the per-second
    /// damage rate is tick-rate-independent of the integer hp type.
    drown_phase: i32,
    /// Poison trees currently in range, per the gateway's `env_state` (#88) —
    /// same push/default/reconverge story as `submerged`.
    poison_sources: i32,
    /// 0..=POISON_PROC_AT: rises near trees, decays clear of them.
    poison_buildup: i32,
    /// The proc: sticks until death (no cure in v1); only respawn clears it.
    poisoned: bool,
    /// What kind of creature this is (#158), for authored mobs. `None` for
    /// players and for the ambient mobs `spawn_mobs` scatters — those stay the
    /// anonymous territory population they have always been, and only authored
    /// creatures are named content.
    species: Option<&'static str>,
    /// Where an authored creature belongs (#158). Past
    /// `world::AUTHORED_MOB_LEASH` from here it abandons whatever it is doing
    /// and walks back — see that constant for why. `None` for players and
    /// ambient mobs, which roam their whole region freely as they always have.
    home: Option<(i32, i32)>,
    /// Who struck the blow that took this creature from alive to dead (#159).
    ///
    /// Set ONLY by the hit that crosses hp from `> 0` to `<= 0`, never by a
    /// later one landing on a corpse in the same tick — otherwise two players
    /// swinging together would credit whoever the iteration order happened to
    /// reach last, rather than whoever actually killed it.
    killed_by: Option<String>,
    /// Damage this player's swing deals (#160), as the gateway most recently
    /// reported it — the gateway owns equipment, this zone just stores the
    /// verdict, exactly like `submerged` and `poison_sources` before it.
    ///
    /// Defaults to bare-handed, which is also the correct value for a recreated
    /// entity until the gateway's next push: a migrated player briefly punching
    /// instead of slashing is a far better failure than one briefly swinging a
    /// sword they no longer own.
    melee_damage: i64,
}

impl Entity {
    fn player(x: i32, y: i32, hp: i32) -> Self {
        Entity {
            x,
            y,
            hp,
            max_hp: PLAYER_MAX_HP,
            kind: EntityKind::Player,
            facing: (1, 0),
            attack_cooldown: 0,
            swinging: false,
            wander: (0, 0),
            submerged: false,
            breath: BREATH_MAX_TICKS,
            drown_phase: 0,
            poison_sources: 0,
            poison_buildup: 0,
            poisoned: false,
            species: None,
            home: None,
            killed_by: None,
            melee_damage: mmo::world::MELEE_DAMAGE_BARE,
        }
    }

    /// An ambient mob: no species, random placement, counts toward territory.
    /// An authored creature: named, fixed-place, and content rather than
    /// ambience. Identical to an ambient mob in every combat respect — same hp,
    /// same AI — so nothing in the tick has to special-case it.
    fn authored_mob(species: &'static str, x: i32, y: i32) -> Self {
        Entity { species: Some(species), home: Some((x, y)), ..Entity::mob(x, y) }
    }

    fn mob(x: i32, y: i32) -> Self {
        let mut rng = rand::thread_rng();
        Entity {
            x,
            y,
            hp: MOB_MAX_HP,
            max_hp: MOB_MAX_HP,
            kind: EntityKind::Mob,
            facing: (1, 0),
            attack_cooldown: 0,
            swinging: false,
            wander: (rng.gen_range(-1..=1), rng.gen_range(-1..=1)),
            submerged: false,
            breath: BREATH_MAX_TICKS,
            drown_phase: 0,
            poison_sources: 0,
            poison_buildup: 0,
            poisoned: false,
            species: None,
            home: None,
            killed_by: None,
            melee_damage: mmo::world::MELEE_DAMAGE_BARE,
        }
    }
}

/// A gatherable node's runtime (cache-only) state. The authored spawn lives in
/// `mmo::world`; this tracks the current quantity and respawn countdown.
struct ResourceNode {
    id: String,
    item_id: String,
    x: i32,
    y: i32,
    qty: i64,
    max_qty: i64,
    respawn_timer: i32, // ticks until refill while depleted (qty == 0)
}

/// A placed home structure's identity/position, as pushed by the gateway (the
/// only party with DB access — see `home_structures_sync`/`home_structure_added`
/// below). The zone only needs kind+position for proximity gating; ownership
/// and everything else durable stays gateway-side (#13).
#[derive(Clone)]
struct HomeStructureRef {
    id: String,
    kind: String,
    x: i32,
    y: i32,
}

fn node_status_json(n: &ResourceNode) -> Value {
    json!({
        "type": "status_update",
        "player_id": n.id,
        "state": {
            "x": n.x, "y": n.y, "type": "resource",
            "item_id": n.item_id, "qty": n.qty,
            "hp": n.qty, "max_hp": n.max_qty, "facing": [0, 0],
        },
    })
}

fn storage_status_json(s: &mmo::world::StoragePoint) -> Value {
    json!({
        "type": "status_update",
        "player_id": s.id,
        "state": { "x": s.x, "y": s.y, "type": "storage", "facing": [0, 0] },
    })
}

fn build_board_status_json(b: &mmo::world::BuildBoard) -> Value {
    json!({
        "type": "status_update",
        "player_id": b.id,
        "state": { "x": b.x, "y": b.y, "type": "build_board", "facing": [0, 0] },
    })
}

/// An authored NPC as a render entity (mining/abilities epic #123, #118) —
/// same `status_update` shape every other zone-cached entity uses, so it
/// renders through the client's ordinary spawn path with no special case.
fn npc_status_json(n: &mmo::world::NpcSpawn) -> Value {
    json!({
        "type": "status_update",
        "player_id": n.id,
        "state": { "x": n.x, "y": n.y, "type": "npc", "name": n.name, "facing": [0, 0] },
    })
}

fn clamp_world(x: i32, y: i32) -> (i32, i32) {
    (x.clamp(0, WORLD_SIZE - 1), y.clamp(0, WORLD_SIZE - 1))
}

/// Keep a point inside a zone's region (used so mobs stay in their own zone).
fn clamp_region(r: &Region, x: i32, y: i32) -> (i32, i32) {
    (
        x.clamp(r.x0, (r.x1 - 1).max(r.x0)),
        y.clamp(r.y0, (r.y1 - 1).max(r.y0)),
    )
}

/// A unit-ish step of `speed` world units from (fx,fy) toward (tx,ty).
fn step_toward(fx: i32, fy: i32, tx: i32, ty: i32, speed: i32) -> (i32, i32) {
    let dx = (tx - fx) as f64;
    let dy = (ty - fy) as f64;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1.0 {
        return (0, 0);
    }
    (
        (dx / len * speed as f64).round() as i32,
        (dy / len * speed as f64).round() as i32,
    )
}

/// Is target (mx,my) within `MELEE_RANGE` and inside the facing arc of a player
/// at (px,py) facing (fx,fy)?
fn in_melee_arc(px: i32, py: i32, fx: i32, fy: i32, mx: i32, my: i32) -> bool {
    let vx = (mx - px) as f64;
    let vy = (my - py) as f64;
    let d = (vx * vx + vy * vy).sqrt();
    if d > MELEE_RANGE as f64 {
        return false;
    }
    if d < 1.0 {
        return true; // standing on top of us
    }
    let fl = ((fx * fx + fy * fy) as f64).sqrt();
    if fl == 0.0 {
        return true;
    }
    (vx * fx as f64 + vy * fy as f64) / (d * fl) >= MELEE_ARC_COS
}

fn entity_status_json(id: &str, e: &Entity) -> Value {
    let mut state = json!({
        "x": e.x, "y": e.y, "hp": e.hp, "max_hp": e.max_hp,
        "type": e.kind.as_str(),
        "facing": [e.facing.0, e.facing.1],
    });
    // Vitals (#87/#88) ride the same state dict hp does, players only — the
    // HUD (#89) shows a breath meter while submerged and a poison gauge
    // while buildup is non-zero.
    // Species (#158) so a client can name and draw a wild dog as a wild dog
    // rather than as another anonymous blob. Absent for players and ambient
    // mobs, which is exactly the distinction it exists to make.
    if let Some(species) = e.species {
        state["species"] = json!(species);
    }
    if e.kind == EntityKind::Player {
        state["breath"] = json!(e.breath);
        state["max_breath"] = json!(BREATH_MAX_TICKS);
        state["submerged"] = json!(e.submerged);
        state["poison_buildup"] = json!(e.poison_buildup);
        state["max_poison"] = json!(POISON_PROC_AT);
        state["poisoned"] = json!(e.poisoned);
    }
    json!({
        "type": "status_update",
        "player_id": id,
        "state": state,
    })
}

struct ZoneServer {
    zone_id: String,
    port: u16,
    proxy_uri: Option<String>,
    version: u32,
    /// This zone's slice of the world. Mutable: the gateway shrinks it on split.
    region: Mutex<Region>,
    /// Players and mobs currently in this zone, in world coordinates.
    entities: Mutex<HashMap<String, Entity>>,
    proxy_tx: Mutex<Option<Tx>>,
    /// Monotonic counter for unique mob ids within this zone.
    mob_counter: Mutex<u64>,
    /// The authored world, used to decide whether this zone's region is a safe
    /// capital district (zero-PvP, no mob aggression) or open wilds.
    capital: mmo::world::Capital,
    /// Gatherable resource nodes in this zone's region (cache-only runtime state),
    /// keyed by node id.
    nodes: Mutex<HashMap<String, ResourceNode>>,
    /// Authored creatures in this zone's region (#158), keyed by their authored
    /// id — which is also the live entity id, so a death is matched back to its
    /// spawn without a second lookup. Runtime state is just the respawn timer;
    /// the position is authored and never drifts.
    authored_mobs: Mutex<HashMap<String, AuthoredMob>>,
    /// Authored storage access points in this zone's region (deposit/withdraw spots).
    storage_points: Mutex<Vec<mmo::world::StoragePoint>>,
    /// Authored build-order boards in this zone's region (contribution spots).
    build_boards: Mutex<Vec<mmo::world::BuildBoard>>,
    /// Authored plot cells in this zone's region — geometry only (not ownership,
    /// which lives in the gateway's DB); gates home-structure placement to
    /// "on some plot" (#12).
    plots: Mutex<Vec<mmo::world::PlotCell>>,
    /// Placed home structures (bed/storage/crafting) in this zone's region, as
    /// pushed by the gateway — gates deposit/withdraw/craft to "near the
    /// specific structure", not just "on some plot" (#13).
    home_structures: Mutex<Vec<HomeStructureRef>>,
    /// Authored NPCs in this zone's region (mining/abilities epic #123, #118) —
    /// fixed, never despawn, no runtime state beyond the authored spawn itself.
    npcs: Mutex<Vec<mmo::world::NpcSpawn>>,
}

impl ZoneServer {
    fn new(zone_id: String, port: u16, proxy_uri: Option<String>, region: Region, version: u32) -> Arc<Self> {
        Arc::new(ZoneServer {
            zone_id,
            port,
            proxy_uri,
            version,
            region: Mutex::new(region),
            entities: Mutex::new(HashMap::new()),
            proxy_tx: Mutex::new(None),
            mob_counter: Mutex::new(0),
            capital: mmo::world::capital(),
            nodes: Mutex::new(HashMap::new()),
            authored_mobs: Mutex::new(HashMap::new()),
            storage_points: Mutex::new(Vec::new()),
            build_boards: Mutex::new(Vec::new()),
            plots: Mutex::new(Vec::new()),
            home_structures: Mutex::new(Vec::new()),
            npcs: Mutex::new(Vec::new()),
        })
    }

    /// (Re)spawn the authored resource nodes that fall inside this zone's current
    /// region. Replaces any existing node set, so a split re-derives the nodes it
    /// now owns. Mirrors `spawn_mobs` but driven by authored world data.
    fn spawn_nodes(&self) {
        let r = *self.region.lock().unwrap();
        let spawns = self
            .capital
            .resource_nodes_in(mmo::world::Rect::new(r.x0, r.y0, r.x1, r.y1));
        let mut nodes = self.nodes.lock().unwrap();
        nodes.clear();
        for s in spawns {
            nodes.insert(
                s.id.to_string(),
                ResourceNode {
                    id: s.id.to_string(),
                    item_id: s.item_id.to_string(),
                    x: s.x,
                    y: s.y,
                    qty: s.qty,
                    max_qty: s.qty,
                    respawn_timer: 0,
                },
            );
        }
    }

    /// (Re)spawn the authored creatures inside this zone's current region (#158).
    /// Replaces the set, so a split re-derives what this zone now owns — exactly
    /// like `spawn_nodes`, and for the same reason: authored content is anchored
    /// to the world, not to whichever zone happens to be serving it.
    fn spawn_authored_mobs(&self) {
        let r = *self.region.lock().unwrap();
        let spawns = self
            .capital
            .mobs_in(mmo::world::Rect::new(r.x0, r.y0, r.x1, r.y1));
        let mut authored = self.authored_mobs.lock().unwrap();
        let mut entities = self.entities.lock().unwrap();
        // Drop any authored creature this zone no longer owns, so a split
        // doesn't leave a ghost behind in the half that lost it.
        for id in authored.keys() {
            entities.remove(id);
        }
        authored.clear();
        for m in spawns {
            authored.insert(
                m.id.to_string(),
                AuthoredMob { species: m.species, x: m.x, y: m.y, respawn_timer: 0 },
            );
            entities.insert(m.id.to_string(), Entity::authored_mob(m.species, m.x, m.y));
        }
    }

    /// (Re)spawn the authored storage points inside this zone's current region.
    fn spawn_storage_points(&self) {
        let r = *self.region.lock().unwrap();
        let pts = self
            .capital
            .storage_points_in(mmo::world::Rect::new(r.x0, r.y0, r.x1, r.y1));
        *self.storage_points.lock().unwrap() = pts;
    }

    /// (Re)spawn the authored build-order boards inside this zone's current region.
    fn spawn_build_boards(&self) {
        let r = *self.region.lock().unwrap();
        let boards = self
            .capital
            .build_boards_in(mmo::world::Rect::new(r.x0, r.y0, r.x1, r.y1));
        *self.build_boards.lock().unwrap() = boards;
    }

    /// (Re)cache the authored plot cells inside this zone's current region —
    /// geometry only, so the zone can gate home-structure placement/crafting to
    /// "standing on some plot" without knowing (or needing to know) who owns it.
    fn spawn_plots(&self) {
        let r = *self.region.lock().unwrap();
        let cells: Vec<mmo::world::PlotCell> = self
            .capital
            .plots_in(mmo::world::Rect::new(r.x0, r.y0, r.x1, r.y1))
            .into_iter()
            .map(|(_, cell)| cell)
            .collect();
        *self.plots.lock().unwrap() = cells;
    }

    /// Whether `(px, py)` falls inside any authored plot cell in this zone.
    fn on_a_plot(&self, px: i32, py: i32) -> bool {
        self.plots.lock().unwrap().iter().any(|c| c.rect().contains(px, py))
    }

    /// (Re)cache the authored NPCs inside this zone's current region.
    fn spawn_npcs(&self) {
        let r = *self.region.lock().unwrap();
        let npcs = self.capital.npcs_in(mmo::world::Rect::new(r.x0, r.y0, r.x1, r.y1));
        *self.npcs.lock().unwrap() = npcs;
    }

    /// Push the current state of every node, storage point, build board, and
    /// NPC to the gateway (which broadcasts it), so a just-joined client
    /// renders them.
    fn send_all_nodes(&self) {
        if let Some(tx) = self.proxy_tx.lock().unwrap().clone() {
            for n in self.nodes.lock().unwrap().values() {
                let _ = tx.send(Message::Text(node_status_json(n).to_string()));
            }
            for s in self.storage_points.lock().unwrap().iter() {
                let _ = tx.send(Message::Text(storage_status_json(s).to_string()));
            }
            for b in self.build_boards.lock().unwrap().iter() {
                let _ = tx.send(Message::Text(build_board_status_json(b).to_string()));
            }
            for n in self.npcs.lock().unwrap().iter() {
                let _ = tx.send(Message::Text(npc_status_json(n).to_string()));
            }
        }
    }

    /// Whether `(px, py)` is within range of any storage point in this zone, **or**
    /// a placed home storage chest — a home chest reuses the town storehouse's
    /// deposit/withdraw messages, so "at your chest" is as valid as "at the
    /// storehouse" (#12, tightened to per-structure proximity in #13).
    fn near_storage(&self, px: i32, py: i32) -> bool {
        self.storage_points
            .lock()
            .unwrap()
            .iter()
            .any(|s| dist2(px, py, s.x, s.y) <= (STORAGE_RANGE as i64).pow(2))
            || self.near_home_structure("storage", px, py)
    }

    /// Whether `(px, py)` is within range of a placed home structure of `kind`
    /// (`bed`/`storage`/`crafting`). The gateway pushes these as they're placed
    /// and on registration/split (`home_structures_sync`/`home_structure_added`)
    /// since it alone has DB access to know where they are (#13).
    fn near_home_structure(&self, kind: &str, px: i32, py: i32) -> bool {
        self.home_structures
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.kind == kind && dist2(px, py, s.x, s.y) <= (HOME_STRUCTURE_RANGE as i64).pow(2))
    }

    /// Whether this zone sits in a `safe` capital district (by its region centre,
    /// against the authored world). Safe zones disable mob aggression and any
    /// player damage; regions outside the authored capital default to wilds.
    /// Recomputed from the current region, so a split that moves the zone updates
    /// its safety automatically.
    fn is_safe(&self) -> bool {
        let r = *self.region.lock().unwrap();
        self.capital
            .district_for_region(mmo::world::Rect::new(r.x0, r.y0, r.x1, r.y1))
            .map(|d| d.safety == mmo::world::Safety::Safe)
            .unwrap_or(false)
    }

    /// Store a gateway-computed `env_state` verdict (#87/#88) on the live
    /// entity; the tick consumes it. A message for an entity not (or no
    /// longer) here is a silent no-op — the gateway re-pushes every second,
    /// so whichever zone actually owns the player converges on the next tick.
    fn apply_env_state(&self, player_id: &str, submerged: bool, poison_sources: i32) {
        if let Some(e) = self.entities.lock().unwrap().get_mut(player_id) {
            e.submerged = submerged;
            e.poison_sources = poison_sources;
        }
    }

    /// Store the gateway's verdict on what this player's swing is worth (#160).
    /// Mirrors [`ZoneServer::apply_env_state`] exactly: the gateway owns the
    /// equipment, this zone owns the hit resolution, and the two meet here.
    fn apply_loadout(&self, player_id: &str, melee_damage: i64) {
        if let Some(e) = self.entities.lock().unwrap().get_mut(player_id) {
            e.melee_damage = melee_damage.max(0);
        }
    }

    /// Report our current entity count to the proxy (feeds the admin count).
    fn send_zone_stats(&self) {
        if let Some(tx) = self.proxy_tx.lock().unwrap().clone() {
            let count = self
                .entities
                .lock()
                .unwrap()
                .values()
                .filter(|e| e.kind == EntityKind::Player)
                .count();
            let _ = tx.send(Message::Text(
                json!({"type": "zone_stats", "count": count}).to_string(),
            ));
        }
    }

    /// Ask the gateway to hand a player to whoever owns world position (x, y).
    fn send_migrate_request(&self, id: &str, x: i32, y: i32, hp: i32) {
        if let Some(tx) = self.proxy_tx.lock().unwrap().clone() {
            let _ = tx.send(Message::Text(
                json!({
                    "type": "migrate_request",
                    "player_id": id,
                    "from": self.zone_id,
                    "x": x, "y": y, "hp": hp,
                })
                .to_string(),
            ));
            println!("[Zone {}] Migrate request: {id} left region at ({x}, {y})", self.zone_id);
        }
    }

    /// Push an ability's client-facing outcome. `ok=false` carries a
    /// `reason` the hotbar can toast (#119, #117); `cooldown_ms` rides
    /// along either way so the client's sweep always matches the gateway's
    /// ledger, which already started it before this was even called. A
    /// success also carries what was actually swung out of the node
    /// (`item_id`/`qty`, #125) so the client can flash an ordinary "+N"
    /// gain notice — a gap the old channel's now-deleted `gather.result`
    /// used to cover and ability swings never did.
    fn send_ability_result(&self, pid: &str, ability_id: &str, ok: bool, reason: &str, cooldown_ms: i64, yield_item: Option<(&str, i64)>) {
        if let Some(tx) = self.proxy_tx.lock().unwrap().clone() {
            let mut v = json!({
                "type": "ability.result", "player_id": pid,
                "id": ability_id, "ok": ok, "cooldown_ms": cooldown_ms,
            });
            if !ok {
                v["reason"] = json!(reason);
            }
            if let Some((item_id, qty)) = yield_item {
                v["item_id"] = json!(item_id);
                v["qty"] = json!(qty);
            }
            let _ = tx.send(Message::Text(v.to_string()));
        }
    }

    /// Resolve a gateway-approved ability swing against this zone's live
    /// state (mining/abilities epic #123, #117; generalized to any
    /// harvesting ability in #125): the gateway already checked the
    /// wielder's tool and cooldown; range, stock, and whether the node is
    /// even the right kind for this ability are the zone's call, since it
    /// alone knows live positions and node state. A successful swing
    /// yields through the same internal `gather_yield` persistence path
    /// every ability already shares — instantly, with no channel.
    fn apply_ability_swing(&self, pid: &str, ability_id: &str, node_id: &str, cooldown_ms: i64) {
        let Some(target_item) = mmo::world::ability_target_item(ability_id) else { return };
        let outcome = {
            let entities = self.entities.lock().unwrap();
            let mut nodes = self.nodes.lock().unwrap();
            let Some(p) = entities.get(pid) else { return };
            match nodes.get_mut(node_id) {
                Some(node) if node.item_id == target_item && node.qty > 0 => {
                    if dist2(p.x, p.y, node.x, node.y) > (SWING_RANGE as i64).pow(2) {
                        Err("out_of_range")
                    } else {
                        node.qty -= 1;
                        let depleted = node.qty <= 0;
                        if depleted {
                            node.respawn_timer = NODE_RESPAWN_TICKS;
                        }
                        Ok((node.item_id.clone(), depleted))
                    }
                }
                _ => Err("exhausted"),
            }
        };
        match outcome {
            Ok((item_id, depleted)) => {
                let xp = mmo::world::ability_xp_per_swing(ability_id);
                self.send_ability_result(pid, ability_id, true, "", cooldown_ms, Some((&item_id, 1)));
                if let Some(tx) = self.proxy_tx.lock().unwrap().clone() {
                    let skill = mmo::world::governing_skill(ability_id).unwrap_or("mining");
                    let _ = tx.send(Message::Text(json!({
                        "type": "gather_yield", "player_id": pid,
                        "item_id": item_id, "qty": 1, "skill": skill, "xp": xp,
                        // Tells the gateway which ability swung — it wears down
                        // whatever tool governs it (#128). Only ever present for
                        // a real swing; nothing else emits gather_yield anymore.
                        "ability_id": ability_id,
                    }).to_string()));
                    let touch = if depleted {
                        json!({"type": "despawn", "player_id": node_id})
                    } else {
                        match self.nodes.lock().unwrap().get(node_id) {
                            Some(node) => node_status_json(node),
                            None => return,
                        }
                    };
                    let _ = tx.send(Message::Text(touch.to_string()));
                }
            }
            Err(reason) => self.send_ability_result(pid, ability_id, false, reason, cooldown_ms, None),
        }
    }

    /// Validate proximity to `npc_id` (mining/abilities epic #123, #118) and,
    /// if close enough, forward an internal `npc_interact` to the gateway —
    /// the only party that knows inventory/equipment, which is what decides
    /// what the NPC actually says (and whether it hands over a pickaxe).
    /// Too far, or an unknown NPC/player: silent no-op, same convention as
    /// every other proximity-gated action in this file.
    fn apply_npc_talk(&self, pid: &str, npc_id: &str) {
        let in_range = {
            let entities = self.entities.lock().unwrap();
            let npcs = self.npcs.lock().unwrap();
            match (entities.get(pid), npcs.iter().find(|n| n.id == npc_id)) {
                (Some(p), Some(n)) => dist2(p.x, p.y, n.x, n.y) <= (NPC_TALK_RANGE as i64).pow(2),
                _ => false,
            }
        };
        if in_range {
            if let Some(tx) = self.proxy_tx.lock().unwrap().clone() {
                let _ = tx.send(Message::Text(json!({
                    "type": "npc_interact", "player_id": pid, "npc_id": npc_id,
                }).to_string()));
            }
        }
    }

    async fn handle_proxy(self: Arc<Self>, raw: TcpStream) {
        let ws = match tokio_tungstenite::accept_async(raw).await {
            Ok(ws) => ws,
            Err(e) => {
                println!("[Zone {}] Proxy handshake error: {e}", self.zone_id);
                return;
            }
        };
        println!("[Zone {}] Proxy connected", self.zone_id);

        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        *self.proxy_tx.lock().unwrap() = Some(tx);

        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
        });

        while let Some(Ok(msg)) = stream.next().await {
            let text = match msg {
                Message::Text(t) => t,
                Message::Close(_) => break,
                _ => continue,
            };
            let data: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let msg_type = data.get("type").and_then(|v| v.as_str()).unwrap_or("");

            // Rolling update: the gateway has drained us onto a new instance.
            if msg_type == "shutdown" {
                println!("[Zone {}] Shutdown requested by gateway, exiting", self.zone_id);
                std::process::exit(0);
            }

            // Auto-scaling: the gateway shrank our region (we were split).
            if msg_type == "set_region" {
                let r = Region {
                    x0: data.get("x0").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
                    y0: data.get("y0").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
                    x1: data.get("x1").and_then(|v| v.as_i64()).unwrap_or(WORLD_SIZE as i64) as i32,
                    y1: data.get("y1").and_then(|v| v.as_i64()).unwrap_or(WORLD_SIZE as i64) as i32,
                };
                *self.region.lock().unwrap() = r;
                println!(
                    "[Zone {}] Region set to ({},{})-({},{})",
                    self.zone_id, r.x0, r.y0, r.x1, r.y1
                );
                // A freshly-split zone gets its own mobs, nodes, storage points,
                // build boards, and plots. Home structures are cleared here (rather
                // than re-derived locally, since they live in the gateway's DB, not
                // static world authoring) and repopulated by the `home_structures_sync`
                // the gateway sends right after a region change (#13).
                self.spawn_mobs(MOBS_PER_ZONE);
                self.spawn_authored_mobs();
                self.spawn_nodes();
                self.spawn_storage_points();
                self.spawn_build_boards();
                self.spawn_plots();
                self.spawn_npcs();
                *self.home_structures.lock().unwrap() = Vec::new();
                continue;
            }

            // The gateway telling us which home structures (bed/storage/crafting)
            // sit in our region — either a full replace (registration/split) or one
            // newly placed structure to add (#13).
            if msg_type == "home_structures_sync" {
                let structures = data.get("structures").and_then(|v| v.as_array()).cloned().unwrap_or_default();
                let parsed: Vec<HomeStructureRef> = structures
                    .iter()
                    .filter_map(|s| {
                        Some(HomeStructureRef {
                            id: s.get("id")?.as_str()?.to_string(),
                            kind: s.get("kind")?.as_str()?.to_string(),
                            x: s.get("x")?.as_i64()? as i32,
                            y: s.get("y")?.as_i64()? as i32,
                        })
                    })
                    .collect();
                *self.home_structures.lock().unwrap() = parsed;
                continue;
            }
            if msg_type == "home_structure_added" {
                if let (Some(id), Some(kind), Some(x), Some(y)) = (
                    data.get("id").and_then(|v| v.as_str()),
                    data.get("kind").and_then(|v| v.as_str()),
                    data.get("x").and_then(|v| v.as_i64()),
                    data.get("y").and_then(|v| v.as_i64()),
                ) {
                    let mut hs = self.home_structures.lock().unwrap();
                    hs.retain(|s| s.id != id); // upsert: replace if already known
                    hs.push(HomeStructureRef {
                        id: id.to_string(),
                        kind: kind.to_string(),
                        x: x as i32,
                        y: y as i32,
                    });
                }
                continue;
            }
            // A home structure was demolished (a rent reclaim, #14) — drop it so
            // it stops gating deposit/withdraw/craft proximity.
            if msg_type == "home_structure_removed" {
                if let Some(id) = data.get("id").and_then(|v| v.as_str()) {
                    self.home_structures.lock().unwrap().retain(|s| s.id != id);
                }
                continue;
            }

            let player_id = match data.get("player_id").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => continue,
            };

            match msg_type {
                "player_join" => {
                    let (x, y) = self.region.lock().unwrap().random_point();
                    self.entities.lock().unwrap().insert(player_id.clone(), Entity::player(x, y, PLAYER_MAX_HP));
                    println!("[Zone {}] Player joined: {player_id} at ({x},{y})", self.zone_id);
                    let ids: Vec<String> = self.entities.lock().unwrap().keys().cloned().collect();
                    for id in ids {
                        self.send_status_update(&id).await;
                    }
                    self.send_all_nodes(); // so the joiner renders gatherable nodes
                    self.send_zone_stats();
                }
                "player_leave" => {
                    self.entities.lock().unwrap().remove(&player_id);
                    println!("[Zone {}] Player left: {player_id}", self.zone_id);
                    self.send_zone_stats();
                }
                "move" => {
                    let dx = data.get("dx").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                    let dy = data.get("dy").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                    let moved = {
                        let mut entities = self.entities.lock().unwrap();
                        if let Some(e) = entities.get_mut(&player_id) {
                            let (nx, ny) = clamp_world(e.x + dx, e.y + dy);
                            e.x = nx;
                            e.y = ny;
                            if dx != 0 || dy != 0 {
                                e.facing = (dx.signum(), dy.signum());
                            }
                            Some((nx, ny, e.hp))
                        } else {
                            None
                        }
                    };
                    if let Some((nx, ny, hp)) = moved {
                        if self.region.lock().unwrap().contains(nx, ny) {
                            self.send_status_update(&player_id).await;
                        } else {
                            // Left our slice of the world: hand off to the gateway.
                            self.entities.lock().unwrap().remove(&player_id);
                            self.send_migrate_request(&player_id, nx, ny, hp);
                            self.send_zone_stats();
                        }
                    }
                }
                "spawn_entity" => {
                    // A player entered our region (persistent login/register, or a
                    // migration from a neighbouring zone).
                    let x = data.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                    let y = data.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                    let hp = data.get("hp").and_then(|v| v.as_i64()).unwrap_or(100) as i32;
                    let (x, y) = clamp_world(x, y);
                    self.entities.lock().unwrap().insert(player_id.clone(), Entity::player(x, y, hp));
                    println!("[Zone {}] Received player {player_id} at ({x}, {y})", self.zone_id);
                    self.send_status_update(&player_id).await;
                    // Persistent players spawn this way (not via player_join), so they
                    // must also be sent the gatherable nodes, storage points, and build
                    // boards — otherwise a logged-in player sees no resources to gather.
                    self.send_all_nodes();
                    self.send_zone_stats();
                }
                "env_state" => {
                    // Per-player environment flags, computed gateway-side (#87)
                    // — the gateway owns terrain and object positions; this
                    // zone just stores the verdict for the tick to consume.
                    // Pushed ~1/s for every player unconditionally, so a stale
                    // flag on a recreated entity self-heals.
                    let submerged = data.get("submerged").and_then(|v| v.as_bool()).unwrap_or(false);
                    let poison_sources = data.get("poison_sources").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                    self.apply_env_state(&player_id, submerged, poison_sources);
                }
                "loadout" => {
                    // What this player is holding, computed gateway-side (#160)
                    // — same contract as `env_state` above. Pushed on every
                    // equipment change for immediacy AND on the periodic sweep,
                    // so a migrated or respawned entity self-heals rather than
                    // staying stuck at whatever it was recreated with.
                    let dmg = data
                        .get("melee_damage")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(mmo::world::MELEE_DAMAGE_BARE);
                    self.apply_loadout(&player_id, dmg);
                }
                "attack" => {
                    // Flag the swing; damage is resolved authoritatively in the tick.
                    let dx = data.get("dx").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                    let dy = data.get("dy").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                    let mut entities = self.entities.lock().unwrap();
                    if let Some(e) = entities.get_mut(&player_id) {
                        if e.kind == EntityKind::Player && e.attack_cooldown == 0 {
                            if dx != 0 || dy != 0 {
                                e.facing = (dx.signum(), dy.signum());
                            }
                            e.swinging = true;
                        }
                    }
                }
                "ability_swing" => {
                    // Internal: the gateway already confirmed the tool and the
                    // cooldown (#117) — only range/stock/target-item are ours
                    // to know, since only the zone has live positions and node
                    // state.
                    let ability_id = data.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let node_id = data.get("node_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let cooldown_ms = data.get("cooldown_ms").and_then(|v| v.as_i64()).unwrap_or(0);
                    self.apply_ability_swing(&player_id, &ability_id, &node_id, cooldown_ms);
                }
                "npc.talk" => {
                    let npc_id = data.get("npc_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    self.apply_npc_talk(&player_id, &npc_id);
                }
                "store.deposit" | "store.withdraw" => {
                    // Validate the player is at a storage point; the gateway performs
                    // the durable inventory<->storage transfer and pushes the result.
                    let item_id = data.get("item_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let qty = data.get("qty").and_then(|v| v.as_i64()).unwrap_or(0);
                    let at_storage = {
                        let entities = self.entities.lock().unwrap();
                        entities.get(&player_id).map(|p| self.near_storage(p.x, p.y)).unwrap_or(false)
                    };
                    if at_storage && qty > 0 && !item_id.is_empty() {
                        let op = if msg_type == "store.deposit" { "deposit" } else { "withdraw" };
                        if let Some(tx) = self.proxy_tx.lock().unwrap().clone() {
                            let _ = tx.send(Message::Text(json!({
                                "type": "store_op", "player_id": player_id,
                                "op": op, "item_id": item_id, "qty": qty,
                            }).to_string()));
                        }
                    }
                }
                "build.contribute" => {
                    // Proximity (a build board, or the order's own placement — e.g. a
                    // mayor-commissioned dirt path far from any board) is validated by
                    // the gateway (city authority), which knows every order's location
                    // and keeps a live position cache; the zone only checks the request
                    // is well-formed before forwarding.
                    let order_id = data.get("order_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let item_id = data.get("item_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let qty = data.get("qty").and_then(|v| v.as_i64()).unwrap_or(0);
                    if qty > 0 && !order_id.is_empty() && !item_id.is_empty() {
                        if let Some(tx) = self.proxy_tx.lock().unwrap().clone() {
                            let _ = tx.send(Message::Text(json!({
                                "type": "build_contribute", "player_id": player_id,
                                "order_id": order_id, "item_id": item_id, "qty": qty,
                            }).to_string()));
                        }
                    }
                }
                "build.place" => {
                    // Geometry-only gate: is the *target* point on some plot? Ownership,
                    // footprint bounds/overlap, and the durable write are the gateway's
                    // job (it alone knows whose plot this is) — see #12.
                    let kind = data.get("kind").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let x = data.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                    let y = data.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                    let rot = data.get("rot").and_then(|v| v.as_i64()).unwrap_or(0);
                    if !kind.is_empty() && self.on_a_plot(x, y) {
                        if let Some(tx) = self.proxy_tx.lock().unwrap().clone() {
                            let _ = tx.send(Message::Text(json!({
                                "type": "build_place", "player_id": player_id,
                                "kind": kind, "x": x, "y": y, "rot": rot,
                            }).to_string()));
                        }
                    }
                }
                "craft.make" => {
                    // Proximity gate: is the player near *a* crafting station? Whose
                    // plot it's on, and the actual craft, are the gateway's job (#13).
                    let recipe_id = data.get("recipe_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let near_station = {
                        let entities = self.entities.lock().unwrap();
                        entities.get(&player_id)
                            .map(|p| self.near_home_structure("crafting", p.x, p.y))
                            .unwrap_or(false)
                    };
                    if near_station && !recipe_id.is_empty() {
                        if let Some(tx) = self.proxy_tx.lock().unwrap().clone() {
                            let _ = tx.send(Message::Text(json!({
                                "type": "craft_make", "player_id": player_id,
                                "recipe_id": recipe_id,
                            }).to_string()));
                        }
                    }
                }
                _ => {}
            }
        }

        println!("[Zone {}] Proxy disconnected", self.zone_id);
        *self.proxy_tx.lock().unwrap() = None;
    }

    /// Insert `count` mobs at random points inside the current region.
    fn spawn_mobs(&self, count: usize) {
        let region = *self.region.lock().unwrap();
        let mut entities = self.entities.lock().unwrap();
        let mut counter = self.mob_counter.lock().unwrap();
        for _ in 0..count {
            let (x, y) = region.random_point();
            let id = format!("mob_{}_{}", self.zone_id, *counter);
            *counter += 1;
            entities.insert(id, Entity::mob(x, y));
        }
    }

    /// Authoritative fixed-rate simulation: mob AI, melee resolution, deaths,
    /// respawns, and mob top-up. Mirrors the Python `_game_loop`. No `.await`
    /// happens while the entities lock is held.
    async fn game_loop(self: Arc<Self>) {
        let mut interval = tokio::time::interval(Duration::from_millis(TICK_MS));

        // Persistent simulation state across ticks (this task owns the zone).
        let mut respawn_timer: i32 = MOB_RESPAWN_TICKS;
        // Territory control state for this zone.
        let mut owner: Option<String> = None;
        let mut capturing: Option<String> = None;
        let mut progress: f32 = 0.0;
        let mut last_reported: Option<(Option<String>, i32)> = None;

        loop {
            interval.tick().await;

            let tx = match self.proxy_tx.lock().unwrap().clone() {
                Some(t) => t,
                None => continue, // no proxy connected yet
            };
            let region = *self.region.lock().unwrap();
            // Safe capital districts disable mob aggression, player damage, and the
            // territory-capture (wilds) mechanic. Re-evaluated each tick so a split
            // that moves this zone is honored immediately.
            let safe = self.is_safe();

            let mut rng = rand::thread_rng();
            let mut changed: HashSet<String> = HashSet::new();
            let mut despawns: Vec<String> = Vec::new();
            let mut died: Vec<(String, i32)> = Vec::new(); // (player_id, respawn max_hp)
            // (killer, item, species) for creatures that died with a killer this
            // tick (#159). Collected while the entity lock is held and flushed
            // with the rest of the packets below, like every other outcome.
            let mut loot: Vec<(String, &'static str, &'static str)> = Vec::new();
            // Players whose swing connected this tick (#160); the gateway owns
            // durability, so it does the wearing.
            let mut weapon_used: Vec<String> = Vec::new();
            let mut packets: Vec<String> = Vec::new();

            {
                let mut entities = self.entities.lock().unwrap();

                // Tick down cooldowns.
                for e in entities.values_mut() {
                    if e.attack_cooldown > 0 {
                        e.attack_cooldown -= 1;
                    }
                }

                // Snapshots so we can read positions while mutating individuals.
                let players: Vec<(String, i32, i32)> = entities
                    .iter()
                    .filter(|(_, e)| e.kind == EntityKind::Player)
                    .map(|(id, e)| (id.clone(), e.x, e.y))
                    .collect();
                let mob_ids: Vec<String> = entities
                    .iter()
                    .filter(|(_, e)| e.kind == EntityKind::Mob)
                    .map(|(id, _)| id.clone())
                    .collect();

                // --- 1. Mob AI: move and accumulate contact damage to players. ---
                let mut player_damage: HashMap<String, i32> = HashMap::new();
                let aggro2 = (AGGRO_RADIUS as i64) * (AGGRO_RADIUS as i64);
                let atk2 = (MOB_ATTACK_RANGE as i64) * (MOB_ATTACK_RANGE as i64);
                for mid in &mob_ids {
                    let (mx, my, ready, authored) = {
                        let e = entities.get(mid).unwrap();
                        (e.x, e.y, e.attack_cooldown == 0, e.home.is_some())
                    };
                    // Nearest player within aggro range.
                    //
                    // A safe district makes AMBIENT mobs harmless — they only
                    // wander, friendly wildlife, and produce no contact damage.
                    // **Authored creatures are exempt (#159): wild dogs bite.**
                    // A pack you can stand in the middle of unharmed isn't
                    // something that "needs clearing", and a sword is a poor
                    // reward when nothing can hurt you.
                    //
                    // Their reach is bounded by geometry rather than by a second
                    // rule: a dog aggros within `AGGRO_RADIUS` of itself and is
                    // leashed to `AUTHORED_MOB_LEASH` of its home (#158), so its
                    // territory is exactly that sum and no larger. The pack is
                    // sited 806 from the town centre precisely so this can never
                    // reach spawn — see
                    // `the_pack_cannot_reach_the_town_centre`.
                    let mut best: Option<(String, i32, i32, i64)> = None;
                    if !safe || authored {
                        for (pid, px, py) in &players {
                            let d2 = dist2(mx, my, *px, *py);
                            if d2 <= aggro2 && best.as_ref().map_or(true, |b| d2 < b.3) {
                                best = Some((pid.clone(), *px, *py, d2));
                            }
                        }
                    }

                    let e = entities.get_mut(mid).unwrap();
                    // Leash first, ahead of both chasing and wandering (#158): an
                    // authored creature dragged away from its ground gives up and
                    // goes home. Ahead of the chase on purpose — otherwise a
                    // player could kite the pack across the district and strand it
                    // somewhere the siting guarantees don't hold.
                    let straying = e.home.filter(|(hx, hy)| {
                        dist2(e.x, e.y, *hx, *hy) > (mmo::world::AUTHORED_MOB_LEASH as i64).pow(2)
                    });
                    if let Some((hx, hy)) = straying {
                        let (sx, sy) = step_toward(e.x, e.y, hx, hy, MOB_SPEED);
                        let (nx, ny) = clamp_region(&region, e.x + sx, e.y + sy);
                        e.x = nx;
                        e.y = ny;
                        if sx != 0 || sy != 0 {
                            e.facing = (sx.signum(), sy.signum());
                        }
                        changed.insert(mid.clone());
                        continue;
                    }
                    if let Some((pid, px, py, d2)) = best {
                        let (sx, sy) = step_toward(mx, my, px, py, MOB_SPEED);
                        let (nx, ny) = clamp_region(&region, e.x + sx, e.y + sy);
                        e.x = nx;
                        e.y = ny;
                        if sx != 0 || sy != 0 {
                            e.facing = (sx.signum(), sy.signum());
                        }
                        if d2 <= atk2 && ready {
                            e.attack_cooldown = MOB_ATTACK_COOLDOWN;
                            *player_damage.entry(pid).or_insert(0) += MOB_DAMAGE;
                        }
                    } else {
                        // Wander: occasionally re-roll heading, then drift.
                        if rng.gen_bool(0.05) || (e.wander.0 == 0 && e.wander.1 == 0) {
                            e.wander = (rng.gen_range(-1..=1), rng.gen_range(-1..=1));
                        }
                        let (nx, ny) = clamp_region(
                            &region,
                            e.x + e.wander.0 * MOB_WANDER_SPEED,
                            e.y + e.wander.1 * MOB_WANDER_SPEED,
                        );
                        e.x = nx;
                        e.y = ny;
                    }
                    changed.insert(mid.clone());
                }

                // Apply mob contact damage to players.
                //
                // No blanket safe-zone guard here any more (#159): TARGET
                // SELECTION above is the authority, and it is the precise place
                // to express the rule — an ambient mob in a safe district never
                // acquires a target, so it can never contribute damage, while an
                // authored creature can. Re-checking `safe` here would silently
                // undo the exception and leave the dogs toothless.
                for (pid, dmg) in player_damage {
                    if let Some(e) = entities.get_mut(&pid) {
                        e.hp -= dmg;
                        changed.insert(pid);
                    }
                }

                // --- 1b. Environmental vitals (#87): breath & suffocation. ---
                // Deliberately NOT gated on `safe`: the safe-hub guard above
                // exists to stop mob/PvP damage in the capital, but the river
                // must drown you anywhere — environmental hazards ARE the pen
                // around the starting area, and the whole capital is a safe
                // district. Death goes through the ordinary path below (#12):
                // hp hits 0, the gateway respawns at bed-or-spawn, and the
                // fresh entity starts with full breath.
                for (id, e) in entities.iter_mut().filter(|(_, e)| e.kind == EntityKind::Player) {
                    if e.submerged {
                        if e.breath > 0 {
                            e.breath -= 1;
                            e.drown_phase = 0;
                        } else {
                            // First suffocating tick hits immediately, then
                            // every DROWN_PERIOD_TICKS.
                            if e.drown_phase == 0 {
                                e.hp -= DROWN_DAMAGE;
                            }
                            e.drown_phase = (e.drown_phase + 1) % DROWN_PERIOD_TICKS;
                        }
                        changed.insert(id.clone());
                    } else if e.breath < BREATH_MAX_TICKS {
                        e.breath = (e.breath + BREATH_REFILL_PER_TICK).min(BREATH_MAX_TICKS);
                        e.drown_phase = 0;
                        changed.insert(id.clone());
                    }

                    // Poison (#88): buildup near trees, proc at the threshold,
                    // then an uncurable DoT until death. Buildup scales mildly
                    // with tree count (+1/tick per 2 extra trees), so the
                    // forest edge telegraphs while the interior kills fast.
                    if e.poisoned {
                        e.hp -= POISON_DAMAGE_PER_TICK;
                        changed.insert(id.clone());
                    } else if e.poison_sources > 0 {
                        e.poison_buildup = (e.poison_buildup
                            + POISON_BUILDUP_PER_TICK
                            + (e.poison_sources - 1) / 2)
                            .min(POISON_PROC_AT);
                        if e.poison_buildup >= POISON_PROC_AT {
                            e.poisoned = true;
                        }
                        changed.insert(id.clone());
                    } else if e.poison_buildup > 0 {
                        e.poison_buildup = (e.poison_buildup - POISON_DECAY_PER_TICK).max(0);
                        changed.insert(id.clone());
                    }
                }

                // --- 2. Resolve player melee swings against mobs in the arc. ---
                let swingers: Vec<String> = entities
                    .iter()
                    .filter(|(_, e)| e.kind == EntityKind::Player && e.swinging)
                    .map(|(id, _)| id.clone())
                    .collect();
                // Current mob positions after movement, for geometry.
                let mob_positions: Vec<(String, i32, i32)> = mob_ids
                    .iter()
                    .filter_map(|id| entities.get(id).map(|e| (id.clone(), e.x, e.y)))
                    .collect();
                for sid in &swingers {
                    let (px, py, fx, fy, dmg) = {
                        let e = entities.get(sid).unwrap();
                        (e.x, e.y, e.facing.0, e.facing.1, e.melee_damage as i32)
                    };
                    let hits: Vec<String> = mob_positions
                        .iter()
                        .filter(|(_, mx, my)| in_melee_arc(px, py, fx, fy, *mx, *my))
                        .map(|(id, _, _)| id.clone())
                        .collect();
                    let connected = !hits.is_empty();
                    for hid in hits {
                        if let Some(m) = entities.get_mut(&hid) {
                            let was_alive = m.hp > 0;
                            m.hp -= dmg;
                            // Credit the KILLING blow, and only it (#159). The
                            // `was_alive` guard is what makes this exactly-once
                            // when two swings land in the same tick: the second
                            // hits a corpse and changes nothing.
                            if was_alive && m.hp <= 0 {
                                m.killed_by = Some(sid.clone());
                            }
                            changed.insert(hid);
                        }
                    }
                    // A swing that CONNECTED wears the blade (#160) — once, not
                    // once per victim. A cleave through five dogs is one swing
                    // and one notch; charging five would make the arc a
                    // liability instead of a reward. A whiff costs nothing.
                    if connected {
                        weapon_used.push(sid.clone());
                    }
                    let e = entities.get_mut(sid).unwrap();
                    e.swinging = false;
                    e.attack_cooldown = PLAYER_ATTACK_COOLDOWN;
                    changed.insert(sid.clone());
                }

                // --- 3. Deaths. ---
                let dead_mobs: Vec<String> = entities
                    .iter()
                    .filter(|(_, e)| e.kind == EntityKind::Mob && e.hp <= 0)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in dead_mobs {
                    // The drop (#159), read off the corpse before it goes.
                    // Species is what gates it: only authored creatures carry
                    // one, so ambient mobs drop nothing and the bounty sends you
                    // to the pack rather than turning every zone into a farm.
                    //
                    // No killer means no drop, and that is the normal case for
                    // an environmental death — a dog that drowns or is poisoned
                    // was killed by the world, and nobody earned anything.
                    if let Some(e) = entities.get(&id) {
                        if let (Some(species), Some(killer)) = (e.species, e.killed_by.clone()) {
                            if let Some(item) = mmo::world::creature_drop(species) {
                                loot.push((killer, item, species));
                            }
                        }
                    }
                    entities.remove(&id);
                    changed.remove(&id);
                    // An authored creature (#158) is remembered rather than
                    // forgotten: it owes the world a comeback at the spot it was
                    // authored, so a pack stays a landmark instead of drifting
                    // into the RNG's hands after one clearing.
                    if let Some(a) = self.authored_mobs.lock().unwrap().get_mut(&id) {
                        a.respawn_timer = AUTHORED_MOB_RESPAWN_TICKS;
                    }
                    despawns.push(id);
                }
                let dead_players: Vec<String> = entities
                    .iter()
                    .filter(|(_, e)| e.kind == EntityKind::Player && e.hp <= 0)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in &dead_players {
                    // Where a player reappears (their bed, if set, else the default
                    // spawn) is the gateway's call — and that point may belong to a
                    // *different* zone, so this zone doesn't respawn them in place; it
                    // reports the death and hands off, mirroring "left our region"
                    // (#12). Removing them here (rather than leaving a stale entity)
                    // is safe even when the gateway ends up sending them right back to
                    // this same zone — `spawn_entity` inserts fresh either way.
                    //
                    // `player_died` is queued into `packets` (after `you_died`, see
                    // below) rather than sent immediately: the gateway routes
                    // player-addressed frames only while the client still points at
                    // this zone, and it relocates the client the moment it processes
                    // `player_died` — sending that first meant a death whose respawn
                    // point another zone owns silently dropped the client's
                    // `you_died` (found live in #88; drowning in an unsplit world
                    // never crossed zones, so #87's testing missed it).
                    let max_hp = entities.get(id).map(|e| e.max_hp).unwrap_or(PLAYER_MAX_HP);
                    entities.remove(id);
                    despawns.push(id.clone());
                    died.push((id.clone(), max_hp));
                }

                // --- 4a. Authored creatures come back where they were put (#158).
                {
                    let mut authored = self.authored_mobs.lock().unwrap();
                    for (id, a) in authored.iter_mut() {
                        if a.respawn_timer <= 0 {
                            continue;
                        }
                        a.respawn_timer -= 1;
                        if a.respawn_timer == 0 {
                            entities
                                .insert(id.clone(), Entity::authored_mob(a.species, a.x, a.y));
                            changed.insert(id.clone());
                        }
                    }
                }

                // --- 4b. Trickle AMBIENT mobs back (slowly, so a zone can be
                // cleared). Authored creatures are excluded from the count on
                // purpose: they are content, not territory, and letting a pack
                // of five suppress the ambient population would quietly change
                // how a zone behaves just because someone put dogs in it.
                let authored_ids: std::collections::HashSet<String> =
                    self.authored_mobs.lock().unwrap().keys().cloned().collect();
                let live_mobs = entities
                    .iter()
                    .filter(|(id, e)| {
                        e.kind == EntityKind::Mob && !authored_ids.contains(*id)
                    })
                    .count();
                if live_mobs < MOBS_PER_ZONE {
                    respawn_timer -= 1;
                    if respawn_timer <= 0 {
                        respawn_timer = MOB_RESPAWN_TICKS;
                        let mut counter = self.mob_counter.lock().unwrap();
                        let (x, y) = region.random_point();
                        let id = format!("mob_{}_{}", self.zone_id, *counter);
                        *counter += 1;
                        entities.insert(id.clone(), Entity::mob(x, y));
                        changed.insert(id);
                    }
                } else {
                    respawn_timer = MOB_RESPAWN_TICKS;
                }

                // --- 5. Capture bar: clear the mobs, then hold the ground. ---
                // Territory control is a wilds mechanic; the safe capital has no
                // capturable ground, so the bar never moves there.
                let present: Vec<String> = entities
                    .iter()
                    .filter(|(_, e)| e.kind == EntityKind::Player)
                    .map(|(id, _)| id.clone())
                    .collect();
                // A single present player can make progress; 0 or 2+ (contested) cannot.
                let claimant = if present.len() == 1 { Some(present[0].clone()) } else { None };
                let mobs_clear = live_mobs <= CAPTURE_MOB_THRESHOLD;

                match (&claimant, mobs_clear) {
                    _ if safe => {}
                    (Some(p), true) => {
                        if owner.as_ref() == Some(p) {
                            progress = 100.0; // reinforce a zone you already hold
                        } else if owner.is_none() {
                            // Claim neutral ground; a new claimant restarts the bar.
                            if capturing.as_ref() != Some(p) {
                                capturing = Some(p.clone());
                                progress = 0.0;
                            }
                            progress += CAPTURE_RATE;
                            if progress >= 100.0 {
                                progress = 100.0;
                                owner = Some(p.clone());
                            }
                        } else {
                            // Enemy eroding the current owner's hold.
                            progress -= CAPTURE_RATE;
                            if progress <= 0.0 {
                                progress = 0.0;
                                owner = None;
                                capturing = Some(p.clone());
                            }
                        }
                    }
                    _ => {
                        // Stalled (mobs defending, empty, or contested): unowned bars
                        // decay back to neutral; owned zones simply hold.
                        if owner.is_none() {
                            progress = (progress - CAPTURE_DECAY).max(0.0);
                            if progress == 0.0 {
                                capturing = None;
                            }
                        }
                    }
                }

                // --- 6. Build outbound entity packets while holding the lock.
                for id in &changed {
                    if let Some(e) = entities.get(id) {
                        packets.push(entity_status_json(id, e).to_string());
                    }
                }
            } // entities lock released here

            // --- 7. Node respawn (the old channel-gathering per-tick yield
            // loop that used to live here was removed in #125 — every
            // resource is ability-swing-gated now, and a swing is instant,
            // resolved synchronously in `apply_ability_swing`, not ticked).
            {
                let mut nodes = self.nodes.lock().unwrap();
                let mut touched: HashSet<String> = HashSet::new();
                for node in nodes.values_mut() {
                    if node.qty <= 0 && node.respawn_timer > 0 {
                        node.respawn_timer -= 1;
                        if node.respawn_timer == 0 {
                            node.qty = node.max_qty;
                            touched.insert(node.id.clone());
                        }
                    }
                }
                for id in &touched {
                    if let Some(n) = nodes.get(id) {
                        packets.push(node_status_json(n).to_string());
                    }
                }
            }

            for id in &despawns {
                packets.push(json!({"type": "despawn", "player_id": id}).to_string());
            }
            // Loot (#159) rides the existing `gather_yield` path rather than a
            // second, parallel one: it already carries an item from a zone event
            // into a player's durable inventory through the gateway, already
            // respects MAX_CARRY, and already refuses guests. A kill is a
            // different WAY to earn an item, not a different kind of item.
            //
            // No `ability_id`: that field is what wears a tool down, and a kill
            // is not a tool swing. Weapon wear is #160's business.
            for pid in &weapon_used {
                packets.push(
                    json!({"type": "weapon_used", "player_id": pid}).to_string(),
                );
            }
            for (killer, item, species) in &loot {
                packets.push(
                    json!({
                        "type": "gather_yield", "player_id": killer,
                        "item_id": item, "qty": 1, "skill": "", "xp": 0,
                        "source": "kill", "species": species,
                    })
                    .to_string(),
                );
            }
            // Order matters: `you_died` must reach the gateway while the dead
            // player's client still points at THIS zone (see the death block
            // above) — `player_died` triggers the relocate, so it goes last.
            for (id, _) in &died {
                packets.push(json!({"type": "you_died", "player_id": id}).to_string());
            }
            for (id, max_hp) in &died {
                packets.push(json!({ "type": "player_died", "player_id": id, "hp": max_hp }).to_string());
            }

            // Report capture state when ownership flips or the bar moves noticeably.
            let bucket = progress.round() as i32;
            let snapshot = (owner.clone(), bucket);
            let report = match &last_reported {
                None => true,
                Some((o, b)) => o != &owner || (b - bucket).abs() >= 5 || bucket == 0 || bucket == 100,
            };
            if report && last_reported.as_ref() != Some(&snapshot) {
                last_reported = Some(snapshot);
                packets.push(
                    json!({
                        "type": "zone_capture",
                        "owner": owner,
                        "progress": progress,
                    })
                    .to_string(),
                );
            }

            for p in packets {
                let _ = tx.send(Message::Text(p));
            }
        }
    }

    async fn register_with_proxy(self: Arc<Self>) {
        let proxy_uri = match &self.proxy_uri {
            Some(u) => u.clone(),
            None => {
                println!("[Zone {}] No proxy URI provided, skipping registration", self.zone_id);
                return;
            }
        };

        loop {
            match tokio_tungstenite::connect_async(&proxy_uri).await {
                Ok((ws, _)) => {
                    let (mut sink, mut stream) = ws.split();
                    let r = *self.region.lock().unwrap();
                    let reg = json!({
                        "type": "register_zone",
                        "zone_id": self.zone_id,
                        "uri": format!("ws://127.0.0.1:{}", self.port),
                        "version": self.version,
                        "exe": std::env::current_exe()
                            .ok()
                            .and_then(|p| p.to_str().map(String::from))
                            .unwrap_or_default(),
                        "x0": r.x0, "y0": r.y0, "x1": r.x1, "y1": r.y1,
                    });
                    if sink.send(Message::Text(reg.to_string())).await.is_err() {
                        println!("[Zone {}] Failed to send registration", self.zone_id);
                        sleep(Duration::from_secs(5)).await;
                        continue;
                    }
                    println!("[Zone {}] Registered with proxy at {proxy_uri}", self.zone_id);

                    let mut interval = tokio::time::interval(Duration::from_secs(30));
                    interval.tick().await;
                    loop {
                        tokio::select! {
                            _ = interval.tick() => {
                                if sink.send(Message::Ping(Vec::new())).await.is_err() {
                                    println!("[Zone {}] Proxy connection lost, re-registering", self.zone_id);
                                    break;
                                }
                            }
                            incoming = stream.next() => {
                                if matches!(incoming, None | Some(Err(_))) {
                                    println!("[Zone {}] Proxy connection lost, re-registering", self.zone_id);
                                    break;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    println!("[Zone {}] Failed to register with proxy: {e}", self.zone_id);
                    println!("[Zone {}] Will retry in 5 seconds...", self.zone_id);
                }
            }
            sleep(Duration::from_secs(5)).await;
        }
    }

    async fn send_status_update(&self, player_id: &str) {
        let tx = self.proxy_tx.lock().unwrap().clone();
        let tx = match tx {
            Some(tx) => tx,
            None => return,
        };
        let packet = {
            let entities = self.entities.lock().unwrap();
            match entities.get(player_id) {
                Some(e) => entity_status_json(player_id, e),
                None => return,
            }
        };
        let _ = tx.send(Message::Text(packet.to_string()));
    }

    async fn start(self: Arc<Self>) {
        let r = *self.region.lock().unwrap();
        println!(
            "[Zone {}] Starting on port {} (v{}) region ({},{})-({},{})",
            self.zone_id, self.port, self.version, r.x0, r.y0, r.x1, r.y1
        );

        if self.proxy_uri.is_some() {
            let me = self.clone();
            tokio::spawn(async move { me.register_with_proxy().await });
        }

        // Seed mobs, resource nodes, storage points, build boards, and plots, then
        // start the 20 Hz sim.
        self.spawn_mobs(MOBS_PER_ZONE);
        self.spawn_authored_mobs();
        self.spawn_nodes();
        self.spawn_storage_points();
        self.spawn_build_boards();
        self.spawn_plots();
        self.spawn_npcs();
        {
            let me = self.clone();
            tokio::spawn(async move { me.game_loop().await });
        }

        let listener = TcpListener::bind(("127.0.0.1", self.port))
            .await
            .expect("bind zone port");
        while let Ok((stream, _)) = listener.accept().await {
            let me = self.clone();
            tokio::spawn(async move { me.handle_proxy(stream).await });
        }
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    let mut positional: Vec<String> = Vec::new();
    let mut region = Region { x0: 0, y0: 0, x1: WORLD_SIZE, y1: WORLD_SIZE };
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--region" {
            let g = |k: usize| args.get(i + k).and_then(|v| v.parse::<i32>().ok());
            if let (Some(a), Some(b), Some(c), Some(d)) = (g(1), g(2), g(3), g(4)) {
                region = Region { x0: a, y0: b, x1: c, y1: d };
            }
            i += 5;
        } else {
            positional.push(args[i].clone());
            i += 1;
        }
    }
    let zone_id = positional.first().cloned().unwrap_or_else(|| "zone_default".to_string());
    let port: u16 = positional.get(1).and_then(|p| p.parse().ok()).unwrap_or(9001);
    let proxy_uri = positional.get(2).cloned();
    let version: u32 = env::var("ZONE_VERSION").ok().and_then(|v| v.parse().ok()).unwrap_or(1);

    let server = ZoneServer::new(zone_id, port, proxy_uri, region, version);
    server.start().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone_for_region(region: Region) -> Arc<ZoneServer> {
        ZoneServer::new("test".to_string(), 0, None, region, 1)
    }

    // --- the weapon slot (#160) ---------------------------------------------

    /// Put a lone dog in front of a hunter, standing back and facing it — see
    /// `killing_a_dog_credits_the_killer_with_a_pelt` for why standing on top of
    /// a target is flaky.
    fn stage_duel(zone: &Arc<ZoneServer>) -> (String, (i32, i32)) {
        zone.spawn_authored_mobs();
        let (id, home) = {
            let a = zone.authored_mobs.lock().unwrap();
            let (id, m) = a.iter().next().unwrap();
            (id.clone(), (m.x, m.y))
        };
        let mut es = zone.entities.lock().unwrap();
        let others: Vec<String> = es
            .iter()
            .filter(|(oid, e)| e.species.is_some() && *oid != &id)
            .map(|(oid, _)| oid.clone())
            .collect();
        for oid in others {
            es.remove(&oid);
        }
        es.insert("duellist".to_string(), Entity::player(home.0 - 30, home.1, PLAYER_MAX_HP));
        es.get_mut("duellist").unwrap().facing = (1, 0);
        (id, home)
    }

    /// An armed swing hits harder than a bare-handed one, resolved server-side.
    /// A sword that didn't change how combat resolves would be a cosmetic item.
    #[tokio::test]
    async fn an_armed_swing_hits_harder_than_a_bare_one() {
        // Bare-handed: the shipped default a fresh entity carries.
        let bare = zone_for_region(CIVIC);
        let (bare_id, _) = stage_duel(&bare);
        bare.entities.lock().unwrap().get_mut("duellist").unwrap().swinging = true;
        drive(bare.clone(), 3).await;
        let bare_hp = bare.entities.lock().unwrap().get(&bare_id).map(|e| e.hp).unwrap();

        // Armed: the gateway's verdict, applied by the zone.
        let armed = zone_for_region(CIVIC);
        let (armed_id, _) = stage_duel(&armed);
        {
            let mut es = armed.entities.lock().unwrap();
            let d = es.get_mut("duellist").unwrap();
            d.melee_damage = mmo::world::melee_damage(Some("sword"));
            d.swinging = true;
        }
        drive(armed.clone(), 3).await;
        let armed_hp = armed.entities.lock().unwrap().get(&armed_id).map(|e| e.hp);

        assert!(
            bare_hp > 0,
            "bare hands should NOT one-shot a dog — that's the whole point of the sword"
        );
        assert!(
            armed_hp.is_none() || armed_hp.unwrap() <= 0,
            "a sword should drop a wild dog in one clean swing, got {armed_hp:?}"
        );
    }

    /// Bare-handed combat is unchanged. It's the current combat, several tests
    /// predate weapons entirely, and a player between swords still has to be
    /// able to defend themselves.
    #[test]
    fn an_empty_weapon_slot_still_swings() {
        assert_eq!(mmo::world::melee_damage(None), mmo::world::MELEE_DAMAGE_BARE);
        assert_eq!(mmo::world::melee_damage(Some("pickaxe")), mmo::world::MELEE_DAMAGE_BARE,
            "a gathering tool is not a weapon");
        assert!(mmo::world::melee_damage(Some("sword")) > mmo::world::MELEE_DAMAGE_BARE);
        // A fresh entity swings bare-handed until the gateway says otherwise —
        // briefly punching is a far better failure than briefly swinging a sword
        // you no longer own.
        assert_eq!(Entity::player(0, 0, 100).melee_damage, mmo::world::MELEE_DAMAGE_BARE);
    }

    /// The gateway's `loadout` push is what arms a swing, and it reconverges a
    /// recreated entity — the same contract `env_state` has.
    #[tokio::test]
    async fn the_loadout_push_arms_and_disarms_a_swing() {
        let zone = zone_for_region(CIVIC);
        zone.entities
            .lock()
            .unwrap()
            .insert("p1".to_string(), Entity::player(12800, 12800, PLAYER_MAX_HP));

        zone.apply_loadout("p1", mmo::world::melee_damage(Some("sword")));
        assert_eq!(
            zone.entities.lock().unwrap().get("p1").unwrap().melee_damage,
            mmo::world::melee_damage(Some("sword"))
        );

        // Sword breaks: the gateway re-pushes, and the very next swing is worth
        // bare-handed damage again.
        zone.apply_loadout("p1", mmo::world::MELEE_DAMAGE_BARE);
        assert_eq!(
            zone.entities.lock().unwrap().get("p1").unwrap().melee_damage,
            mmo::world::MELEE_DAMAGE_BARE
        );
    }

    /// A connecting swing wears the blade ONCE, not once per victim. A cleave
    /// through five dogs is one swing and one notch; charging five would make
    /// the arc a liability instead of a reward.
    #[tokio::test]
    async fn a_cleave_wears_the_blade_once_not_once_per_victim() {
        let zone = zone_for_region(CIVIC);
        zone.spawn_authored_mobs();
        let home = {
            let a = zone.authored_mobs.lock().unwrap();
            let m = a.values().next().unwrap();
            (m.x, m.y)
        };
        // Line the dogs up east of the swinger, all inside the 60-unit reach and
        // the ±90° arc. Positioned EXPLICITLY rather than relying on the authored
        // layout: the anchor dog comes out of a HashMap, so which way the rest of
        // the pack lies relative to the swing varies run to run — and the pack is
        // wider than a swing reaches anyway, so "hits all five" was never true.
        {
            let mut es = zone.entities.lock().unwrap();
            let ids: Vec<String> = es
                .iter()
                .filter(|(_, e)| e.species.is_some())
                .map(|(id, _)| id.clone())
                .collect();
            assert!(ids.len() >= 3, "precondition: a pack to cleave");
            for (i, id) in ids.iter().take(3).enumerate() {
                let d = es.get_mut(id).unwrap();
                d.x = home.0 + 20 + (i as i32 * 12);
                d.y = home.1;
            }
            for id in ids.iter().skip(3) {
                es.remove(id);
            }
            es.insert("cleaver".to_string(), Entity::player(home.0, home.1, PLAYER_MAX_HP));
            let c = es.get_mut("cleaver").unwrap();
            c.facing = (1, 0);
            c.melee_damage = mmo::world::melee_damage(Some("sword"));
            c.swinging = true;
        }

        let packets = drive_until(zone.clone(), |p| p.contains("weapon_used")).await;
        let wears = packets.iter().filter(|p| p.contains("weapon_used")).count();
        assert_eq!(wears, 1, "one swing should be one notch, got {wears}: {packets:?}");
        // ...and it really did hit several of them.
        let killed = packets.iter().filter(|p| p.contains("dog_pelt")).count();
        assert_eq!(killed, 3, "one armed swing should have felled all three, got {killed}");
    }

    /// A whiff costs nothing. Wearing a blade on air would make missing doubly
    /// punishing and turn every fight into a durability calculation.
    #[tokio::test]
    async fn a_missed_swing_does_not_wear_the_blade() {
        let zone = zone_for_region(CIVIC);
        zone.spawn_authored_mobs();
        // Far from every dog, swinging at nothing.
        {
            let mut es = zone.entities.lock().unwrap();
            es.insert("whiffer".to_string(), Entity::player(18000, 18000, PLAYER_MAX_HP));
            let w = es.get_mut("whiffer").unwrap();
            w.facing = (1, 0);
            w.melee_damage = mmo::world::melee_damage(Some("sword"));
            w.swinging = true;
        }
        let packets = drive(zone.clone(), 6).await;
        assert!(
            !packets.iter().any(|p| p.contains("weapon_used")),
            "swinging at air wore the blade: {packets:?}"
        );
    }

    // --- authored creatures (wild dogs epic #157, issue #158) ---------------

    /// The authored pack turns up where it was authored, named, when a zone owns
    /// that ground. Before #158 every mob was an anonymous blob at a random
    /// point, so "is there a dog over there" had no answer.
    #[tokio::test]
    async fn a_zone_owning_the_pack_spawns_it_where_it_was_authored() {
        let zone = zone_for_region(CIVIC);
        zone.spawn_authored_mobs();

        let authored = mmo::world::capital().mobs;
        let entities = zone.entities.lock().unwrap();
        for m in &authored {
            let e = entities.get(m.id).unwrap_or_else(|| panic!("{} did not spawn", m.id));
            assert_eq!((e.x, e.y), (m.x, m.y), "{} spawned somewhere else", m.id);
            assert_eq!(e.species, Some(m.species), "{} lost its species", m.id);
            assert_eq!(e.kind, EntityKind::Mob);
            assert_eq!(e.hp, MOB_MAX_HP, "an authored creature fights like any other mob");
        }
    }

    /// A zone that owns none of the authored ground gets none of them — and
    /// still fills its ambient population exactly as before. Authored content
    /// must not become a prerequisite for a zone working.
    #[tokio::test]
    async fn a_zone_without_authored_ground_is_unchanged() {
        // A far corner of the world with nothing authored in it.
        let empty = Region { x0: 100, y0: 100, x1: 900, y1: 900 };
        let zone = zone_for_region(empty);
        zone.spawn_authored_mobs();
        assert!(zone.authored_mobs.lock().unwrap().is_empty());

        zone.spawn_mobs(MOBS_PER_ZONE);
        let entities = zone.entities.lock().unwrap();
        let mobs: Vec<_> = entities.values().filter(|e| e.kind == EntityKind::Mob).collect();
        assert_eq!(mobs.len(), MOBS_PER_ZONE, "the ambient population changed");
        assert!(
            mobs.iter().all(|e| e.species.is_none()),
            "ambient mobs should stay anonymous — species is what marks authored content"
        );
    }

    /// Re-deriving on a region change replaces the set rather than accumulating,
    /// and a zone that loses the ground loses the creatures on it. This is the
    /// bug a split would otherwise cause: the same dog alive in two zones.
    #[tokio::test]
    async fn a_region_change_re_derives_the_pack_without_ghosts() {
        let zone = zone_for_region(CIVIC);
        zone.spawn_authored_mobs();
        let owned = zone.authored_mobs.lock().unwrap().len();
        assert!(owned > 0, "precondition: the civic region owns the pack");

        // Running it twice must not double anything.
        zone.spawn_authored_mobs();
        assert_eq!(zone.authored_mobs.lock().unwrap().len(), owned, "re-derive accumulated");
        assert_eq!(
            zone.entities.lock().unwrap().values().filter(|e| e.species.is_some()).count(),
            owned,
            "re-derive left duplicate entities"
        );

        // Hand the ground away: the creatures go with it, leaving no ghost.
        *zone.region.lock().unwrap() = Region { x0: 100, y0: 100, x1: 900, y1: 900 };
        zone.spawn_authored_mobs();
        assert!(zone.authored_mobs.lock().unwrap().is_empty());
        assert_eq!(
            zone.entities.lock().unwrap().values().filter(|e| e.species.is_some()).count(),
            0,
            "an authored creature was left behind in a zone that no longer owns its ground"
        );
    }

    /// Species rides the wire, so a client can name and draw a dog as a dog.
    /// Absent for players and ambient mobs, which is the distinction it exists
    /// to make.
    #[test]
    fn species_rides_the_entity_wire_only_for_authored_creatures() {
        let dog = Entity::authored_mob(mmo::world::SPECIES_WILD_DOG, 1, 2);
        let v = entity_status_json("mob_dog_0", &dog);
        assert_eq!(v["state"]["species"], mmo::world::SPECIES_WILD_DOG);
        assert_eq!(v["state"]["type"], "mob");

        let ambient = entity_status_json("mob_x", &Entity::mob(1, 2));
        assert!(ambient["state"]["species"].is_null(), "ambient mobs are anonymous");
        let player = entity_status_json("p1", &Entity::player(1, 2, PLAYER_MAX_HP));
        assert!(player["state"]["species"].is_null(), "players have no species");
    }

    /// A killed dog comes back **where it was authored**, not at a random point,
    /// so a pack stays a landmark across a session rather than dispersing after
    /// one clearing. It also takes materially longer than the ambient trickle:
    /// clearing a pack should last a moment, and a bounty target that respawned
    /// under your feet would be farming rather than hunting.
    #[tokio::test]
    async fn a_killed_authored_creature_returns_to_its_authored_spot() {
        let zone = zone_for_region(CIVIC);
        zone.spawn_authored_mobs();
        let (id, home) = {
            let a = zone.authored_mobs.lock().unwrap();
            let (id, m) = a.iter().next().unwrap();
            (id.clone(), (m.x, m.y))
        };

        // Kill it, and shove the corpse's timer down so the test doesn't wait
        // 30 real seconds for the authored cadence.
        zone.entities.lock().unwrap().get_mut(&id).unwrap().hp = 0;
        drive(zone.clone(), 2).await;
        assert!(!zone.entities.lock().unwrap().contains_key(&id), "it should have died");
        assert!(
            zone.authored_mobs.lock().unwrap().get(&id).unwrap().respawn_timer > 0,
            "an authored creature owes the world a comeback"
        );
        zone.authored_mobs.lock().unwrap().get_mut(&id).unwrap().respawn_timer = 2;

        drive(zone.clone(), 4).await;
        let entities = zone.entities.lock().unwrap();
        let back = entities.get(&id).expect("it should have come back");
        // Near home, not exactly on it: a respawned mob starts wandering
        // immediately, which is correct. The property that matters is that it
        // came back to its authored ground rather than to a random point in a
        // 12800-wide region — so the tolerance is tiny next to the alternative.
        let drift = (((back.x - home.0) as f64).powi(2) + ((back.y - home.1) as f64).powi(2)).sqrt();
        assert!(
            drift < 100.0,
            "it came back {drift:.0} from home ({:?} vs {home:?}) — that looks like a random              respawn, not an authored one",
            (back.x, back.y)
        );
        assert_eq!(back.hp, MOB_MAX_HP, "and at full health");
        assert_eq!(back.species, Some(mmo::world::SPECIES_WILD_DOG));
    }

    /// Wild dogs bite, even though the whole capital is a safe district (#159).
    /// A pack you can stand in the middle of unharmed isn't something that
    /// "needs clearing", and a sword is a poor reward when nothing can hurt you.
    #[tokio::test]
    async fn wild_dogs_bite_inside_the_safe_capital() {
        let zone = zone_for_region(CIVIC);
        assert!(zone.is_safe(), "precondition: the capital is a safe district");
        zone.spawn_authored_mobs();
        let home = {
            let a = zone.authored_mobs.lock().unwrap();
            let m = a.values().next().unwrap();
            (m.x, m.y)
        };
        // Standing in the middle of the pack.
        zone.entities
            .lock()
            .unwrap()
            .insert("prey".to_string(), Entity::player(home.0, home.1, PLAYER_MAX_HP));

        drive_until(zone.clone(), |p| p.contains("\"prey\"") && p.contains("\"hp\"")).await;
        // Give the dogs a few attack cooldowns' worth of contact.
        drive(zone.clone(), (MOB_ATTACK_COOLDOWN * 4) as u32).await;

        let hp = zone.entities.lock().unwrap().get("prey").map(|e| e.hp).unwrap_or(0);
        assert!(
            hp < PLAYER_MAX_HP,
            "the pack never touched a player standing in the middle of it (hp {hp})"
        );
    }

    /// The exception is precisely scoped: ambient mobs stay harmless in a safe
    /// district exactly as before. Only authored creatures are exempt.
    #[tokio::test]
    async fn ambient_mobs_are_still_harmless_in_the_safe_capital() {
        let zone = zone_for_region(CIVIC);
        {
            let mut es = zone.entities.lock().unwrap();
            es.insert("bystander".to_string(), Entity::player(9000, 9000, PLAYER_MAX_HP));
            for i in 0..4 {
                es.insert(format!("mob_amb_{i}"), Entity::mob(9000, 9000));
            }
        }
        drive(zone.clone(), (MOB_ATTACK_COOLDOWN * 6) as u32).await;
        let hp = zone.entities.lock().unwrap().get("bystander").map(|e| e.hp).unwrap_or(0);
        assert_eq!(
            hp, PLAYER_MAX_HP,
            "an anonymous mob drew blood in the capital — the #159 exception leaked"
        );
    }

    // --- kill credit and the drop (#159) ------------------------------------

    /// Killing an authored creature credits the killer, and the loot rides the
    /// existing `gather_yield` path rather than a second parallel one.
    #[tokio::test]
    async fn killing_a_dog_credits_the_killer_with_a_pelt() {
        let zone = zone_for_region(CIVIC);
        zone.spawn_authored_mobs();
        let (id, home) = {
            let a = zone.authored_mobs.lock().unwrap();
            let (id, m) = a.iter().next().unwrap();
            (id.clone(), (m.x, m.y))
        };
        // A player right on top of it, and the dog one hit from death.
        {
            let mut es = zone.entities.lock().unwrap();
            // One dog in reach, for the same reason as above: a swing cleaves.
            let others: Vec<String> = es
                .iter()
                .filter(|(oid, e)| e.species.is_some() && *oid != &id)
                .map(|(oid, _)| oid.clone())
                .collect();
            for oid in others {
                es.remove(&oid);
            }
            // Stand BACK from the target, facing it. Standing on top of it is
            // flaky: the arc is ±90°, mobs wander up to 2 units on the tick the
            // swing resolves, and a dog that drifts behind you is a clean miss —
            // with `swinging` consumed, there is no second chance. 30 units back
            // is well inside the 60 reach and far enough that a 2-unit wander
            // can't flip the angle.
            es.insert(
                "hunter".to_string(),
                Entity::player(home.0 - 30, home.1, PLAYER_MAX_HP),
            );
            es.get_mut(&id).unwrap().hp = 1;
            es.get_mut("hunter").unwrap().facing = (1, 0);
        }
        zone.entities.lock().unwrap().get_mut("hunter").unwrap().swinging = true;

        let packets = drive_until(zone.clone(), |p| p.contains("dog_pelt")).await;
        let loot: Vec<&String> = packets
            .iter()
            .filter(|p| p.contains("\"gather_yield\"") && p.contains("dog_pelt"))
            .collect();
        assert_eq!(loot.len(), 1, "expected exactly one pelt, got {loot:?}");
        let v: serde_json::Value = serde_json::from_str(loot[0]).unwrap();
        assert_eq!(v["player_id"], "hunter", "the wrong player was credited");
        assert_eq!(v["qty"], 1);
        assert_eq!(v["source"], "kill");
        // No ability id: a kill is not a tool swing, so nothing wears down here.
        assert!(v["ability_id"].is_null(), "a kill must not wear a gathering tool");
    }

    /// Two hunters swinging in the same tick produce exactly ONE pelt, to
    /// whoever landed the fatal blow. A creature can't die twice, and the second
    /// swing hits a corpse.
    #[tokio::test]
    async fn two_hunters_in_one_tick_produce_exactly_one_pelt() {
        let zone = zone_for_region(CIVIC);
        zone.spawn_authored_mobs();
        let (id, home) = {
            let a = zone.authored_mobs.lock().unwrap();
            let (id, m) = a.iter().next().unwrap();
            (id.clone(), (m.x, m.y))
        };
        {
            let mut es = zone.entities.lock().unwrap();
            // Isolate the target. The pack is clustered inside ~65 units and a
            // swing reaches 60 in a ±90° arc, so leaving the others in place
            // means the swings cleave through several dogs at once — correct
            // behaviour, but it would make this test about arcs rather than
            // about credit.
            let others: Vec<String> = es
                .iter()
                .filter(|(oid, e)| e.species.is_some() && *oid != &id)
                .map(|(oid, _)| oid.clone())
                .collect();
            for oid in others {
                es.remove(&oid);
            }
            es.get_mut(&id).unwrap().hp = 1;
            // Back from the target and facing it, for the same reason as above.
            for who in ["h1", "h2"] {
                es.insert(who.to_string(), Entity::player(home.0 - 30, home.1, PLAYER_MAX_HP));
                es.get_mut(who).unwrap().facing = (1, 0);
            }
        }
        {
            let mut es = zone.entities.lock().unwrap();
            es.get_mut("h1").unwrap().swinging = true;
            es.get_mut("h2").unwrap().swinging = true;
        }

        let packets = drive_until(zone.clone(), |p| p.contains("dog_pelt")).await;
        let loot: Vec<&String> = packets
            .iter()
            .filter(|p| p.contains("\"gather_yield\"") && p.contains("dog_pelt"))
            .collect();
        assert_eq!(
            loot.len(),
            1,
            "a creature died once but paid out {} times: {loot:?}",
            loot.len()
        );
    }

    /// A creature killed by the world — drowning, poison — has no killer, so
    /// nobody earns anything and nothing panics looking for someone.
    #[tokio::test]
    async fn an_environmental_death_drops_nothing_and_credits_nobody() {
        let zone = zone_for_region(CIVIC);
        zone.spawn_authored_mobs();
        let id = zone.authored_mobs.lock().unwrap().keys().next().unwrap().clone();
        // Killed outright with no attacker, the way drowning/poison does it.
        zone.entities.lock().unwrap().get_mut(&id).unwrap().hp = 0;

        // Wait for the DESPAWN (the outcome that does happen), then assert the
        // pelt isn't among what was emitted — waiting on an absence directly
        // would just be a sleep with extra steps.
        let packets = drive_until(zone.clone(), |p| p.contains("\"despawn\"")).await;
        assert!(
            !packets.iter().any(|p| p.contains("dog_pelt")),
            "the world killed it, but somebody got paid"
        );
        assert!(
            packets.iter().any(|p| p.contains("\"despawn\"") && p.contains(&id)),
            "it should still have died"
        );
    }

    /// Ambient mobs drop nothing. The bounty should send you to the authored
    /// pack; a dropping ambient mob would turn every zone in the world into a
    /// farm — which is also why they stay speciesless (#158).
    #[tokio::test]
    async fn an_ambient_mob_drops_nothing() {
        let zone = zone_for_region(CIVIC);
        {
            let mut es = zone.entities.lock().unwrap();
            es.insert("mob_ambient".to_string(), Entity::mob(9000, 9000));
            es.get_mut("mob_ambient").unwrap().hp = 1;
            es.insert("hunter".to_string(), Entity::player(9000, 9000, PLAYER_MAX_HP));
            es.get_mut("hunter").unwrap().facing = (1, 0);
        }
        zone.entities.lock().unwrap().get_mut("hunter").unwrap().swinging = true;

        let packets = drive_until(zone.clone(), |p| p.contains("\"despawn\"")).await;
        assert!(
            !packets.iter().any(|p| p.contains("\"gather_yield\"")),
            "an anonymous mob paid out loot: {packets:?}"
        );
    }

    /// An authored creature never strays far from its ground (#158). Mob wander
    /// is 2 units/tick at 20Hz, so an unleashed one drifts over a thousand units
    /// within a minute — which is exactly what happened live, scattering the pack
    /// up to 1250 units from its anchor and carrying it clean out of every
    /// promise the siting made (dry land, level footing, clear of everything).
    #[tokio::test]
    async fn an_authored_creature_stays_near_its_ground() {
        let zone = zone_for_region(CIVIC);
        zone.spawn_authored_mobs();
        let homes: Vec<(String, (i32, i32))> = zone
            .authored_mobs
            .lock()
            .unwrap()
            .iter()
            .map(|(id, m)| (id.clone(), (m.x, m.y)))
            .collect();

        // Long enough to drift far past the leash if nothing held them.
        drive(zone.clone(), 400).await;

        let leash = mmo::world::AUTHORED_MOB_LEASH as f64;
        let entities = zone.entities.lock().unwrap();
        for (id, home) in &homes {
            let e = entities.get(id).unwrap_or_else(|| panic!("{id} vanished"));
            let drift =
                (((e.x - home.0) as f64).powi(2) + ((e.y - home.1) as f64).powi(2)).sqrt();
            // A step of slack: the leash turns them around, it doesn't teleport
            // them, so they can be a stride or two beyond it at any instant.
            assert!(
                drift <= leash + (MOB_SPEED * 2) as f64,
                "{id} drifted {drift:.0} from home, past the {leash:.0} leash"
            );
        }
    }

    /// Dragged out and released, it walks home rather than standing where it was
    /// abandoned. The leash sits ahead of the chase on purpose — otherwise a
    /// player could kite the pack across the district and strand it.
    #[tokio::test]
    async fn a_strayed_creature_walks_back_home() {
        let zone = zone_for_region(CIVIC);
        zone.spawn_authored_mobs();
        let (id, home) = {
            let a = zone.authored_mobs.lock().unwrap();
            let (id, m) = a.iter().next().unwrap();
            (id.clone(), (m.x, m.y))
        };

        // Shove it well past the leash, and park a player next to it so the
        // chase branch would fire if the leash didn't take priority.
        let far = (home.0 + 1200, home.1);
        {
            let mut es = zone.entities.lock().unwrap();
            es.get_mut(&id).unwrap().x = far.0;
            es.get_mut(&id).unwrap().y = far.1;
            es.insert("bait".to_string(), Entity::player(far.0 + 20, far.1, PLAYER_MAX_HP));
        }
        let before =
            (((far.0 - home.0) as f64).powi(2) + ((far.1 - home.1) as f64).powi(2)).sqrt();

        drive(zone.clone(), 40).await;

        let entities = zone.entities.lock().unwrap();
        let e = entities.get(&id).unwrap();
        let after = (((e.x - home.0) as f64).powi(2) + ((e.y - home.1) as f64).powi(2)).sqrt();
        assert!(
            after < before - 50.0,
            "it stayed out with the player instead of going home ({before:.0} -> {after:.0})"
        );
    }

    /// Ambient mobs are NOT leashed — they roam their whole region as they
    /// always have. The leash is a property of authored content, not of mobs.
    #[tokio::test]
    async fn ambient_mobs_are_not_leashed() {
        let zone = zone_for_region(CIVIC);
        zone.spawn_mobs(4);
        let entities = zone.entities.lock().unwrap();
        assert!(
            entities.values().filter(|e| e.kind == EntityKind::Mob).all(|e| e.home.is_none()),
            "an ambient mob was given a home — it would be leashed to a random point"
        );
    }

    /// Authored creatures are CONTENT, ambient mobs are TERRITORY. A pack must
    /// not suppress the ambient top-up — otherwise putting dogs somewhere would
    /// quietly change how that zone behaves for reasons nobody wrote down.
    #[tokio::test]
    async fn the_pack_does_not_suppress_the_ambient_population() {
        let zone = zone_for_region(CIVIC);
        zone.spawn_authored_mobs();
        let pack = zone.authored_mobs.lock().unwrap().len();
        assert!(pack > 0);

        // No ambient mobs yet: the trickle should still fill to its full quota
        // despite the pack already standing there.
        drive(zone.clone(), (MOB_RESPAWN_TICKS as u32 + 2) * MOBS_PER_ZONE as u32).await;

        let entities = zone.entities.lock().unwrap();
        let ambient = entities
            .values()
            .filter(|e| e.kind == EntityKind::Mob && e.species.is_none())
            .count();
        assert_eq!(
            ambient, MOBS_PER_ZONE,
            "the authored pack ate into the ambient population ({ambient} of {MOBS_PER_ZONE})"
        );
        assert_eq!(
            entities.values().filter(|e| e.species.is_some()).count(),
            pack,
            "and the pack is still there"
        );
    }

    /// Drive a freshly-built zone's `game_loop` for `ticks` with a wired (dummy)
    /// proxy channel, then return the live entity HP for `player_id`.
    async fn run_with_player_and_adjacent_mob(
        region: Region,
        player_id: &str,
        spot: (i32, i32),
        ticks: u32,
    ) -> i32 {
        let zone = zone_for_region(region);
        // game_loop needs a proxy tx or it idles; keep the rx so the channel stays open.
        let (tx, _rx) = mpsc::unbounded_channel();
        *zone.proxy_tx.lock().unwrap() = Some(tx);
        {
            let mut es = zone.entities.lock().unwrap();
            es.insert(player_id.to_string(), Entity::player(spot.0, spot.1, PLAYER_MAX_HP));
            es.insert("mob_x".to_string(), Entity::mob(spot.0, spot.1)); // in attack range
        }
        let runner = zone.clone();
        tokio::spawn(runner.game_loop());
        // Wait the requested number of ticks (plus slack for the immediate first tick).
        sleep(Duration::from_millis(TICK_MS * ticks as u64 + 60)).await;
        let hp = zone.entities.lock().unwrap().get(player_id).map(|e| e.hp).unwrap_or(0);
        hp
    }

    #[test]
    fn safe_in_capital_wilds_outside() {
        // Each authored district band is safe.
        assert!(zone_for_region(Region { x0: 0, y0: 0, x1: 6400, y1: 25600 }).is_safe()); // suburbs
        assert!(zone_for_region(Region { x0: 6400, y0: 6400, x1: 19200, y1: 19200 }).is_safe()); // civic
        assert!(zone_for_region(Region { x0: 19200, y0: 0, x1: 25600, y1: 25600 }).is_safe()); // market
        assert!(zone_for_region(Region { x0: 6400, y0: 0, x1: 19200, y1: 6400 }).is_safe()); // craftworks
        assert!(zone_for_region(Region { x0: 6400, y0: 19200, x1: 19200, y1: 25600 }).is_safe()); // old_quarter
        // The default whole-world zone is safe (centre is the Civic Centre).
        assert!(zone_for_region(Region { x0: 0, y0: 0, x1: 25600, y1: 25600 }).is_safe());
        // A region whose centre falls outside the authored capital is wilds.
        assert!(!zone_for_region(Region { x0: 26600, y0: 26600, x1: 32000, y1: 32000 }).is_safe());
    }

    /// Acceptance (#5): in the safe capital, a mob sitting on top of a player deals
    /// no damage — the player's HP is untouched.
    #[tokio::test]
    async fn safe_zone_deals_no_player_damage() {
        // Civic Centre band, centred on the town centre — safe.
        let region = Region { x0: 6400, y0: 6400, x1: 19200, y1: 19200 };
        let hp = run_with_player_and_adjacent_mob(region, "p1", (12800, 12800), 8).await;
        assert_eq!(hp, PLAYER_MAX_HP, "a player took damage inside the safe capital");
    }

    /// Control: the same setup in a wilds region *does* damage the player, proving
    /// the test actually exercises the mob-aggression/damage path that #5 gates.
    #[tokio::test]
    async fn wilds_zone_damages_player() {
        // A region whose centre is outside the capital -> wilds. It still contains
        // the player's spot so the mob can reach them.
        let region = Region { x0: 26600, y0: 26600, x1: 32000, y1: 32000 };
        let hp = run_with_player_and_adjacent_mob(region, "p1", (26650, 26650), 8).await;
        assert!(hp < PLAYER_MAX_HP, "a wilds mob should have damaged the player (hp={hp})");
    }

    // --- #12: death hands respawn off to the gateway (bed-or-fallback) --------

    /// A dead player is removed from the zone's own map (not respawned in
    /// place) and the zone reports the death to the gateway instead — the
    /// gateway alone decides where they reappear, since that point may be a
    /// different zone entirely (their bed).
    #[tokio::test]
    async fn dead_player_is_removed_and_reported_to_gateway_not_respawned_locally() {
        let zone = zone_for_region(CIVIC);
        zone.entities.lock().unwrap().insert(
            "p1".to_string(),
            Entity { hp: 0, ..Entity::player(12800, 12800, PLAYER_MAX_HP) },
        );
        let packets = drive(zone.clone(), 1).await;

        assert!(!zone.entities.lock().unwrap().contains_key("p1"), "the dead player should be removed, not teleported in place");
        assert!(packets.iter().any(|p| p.contains("\"despawn\"") && p.contains("\"p1\"")),
            "bystanders should see the dead player despawn from this zone");
        assert!(packets.iter().any(|p| p.contains("\"you_died\"") && p.contains("\"p1\"")),
            "the player's own client should learn it died");
        assert!(packets.iter().any(|p| p.contains("\"player_died\"") && p.contains("\"p1\"")
            && p.contains(&format!("\"hp\":{PLAYER_MAX_HP}"))),
            "the gateway should be told the death happened, with the hp to respawn at");
        // Ordering is load-bearing (#88 regression): the gateway relocates the
        // client the moment it processes `player_died`, and only routes
        // player-addressed frames while the client still points at this zone —
        // so `you_died` must be on the wire FIRST or a cross-zone respawn
        // silently swallows it.
        let you_died_at = packets.iter().position(|p| p.contains("\"you_died\"")).unwrap();
        let died_at = packets.iter().position(|p| p.contains("\"player_died\"")).unwrap();
        assert!(you_died_at < died_at, "you_died must precede player_died on the wire");
    }

    // --- #87: breath & drowning (environmental vitals) -------------------------

    /// A submerged player drains breath, then suffocates to death — and the
    /// death takes the ordinary #12 handoff. Runs in the SAFE civic district
    /// on purpose: the safe-hub guard suppresses mob/PvP damage only, never
    /// environmental damage (the whole capital is safe, so a safe-gated river
    /// could never drown anyone).
    #[tokio::test]
    async fn submerged_player_suffocates_to_death_even_in_the_safe_capital() {
        let zone = zone_for_region(CIVIC);
        zone.entities.lock().unwrap().insert(
            "p1".to_string(),
            Entity {
                submerged: true,
                breath: 2,
                hp: 6,
                ..Entity::player(12800, 12800, PLAYER_MAX_HP)
            },
        );
        // breath 2 -> 0 in two ticks, first 3-damage on the next, second one
        // DROWN_PERIOD_TICKS later kills — well inside 12 ticks.
        let packets = drive(zone.clone(), 12).await;

        assert!(!zone.entities.lock().unwrap().contains_key("p1"), "the drowned player should be removed for the gateway handoff");
        assert!(packets.iter().any(|p| p.contains("\"you_died\"") && p.contains("\"p1\"")),
            "the drowning player's client should learn it died");
        assert!(packets.iter().any(|p| p.contains("\"player_died\"") && p.contains("\"p1\"")),
            "the gateway should be told so bed-or-spawn respawn happens");
        // Vitals ride status updates while the drowning was in progress.
        assert!(packets.iter().any(|p| p.contains("\"breath\"") && p.contains("\"submerged\":true")),
            "status updates should carry breath/submerged while in the water");
    }

    /// Holding breath is survivable: a submerged player with plenty of breath
    /// takes no damage at all — only the timer runs.
    #[tokio::test]
    async fn submerged_with_breath_left_takes_no_damage() {
        let zone = zone_for_region(CIVIC);
        zone.entities.lock().unwrap().insert(
            "p1".to_string(),
            Entity { submerged: true, ..Entity::player(12800, 12800, PLAYER_MAX_HP) },
        );
        drive(zone.clone(), 5).await;
        let entities = zone.entities.lock().unwrap();
        let e = entities.get("p1").expect("still alive");
        assert_eq!(e.hp, PLAYER_MAX_HP, "no damage while breath lasts");
        assert!(e.breath < BREATH_MAX_TICKS, "but the breath timer must be draining");
    }

    /// Surfacing refills breath quickly (faster than it drained), and a dry
    /// player with full lungs is completely untouched by the vitals step.
    #[tokio::test]
    async fn breath_refills_on_surfacing_and_dry_land_is_harmless() {
        let zone = zone_for_region(CIVIC);
        zone.entities.lock().unwrap().insert(
            "wet".to_string(),
            Entity { breath: 10, ..Entity::player(12800, 12800, PLAYER_MAX_HP) },
        );
        zone.entities.lock().unwrap().insert(
            "dry".to_string(),
            Entity::player(12810, 12810, PLAYER_MAX_HP),
        );
        drive(zone.clone(), 4).await;
        let entities = zone.entities.lock().unwrap();
        let wet = entities.get("wet").unwrap();
        assert!(wet.breath > 10 + BREATH_REFILL_PER_TICK, "surfaced breath should refill by several ticks' worth (got {})", wet.breath);
        assert_eq!(wet.hp, PLAYER_MAX_HP);
        let dry = entities.get("dry").unwrap();
        assert_eq!((dry.hp, dry.breath), (PLAYER_MAX_HP, BREATH_MAX_TICKS), "a dry, full-lunged player is untouched");
    }

    /// `env_state` flips the live entity's flag both ways, ignores unknown
    /// players, and a recreated entity (fresh `spawn_entity` after respawn or
    /// migration) starts dry-by-default — the gateway's 1/s unconditional
    /// re-push is what reconverges it, by design.
    #[test]
    fn apply_env_state_sets_and_clears_and_recreation_resets() {
        let zone = zone_for_region(CIVIC);
        zone.entities.lock().unwrap().insert(
            "p1".to_string(),
            Entity::player(12800, 12800, PLAYER_MAX_HP),
        );
        zone.apply_env_state("p1", true, 0);
        assert!(zone.entities.lock().unwrap().get("p1").unwrap().submerged);
        zone.apply_env_state("p1", false, 0);
        assert!(!zone.entities.lock().unwrap().get("p1").unwrap().submerged);
        zone.apply_env_state("ghost", true, 0); // unknown player: silent no-op

        // Recreation (what spawn_entity does) resets to defaults...
        zone.apply_env_state("p1", true, 0);
        zone.entities.lock().unwrap().insert(
            "p1".to_string(),
            Entity::player(12800, 12800, PLAYER_MAX_HP),
        );
        let fresh_ok = {
            let entities = zone.entities.lock().unwrap();
            let e = entities.get("p1").unwrap();
            !e.submerged && e.breath == BREATH_MAX_TICKS
        };
        assert!(fresh_ok, "a recreated entity starts dry with full lungs");
        // ...and the next gateway push restores the truth.
        zone.apply_env_state("p1", true, 0);
        assert!(zone.entities.lock().unwrap().get("p1").unwrap().submerged);
    }

    // --- #88: poison buildup, proc, DoT -----------------------------------------

    /// Standing among poison trees builds up, procs at the threshold, and the
    /// DoT then kills through the ordinary #12 handoff — in the SAFE civic
    /// district, same environmental-damage-bypasses-the-safe-guard rule as
    /// drowning.
    #[tokio::test]
    async fn poison_builds_up_procs_and_kills_even_in_the_safe_capital() {
        let zone = zone_for_region(CIVIC);
        zone.entities.lock().unwrap().insert(
            "p1".to_string(),
            Entity {
                poison_sources: 1,
                poison_buildup: POISON_PROC_AT - 2,
                hp: 3,
                ..Entity::player(12800, 12800, PLAYER_MAX_HP)
            },
        );
        // 2 ticks to proc, then 1 hp/tick: dead within ~6 ticks.
        let packets = drive(zone.clone(), 10).await;

        assert!(!zone.entities.lock().unwrap().contains_key("p1"), "the poisoned player should die and hand off");
        assert!(packets.iter().any(|p| p.contains("\"you_died\"") && p.contains("\"p1\"")));
        assert!(packets.iter().any(|p| p.contains("\"player_died\"") && p.contains("\"p1\"")));
        assert!(packets.iter().any(|p| p.contains("\"poisoned\":true")),
            "status updates should have carried the proc");
    }

    /// Leaving the trees before the proc drains the buildup — the forest edge
    /// gives real escape time, and short exposure costs nothing but nerves.
    #[tokio::test]
    async fn poison_buildup_decays_without_proc_when_clear_of_trees() {
        let zone = zone_for_region(CIVIC);
        zone.entities.lock().unwrap().insert(
            "p1".to_string(),
            Entity {
                poison_sources: 0,
                poison_buildup: POISON_PROC_AT - 10, // was at the brink, stepped out in time
                ..Entity::player(12800, 12800, PLAYER_MAX_HP)
            },
        );
        drive(zone.clone(), 6).await;
        let entities = zone.entities.lock().unwrap();
        let e = entities.get("p1").expect("alive and well");
        assert!(!e.poisoned, "no proc once clear of the trees");
        assert!(e.poison_buildup < POISON_PROC_AT - 10, "buildup must decay (got {})", e.poison_buildup);
        assert_eq!(e.hp, PLAYER_MAX_HP, "buildup alone never damages");
    }

    /// More trees in range build up faster — a dense forest interior procs in
    /// a fraction of the single-tree time.
    #[tokio::test]
    async fn poison_builds_faster_among_more_trees() {
        let zone = zone_for_region(CIVIC);
        {
            let mut es = zone.entities.lock().unwrap();
            es.insert("edge".to_string(), Entity { poison_sources: 1, ..Entity::player(12800, 12800, PLAYER_MAX_HP) });
            es.insert("deep".to_string(), Entity { poison_sources: 7, ..Entity::player(12810, 12810, PLAYER_MAX_HP) });
        }
        drive(zone.clone(), 6).await;
        let entities = zone.entities.lock().unwrap();
        let edge = entities.get("edge").unwrap().poison_buildup;
        let deep = entities.get("deep").unwrap().poison_buildup;
        assert!(deep >= edge * 3, "7 trees should build up ~4x one tree (edge={edge}, deep={deep})");
    }

    /// The proc is a death sentence: once poisoned, walking out of the forest
    /// (sources back to 0, buildup irrelevant) does NOT stop the DoT — only
    /// death (and the fresh respawned entity) clears it in v1.
    #[tokio::test]
    async fn poisoned_state_sticks_after_leaving_the_forest() {
        let zone = zone_for_region(CIVIC);
        zone.entities.lock().unwrap().insert(
            "p1".to_string(),
            Entity {
                poisoned: true,
                poison_sources: 0,
                poison_buildup: 0,
                ..Entity::player(12800, 12800, PLAYER_MAX_HP)
            },
        );
        drive(zone.clone(), 6).await;
        let entities = zone.entities.lock().unwrap();
        let e = entities.get("p1").expect("not dead yet from full hp");
        assert!(e.poisoned, "no cure in v1");
        assert!(e.hp < PLAYER_MAX_HP, "the DoT keeps ticking outside the forest (hp={})", e.hp);
        // And the recreated entity (respawn) is clean — same reset the
        // env_state recreation test proves for submerged.
        drop(entities);
        zone.entities.lock().unwrap().insert("p1".to_string(), Entity::player(12800, 12800, PLAYER_MAX_HP));
        let entities = zone.entities.lock().unwrap();
        let fresh = entities.get("p1").unwrap();
        assert!(!fresh.poisoned);
        assert_eq!(fresh.poison_buildup, 0);
    }

    // --- #7: resource gathering -----------------------------------------------

    const CIVIC: Region = Region { x0: 6400, y0: 6400, x1: 19200, y1: 19200 };
    const TREE: &str = "node_civic_tree_0"; // authored at (12740, 12740), wood, qty 5

    /// Civic zone with its authored nodes spawned and a player standing on the tree.
    fn civic_zone_on_tree() -> Arc<ZoneServer> {
        let zone = zone_for_region(CIVIC);
        zone.spawn_nodes();
        zone.entities.lock().unwrap().insert(
            "p1".to_string(),
            Entity::player(12740, 12740, PLAYER_MAX_HP),
        );
        zone
    }

    const ROCK: &str = "node_civic_rock_0"; // authored at (12800, 12690), stone, qty 5

    /// Civic zone with its authored nodes spawned and a player standing on the rock.
    fn civic_zone_on_rock() -> Arc<ZoneServer> {
        let zone = zone_for_region(CIVIC);
        zone.spawn_nodes();
        zone.entities.lock().unwrap().insert(
            "p1".to_string(),
            Entity::player(12800, 12690, PLAYER_MAX_HP),
        );
        zone
    }

    /// Wire a fresh proxy_tx onto `zone` without starting the game loop — for
    /// instant (non-tick-driven) handlers like `apply_ability_swing`, which
    /// push their outcome synchronously the moment they're called.
    fn wire_proxy_tx(zone: &Arc<ZoneServer>) -> mpsc::UnboundedReceiver<Message> {
        let (tx, rx) = mpsc::unbounded_channel();
        *zone.proxy_tx.lock().unwrap() = Some(tx);
        rx
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<Message>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(Message::Text(t)) = rx.try_recv() {
            out.push(t);
        }
        out
    }

    /// Run the game loop for `ticks` and return every text packet the zone emitted.
    /// Drive the sim until `want` matches one of the emitted packets, or a
    /// generous deadline passes, and return everything seen.
    ///
    /// A fixed `drive(n)` sleeps wall-clock time and ASSUMES n ticks elapsed —
    /// which is fine alone and flaky under the full suite's parallel load, where
    /// the sim task may not be scheduled that often. Same trap `poll_progress_json`
    /// was added for on the gateway side. Anything asserting on a specific
    /// outcome should wait for that outcome, not for the clock.
    async fn drive_until(
        zone: Arc<ZoneServer>,
        want: impl Fn(&str) -> bool,
    ) -> Vec<String> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        *zone.proxy_tx.lock().unwrap() = Some(tx);
        let runner = zone.clone();
        tokio::spawn(runner.game_loop());
        let mut out = Vec::new();
        for _ in 0..100 {
            sleep(Duration::from_millis(TICK_MS * 2)).await;
            while let Ok(Message::Text(t)) = rx.try_recv() {
                out.push(t);
            }
            if out.iter().any(|p| want(p)) {
                break;
            }
        }
        out
    }

    async fn drive(zone: Arc<ZoneServer>, ticks: u32) -> Vec<String> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        *zone.proxy_tx.lock().unwrap() = Some(tx);
        let runner = zone.clone();
        tokio::spawn(runner.game_loop());
        sleep(Duration::from_millis(TICK_MS * ticks as u64 + 80)).await;
        let mut out = Vec::new();
        while let Ok(Message::Text(t)) = rx.try_recv() {
            out.push(t);
        }
        out
    }

    // --- Mining/abilities epic #123, #117/#125: ability swings -----------------

    /// A swing in range of a stocked rock yields exactly one stone, mining
    /// XP, an `ability.result` success, and an updated node status — no
    /// channel, no progress bar, all synchronous.
    #[tokio::test]
    async fn pick_swing_yields_stone_and_mining_xp() {
        let zone = civic_zone_on_rock();
        let mut rx = wire_proxy_tx(&zone);
        zone.apply_ability_swing("p1", "pick", ROCK, 1600);

        let packets = drain(&mut rx);
        assert!(packets.iter().any(|p| p.contains("\"ability.result\"") && p.contains("\"ok\":true")
            && p.contains("\"cooldown_ms\":1600")), "expected a successful ability.result: {packets:?}");
        assert!(packets.iter().any(|p| p.contains("\"gather_yield\"") && p.contains("\"item_id\":\"stone\"")
            && p.contains("\"skill\":\"mining\"") && p.contains("\"xp\":12")
            && p.contains("\"ability_id\":\"pick\"")),
            "missing the internal gather_yield with mining xp and ability_id (#128 needs it to wear the tool): {packets:?}");
        assert_eq!(zone.nodes.lock().unwrap().get(ROCK).unwrap().qty, 4, "one stone taken");
    }

    /// Depleting a node on the final swing despawns it and schedules a
    /// respawn — same tail behaviour as a gather channel's last tick.
    #[tokio::test]
    async fn pick_swing_depletes_node_then_despawns_and_schedules_respawn() {
        let zone = civic_zone_on_rock();
        zone.nodes.lock().unwrap().get_mut(ROCK).unwrap().qty = 1;
        let mut rx = wire_proxy_tx(&zone);
        zone.apply_ability_swing("p1", "pick", ROCK, 1600);

        let packets = drain(&mut rx);
        assert!(packets.iter().any(|p| p.contains("\"despawn\"") && p.contains(ROCK)));
        let nodes = zone.nodes.lock().unwrap();
        let n = nodes.get(ROCK).unwrap();
        assert_eq!(n.qty, 0);
        assert!(n.respawn_timer > 0, "respawn should be scheduled");
    }

    /// Too far from the rock: rejected, nothing taken, no yield.
    #[tokio::test]
    async fn pick_swing_out_of_range_is_rejected() {
        let zone = civic_zone_on_rock();
        zone.entities.lock().unwrap().get_mut("p1").unwrap().x = 12900; // well past PICK_RANGE
        let mut rx = wire_proxy_tx(&zone);
        zone.apply_ability_swing("p1", "pick", ROCK, 1600);

        let packets = drain(&mut rx);
        assert!(packets.iter().any(|p| p.contains("\"ability.result\"") && p.contains("\"ok\":false")
            && p.contains("\"reason\":\"out_of_range\"")));
        assert!(packets.iter().all(|p| !p.contains("\"gather_yield\"")));
        assert_eq!(zone.nodes.lock().unwrap().get(ROCK).unwrap().qty, 5, "node untouched");
    }

    /// An empty node (or a wrong node kind — a tree, say) can't be swung at.
    #[tokio::test]
    async fn pick_swing_on_an_exhausted_or_wrong_node_is_rejected() {
        let zone = civic_zone_on_rock();
        zone.nodes.lock().unwrap().get_mut(ROCK).unwrap().qty = 0;
        let mut rx = wire_proxy_tx(&zone);
        zone.apply_ability_swing("p1", "pick", ROCK, 1600);
        assert!(drain(&mut rx).iter().any(|p| p.contains("\"reason\":\"exhausted\"")));

        // Standing on a tree instead: pick only targets stone.
        let zone2 = civic_zone_on_tree();
        let mut rx2 = wire_proxy_tx(&zone2);
        zone2.apply_ability_swing("p1", "pick", TREE, 1600);
        assert!(drain(&mut rx2).iter().any(|p| p.contains("\"reason\":\"exhausted\"")));
        assert_eq!(zone2.nodes.lock().unwrap().get(TREE).unwrap().qty, 5, "wrong-target swing takes nothing");
    }

    /// A successful swing's `ability.result` carries what was actually
    /// taken (#125) — the client flashes it as an ordinary "+N" gain notice,
    /// closing a gap where ability swings never showed one (only the old
    /// channel's now-deleted `gather.result` did).
    #[tokio::test]
    async fn ability_result_carries_the_yield_for_the_client_flash() {
        let zone = civic_zone_on_rock();
        let mut rx = wire_proxy_tx(&zone);
        zone.apply_ability_swing("p1", "pick", ROCK, 1600);
        let packets = drain(&mut rx);
        assert!(packets.iter().any(|p| p.contains("\"ability.result\"") && p.contains("\"ok\":true")
            && p.contains("\"item_id\":\"stone\"") && p.contains("\"qty\":1")),
            "expected the yield on the successful ability.result: {packets:?}");

        // A rejection carries no item_id/qty — nothing was taken.
        zone.entities.lock().unwrap().get_mut("p1").unwrap().x = 12900; // out of range
        let mut rx2 = wire_proxy_tx(&zone);
        zone.apply_ability_swing("p1", "pick", ROCK, 1600);
        let packets2 = drain(&mut rx2);
        assert!(packets2.iter().any(|p| p.contains("\"ok\":false") && !p.contains("\"item_id\"")),
            "a rejected swing must not carry an item_id: {packets2:?}");
    }

    /// Chop mirrors Pick exactly (#125) — the same `apply_ability_swing` is
    /// generic over `ability_target_item`/`governing_skill`, so a swing
    /// against a tree yields wood and woodcutting xp instead of stone and
    /// mining xp, with identical range/depletion/wrong-target behaviour.
    #[tokio::test]
    async fn chop_swing_yields_wood_and_woodcutting_xp() {
        let zone = civic_zone_on_tree();
        let mut rx = wire_proxy_tx(&zone);
        zone.apply_ability_swing("p1", "chop", TREE, 1600);

        let packets = drain(&mut rx);
        assert!(packets.iter().any(|p| p.contains("\"ability.result\"") && p.contains("\"ok\":true")
            && p.contains("\"item_id\":\"wood\"") && p.contains("\"qty\":1")),
            "expected a successful ability.result: {packets:?}");
        assert!(packets.iter().any(|p| p.contains("\"gather_yield\"") && p.contains("\"item_id\":\"wood\"")
            && p.contains("\"skill\":\"woodcutting\"") && p.contains("\"xp\":12")),
            "missing the internal gather_yield with woodcutting xp: {packets:?}");
        assert_eq!(zone.nodes.lock().unwrap().get(TREE).unwrap().qty, 4, "one wood taken");
    }

    #[tokio::test]
    async fn chop_swing_depletes_node_then_despawns_and_schedules_respawn() {
        let zone = civic_zone_on_tree();
        zone.nodes.lock().unwrap().get_mut(TREE).unwrap().qty = 1;
        let mut rx = wire_proxy_tx(&zone);
        zone.apply_ability_swing("p1", "chop", TREE, 1600);

        let packets = drain(&mut rx);
        assert!(packets.iter().any(|p| p.contains("\"despawn\"") && p.contains(TREE)));
        let nodes = zone.nodes.lock().unwrap();
        let n = nodes.get(TREE).unwrap();
        assert_eq!(n.qty, 0);
        assert!(n.respawn_timer > 0, "respawn should be scheduled");
    }

    #[tokio::test]
    async fn chop_swing_out_of_range_is_rejected() {
        let zone = civic_zone_on_tree();
        zone.entities.lock().unwrap().get_mut("p1").unwrap().x = 12850; // well past SWING_RANGE
        let mut rx = wire_proxy_tx(&zone);
        zone.apply_ability_swing("p1", "chop", TREE, 1600);

        let packets = drain(&mut rx);
        assert!(packets.iter().any(|p| p.contains("\"ability.result\"") && p.contains("\"ok\":false")
            && p.contains("\"reason\":\"out_of_range\"")));
        assert!(packets.iter().all(|p| !p.contains("\"gather_yield\"")));
        assert_eq!(zone.nodes.lock().unwrap().get(TREE).unwrap().qty, 5, "node untouched");
    }

    /// Chop only targets wood — a rock is the wrong node kind for it, same
    /// as a tree is the wrong kind for Pick.
    #[tokio::test]
    async fn chop_swing_on_an_exhausted_or_wrong_node_is_rejected() {
        let zone = civic_zone_on_tree();
        zone.nodes.lock().unwrap().get_mut(TREE).unwrap().qty = 0;
        let mut rx = wire_proxy_tx(&zone);
        zone.apply_ability_swing("p1", "chop", TREE, 1600);
        assert!(drain(&mut rx).iter().any(|p| p.contains("\"reason\":\"exhausted\"")));

        let zone2 = civic_zone_on_rock();
        let mut rx2 = wire_proxy_tx(&zone2);
        zone2.apply_ability_swing("p1", "chop", ROCK, 1600);
        assert!(drain(&mut rx2).iter().any(|p| p.contains("\"reason\":\"exhausted\"")));
        assert_eq!(zone2.nodes.lock().unwrap().get(ROCK).unwrap().qty, 5, "wrong-target swing takes nothing");
    }

    // --- Mining/abilities epic #123, #118: quarry foreman NPC talk range --------

    const FOREMAN: &str = "npc_quarry_foreman"; // authored at (8232, 13945)

    /// Civic zone with its authored NPCs spawned and a player standing at the foreman.
    fn civic_zone_at_foreman() -> Arc<ZoneServer> {
        let zone = zone_for_region(CIVIC);
        zone.spawn_npcs();
        zone.entities.lock().unwrap().insert(
            "p1".to_string(),
            Entity::player(8232, 13945, PLAYER_MAX_HP),
        );
        zone
    }

    /// Standing close enough forwards an internal `npc_interact` — the
    /// gateway decides what's actually said (and whether anything's handed
    /// over); the zone only ever gates on proximity.
    #[tokio::test]
    async fn npc_talk_in_range_forwards_interact() {
        let zone = civic_zone_at_foreman();
        let mut rx = wire_proxy_tx(&zone);
        zone.apply_npc_talk("p1", FOREMAN);
        let packets = drain(&mut rx);
        assert!(
            packets.iter().any(|p| p.contains("\"npc_interact\"")
                && p.contains(FOREMAN) && p.contains("\"p1\"")),
            "expected a forwarded npc_interact: {packets:?}"
        );
    }

    /// Too far away: silent no-op, same convention as every other
    /// proximity-gated action in this file.
    #[tokio::test]
    async fn npc_talk_out_of_range_is_silent() {
        let zone = civic_zone_at_foreman();
        zone.entities.lock().unwrap().get_mut("p1").unwrap().x = 8500; // well past NPC_TALK_RANGE
        let mut rx = wire_proxy_tx(&zone);
        zone.apply_npc_talk("p1", FOREMAN);
        assert!(drain(&mut rx).is_empty(), "too far to talk");
    }

    /// The logging camp's starter grove spawns alongside the quarry in the
    /// civic district (#126) — same authored-spawn mechanism as every other
    /// resource cluster, just a second site.
    #[tokio::test]
    async fn logging_camp_nodes_spawn_in_civic() {
        let zone = zone_for_region(CIVIC);
        zone.spawn_nodes();
        let nodes = zone.nodes.lock().unwrap();
        for i in 0..6 {
            let id = format!("node_logging_tree_{i}");
            let n = nodes.get(&id).unwrap_or_else(|| panic!("expected {id} to spawn"));
            assert_eq!(n.item_id, "wood");
            assert!(n.qty > 0);
        }
    }

    /// The logging foreman (#126) talks through the exact same generic
    /// proximity gate as the quarry foreman — proving `apply_npc_talk`
    /// never hardcoded which NPC it's gating, only distance.
    #[tokio::test]
    async fn logging_foreman_talk_in_range_forwards_interact() {
        let zone = zone_for_region(CIVIC);
        zone.spawn_npcs();
        zone.entities.lock().unwrap().insert(
            "p1".to_string(),
            Entity::player(14300, 11400, PLAYER_MAX_HP),
        );
        let mut rx = wire_proxy_tx(&zone);
        zone.apply_npc_talk("p1", "npc_logging_foreman");
        let packets = drain(&mut rx);
        assert!(
            packets.iter().any(|p| p.contains("\"npc_interact\"")
                && p.contains("npc_logging_foreman") && p.contains("\"p1\"")),
            "expected a forwarded npc_interact: {packets:?}"
        );
    }

    #[tokio::test]
    async fn build_board_spawns_in_civic() {
        // Proximity gating for build.contribute now lives at the gateway (it knows
        // every order's placement, not just authored boards); the zone just spawns
        // the authored board entities for rendering.
        let zone = zone_for_region(CIVIC);
        zone.spawn_build_boards();
        let boards = zone.build_boards.lock().unwrap().clone();
        assert!(!boards.is_empty(), "the civic centre has an authored build board");
    }

    #[tokio::test]
    async fn plots_spawn_in_suburbs_and_gate_geometrically() {
        // Suburbs band — the only district with an authored plot grid.
        let region = Region { x0: 0, y0: 0, x1: 6400, y1: 25600 };
        let zone = zone_for_region(region);
        zone.spawn_plots();
        let plots = zone.plots.lock().unwrap().clone();
        assert_eq!(plots.len(), 240, "every starter plot sits in the suburbs band");
        let p = plots[0];
        assert!(zone.on_a_plot(p.x, p.y), "the plot's own corner is on the plot");
        assert!(zone.on_a_plot(p.x + p.w / 2, p.y + p.h / 2), "the plot's centre is on the plot");
        assert!(!zone.on_a_plot(p.x - 200, p.y), "well outside any plot");

        // A civic-only region has no plots at all — nowhere gates as "on a plot".
        let civic_zone = zone_for_region(CIVIC);
        civic_zone.spawn_plots();
        assert!(civic_zone.plots.lock().unwrap().is_empty());
        assert!(!civic_zone.on_a_plot(12800, 12800), "no plot grid in the civic centre");
    }

    /// #13: deposit/withdraw and crafting are gated on proximity to a *specific*
    /// placed home structure (not just anywhere on the plot), and the gateway's
    /// full replace (`home_structures_sync`) vs incremental add
    /// (`home_structure_added`) both update the zone's live cache correctly.
    #[tokio::test]
    async fn home_structures_gate_storage_and_crafting_by_proximity() {
        let zone = zone_for_region(CIVIC);
        *zone.home_structures.lock().unwrap() = vec![
            HomeStructureRef { id: "s1".to_string(), kind: "storage".to_string(), x: 500, y: 500 },
        ];
        assert!(zone.near_storage(500, 500), "on the chest");
        assert!(zone.near_storage(500 + HOME_STRUCTURE_RANGE - 1, 500), "just inside range");
        assert!(!zone.near_storage(500 + HOME_STRUCTURE_RANGE + 20, 500), "out of range");
        assert!(!zone.near_home_structure("crafting", 500, 500), "wrong kind");

        // Incrementally adding a crafting station (as placement would) makes it
        // gate too, without disturbing the existing storage entry.
        let mut hs = zone.home_structures.lock().unwrap();
        hs.push(HomeStructureRef { id: "s2".to_string(), kind: "crafting".to_string(), x: 700, y: 700 });
        drop(hs);
        assert!(zone.near_home_structure("crafting", 700, 700));
        assert!(zone.near_storage(500, 500), "the earlier chest is still known");

        // Removing one (a rent reclaim demolished it, #14) stops it gating,
        // without disturbing the other.
        zone.home_structures.lock().unwrap().retain(|s| s.id != "s1");
        assert!(!zone.near_storage(500, 500), "the demolished chest no longer gates");
        assert!(zone.near_home_structure("crafting", 700, 700), "the crafting station is unaffected");
    }

    #[tokio::test]
    async fn node_respawns_after_timer() {
        let zone = civic_zone_on_tree();
        {
            let mut nodes = zone.nodes.lock().unwrap();
            let n = nodes.get_mut(TREE).unwrap();
            n.qty = 0;
            n.respawn_timer = 2; // refills in ~2 ticks
        }
        let packets = drive(zone.clone(), 5).await;

        assert_eq!(zone.nodes.lock().unwrap().get(TREE).unwrap().qty, 5, "node refilled");
        assert!(packets.iter().any(|p| p.contains(TREE) && p.contains("\"resource\"")),
            "respawn should emit a node status_update");
    }
}
