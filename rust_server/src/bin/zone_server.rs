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

/// Mirrors `mmo::world::WORLD_SIZE` / `proxy.rs`'s copy — keep in sync.
const WORLD_SIZE: i32 = 6400;

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
const MELEE_DAMAGE: i32 = 20;
const PLAYER_ATTACK_COOLDOWN: i32 = 6; // ticks between swings (~0.3s)

// --- Territory control (capture bar) ------------------------------------------
const MOB_RESPAWN_TICKS: i32 = 40; // a killed mob trickles back ~every 2s
const CAPTURE_MOB_THRESHOLD: usize = 2; // capture only progresses at/below this many mobs
const CAPTURE_RATE: f32 = 1.0; // bar units/tick while capturing (~5s to take a zone)
const CAPTURE_DECAY: f32 = 0.5; // bar units/tick lost when a capture stalls

// --- Resource gathering -------------------------------------------------------
const GATHER_RANGE: i32 = 50; // must be within this of a node to gather it
const GATHER_PERIOD: i32 = 20; // ticks per yielded unit (~1s); a 5-qty node ~5s
const GATHER_XP: i64 = 10; // gathering-skill xp per unit
const NODE_RESPAWN_TICKS: i32 = 200; // a depleted node refills after ~10s

// --- Storage ------------------------------------------------------------------
const STORAGE_RANGE: i32 = 60; // must be within this of a storage point to use it

// --- Build orders -------------------------------------------------------------
const BOARD_RANGE: i32 = 60; // must be within this of a build board to contribute

// --- Home structures (#13) -----------------------------------------------------
const HOME_STRUCTURE_RANGE: i32 = 60; // must be within this of a placed bed/storage/crafting

type Tx = mpsc::UnboundedSender<Message>;

#[derive(Clone, Copy, PartialEq)]
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
        }
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

/// An in-progress gather: which node a player is working and how far along the
/// current unit they are.
struct GatherJob {
    node_id: String,
    progress: i32,
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

fn dist2(ax: i32, ay: i32, bx: i32, by: i32) -> i64 {
    let dx = (ax - bx) as i64;
    let dy = (ay - by) as i64;
    dx * dx + dy * dy
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

/// `gathering` is the entity's current `GatherJob`, if any — included so the
/// gateway's migration cache can resume it on a split/merge/rolling-update
/// rather than silently dropping it (#16).
fn entity_status_json(id: &str, e: &Entity, gathering: Option<&GatherJob>) -> Value {
    let mut state = json!({
        "x": e.x, "y": e.y, "hp": e.hp, "max_hp": e.max_hp,
        "type": e.kind.as_str(),
        "facing": [e.facing.0, e.facing.1],
    });
    if let Some(job) = gathering {
        state["gather_node"] = json!(job.node_id);
        state["gather_progress"] = json!(job.progress);
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
    /// In-progress gather jobs, keyed by player id.
    gathering: Mutex<HashMap<String, GatherJob>>,
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
            gathering: Mutex::new(HashMap::new()),
            storage_points: Mutex::new(Vec::new()),
            build_boards: Mutex::new(Vec::new()),
            plots: Mutex::new(Vec::new()),
            home_structures: Mutex::new(Vec::new()),
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

    /// Push the current state of every node and storage point to the gateway (which
    /// broadcasts it), so a just-joined client renders them.
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

    /// Whether `(px, py)` is within range of any build board in this zone.
    fn near_board(&self, px: i32, py: i32) -> bool {
        self.build_boards
            .lock()
            .unwrap()
            .iter()
            .any(|b| dist2(px, py, b.x, b.y) <= (BOARD_RANGE as i64).pow(2))
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

    /// Report a player death to the gateway, which alone knows where they should
    /// reappear (their bed, if one's set, else the default spawn) and — since
    /// that point may be owned by a different zone — handles the hand-off exactly
    /// like a `migrate_request` (#12). The entity has already been removed from
    /// this zone's map by the caller.
    fn send_player_died(&self, id: &str, hp: i32) {
        if let Some(tx) = self.proxy_tx.lock().unwrap().clone() {
            let _ = tx.send(Message::Text(
                json!({ "type": "player_died", "player_id": id, "hp": hp }).to_string(),
            ));
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
                self.spawn_nodes();
                self.spawn_storage_points();
                self.spawn_build_boards();
                self.spawn_plots();
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
                    // Resume an in-progress gather job carried over from a migration
                    // (#16) — only if the node actually exists here (it might not, if
                    // the split/merge moved the player away from it).
                    if let Some(node_id) = data.get("gather_node").and_then(|v| v.as_str()) {
                        let progress = data.get("gather_progress").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                        if self.nodes.lock().unwrap().contains_key(node_id) {
                            self.gathering.lock().unwrap().insert(
                                player_id.clone(),
                                GatherJob { node_id: node_id.to_string(), progress },
                            );
                        }
                    }
                    self.send_status_update(&player_id).await;
                    // Persistent players spawn this way (not via player_join), so they
                    // must also be sent the gatherable nodes, storage points, and build
                    // boards — otherwise a logged-in player sees no resources to gather.
                    self.send_all_nodes();
                    self.send_zone_stats();
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
                "gather.start" => {
                    // Begin gathering a node: validate it exists, is in range, and has
                    // stock; the per-unit yield is resolved authoritatively in the tick.
                    let node_id = data.get("node_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let ok = {
                        let entities = self.entities.lock().unwrap();
                        let nodes = self.nodes.lock().unwrap();
                        match (entities.get(&player_id), nodes.get(&node_id)) {
                            (Some(p), Some(n)) => n.qty > 0 && dist2(p.x, p.y, n.x, n.y) <= (GATHER_RANGE as i64).pow(2),
                            _ => false,
                        }
                    };
                    if ok {
                        self.gathering.lock().unwrap().insert(
                            player_id.clone(),
                            GatherJob { node_id, progress: 0 },
                        );
                    }
                }
                "gather.stop" => {
                    self.gathering.lock().unwrap().remove(&player_id);
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
                    // Validate the player is at a build board; the gateway (city
                    // authority) performs the durable pooled contribution and pushes
                    // the result.
                    let order_id = data.get("order_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let item_id = data.get("item_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let qty = data.get("qty").and_then(|v| v.as_i64()).unwrap_or(0);
                    let at_board = {
                        let entities = self.entities.lock().unwrap();
                        entities.get(&player_id).map(|p| self.near_board(p.x, p.y)).unwrap_or(false)
                    };
                    if at_board && qty > 0 && !order_id.is_empty() && !item_id.is_empty() {
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
            let mut died: Vec<String> = Vec::new();
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
                    let (mx, my, ready) = {
                        let e = entities.get(mid).unwrap();
                        (e.x, e.y, e.attack_cooldown == 0)
                    };
                    // Nearest player within aggro range. In a safe zone mobs never
                    // target players — they only wander (friendly wildlife) — so no
                    // contact damage is ever produced.
                    let mut best: Option<(String, i32, i32, i64)> = None;
                    if !safe {
                        for (pid, px, py) in &players {
                            let d2 = dist2(mx, my, *px, *py);
                            if d2 <= aggro2 && best.as_ref().map_or(true, |b| d2 < b.3) {
                                best = Some((pid.clone(), *px, *py, d2));
                            }
                        }
                    }

                    let e = entities.get_mut(mid).unwrap();
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

                // Apply mob contact damage to players. Never in a safe zone — the
                // map is empty there, but the guard makes "no player takes damage in
                // the capital" an explicit, enforced invariant.
                if !safe {
                    for (pid, dmg) in player_damage {
                        if let Some(e) = entities.get_mut(&pid) {
                            e.hp -= dmg;
                            changed.insert(pid);
                        }
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
                    let (px, py, fx, fy) = {
                        let e = entities.get(sid).unwrap();
                        (e.x, e.y, e.facing.0, e.facing.1)
                    };
                    let hits: Vec<String> = mob_positions
                        .iter()
                        .filter(|(_, mx, my)| in_melee_arc(px, py, fx, fy, *mx, *my))
                        .map(|(id, _, _)| id.clone())
                        .collect();
                    for hid in hits {
                        if let Some(m) = entities.get_mut(&hid) {
                            m.hp -= MELEE_DAMAGE;
                            changed.insert(hid);
                        }
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
                    entities.remove(&id);
                    changed.remove(&id);
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
                    let max_hp = entities.get(id).map(|e| e.max_hp).unwrap_or(PLAYER_MAX_HP);
                    entities.remove(id);
                    despawns.push(id.clone());
                    died.push(id.clone());
                    self.send_player_died(id, max_hp);
                }

                // --- 4. Trickle mobs back (slowly, so a zone can be cleared). ---
                let live_mobs = entities.values().filter(|e| e.kind == EntityKind::Mob).count();
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
                // `gathering` locked after `entities` (consistent order, matching
                // step 7 below) so each packet can include an in-progress gather
                // job — the gateway caches it to resume on migration (#16).
                let gathering = self.gathering.lock().unwrap();
                for id in &changed {
                    if let Some(e) = entities.get(id) {
                        packets.push(entity_status_json(id, e, gathering.get(id)).to_string());
                    }
                }
            } // entities (and gathering) lock released here

            // --- 7. Resource gathering + node respawn. ---
            // Locks taken after `entities` (consistent order) to avoid deadlock.
            {
                let entities = self.entities.lock().unwrap();
                let mut nodes = self.nodes.lock().unwrap();
                let mut gathering = self.gathering.lock().unwrap();
                let mut finished: Vec<String> = Vec::new();
                let mut touched: HashSet<String> = HashSet::new();

                for (pid, job) in gathering.iter_mut() {
                    // Player and node must exist, be in range, and the node have stock.
                    let (px, py) = match entities.get(pid) {
                        Some(p) => (p.x, p.y),
                        None => { finished.push(pid.clone()); continue; }
                    };
                    let (nx, ny, has_stock) = match nodes.get(&job.node_id) {
                        Some(n) => (n.x, n.y, n.qty > 0),
                        None => { finished.push(pid.clone()); continue; }
                    };
                    if !has_stock || dist2(px, py, nx, ny) > (GATHER_RANGE as i64).pow(2) {
                        finished.push(pid.clone());
                        continue;
                    }

                    job.progress += 1;
                    let pct = (job.progress * 100 / GATHER_PERIOD).min(100);
                    packets.push(json!({
                        "type": "gather.progress", "player_id": pid,
                        "node_id": job.node_id, "pct": pct,
                    }).to_string());

                    if job.progress >= GATHER_PERIOD {
                        job.progress = 0;
                        let node = nodes.get_mut(&job.node_id).unwrap();
                        node.qty -= 1;
                        let item = node.item_id.clone();
                        touched.insert(node.id.clone());
                        // Client-facing yield feedback.
                        packets.push(json!({
                            "type": "gather.result", "player_id": pid,
                            "item_id": item, "qty": 1,
                        }).to_string());
                        // Internal: the gateway persists inventory + xp and pushes
                        // the authoritative inv.update / skill.update to the client.
                        packets.push(json!({
                            "type": "gather_yield", "player_id": pid,
                            "item_id": item, "qty": 1, "skill": "gathering", "xp": GATHER_XP,
                        }).to_string());
                        if node.qty <= 0 {
                            node.respawn_timer = NODE_RESPAWN_TICKS;
                            finished.push(pid.clone()); // stop gathering a depleted node
                        }
                    }
                }
                for pid in finished {
                    gathering.remove(&pid);
                }

                // Respawn depleted nodes on their timer.
                for node in nodes.values_mut() {
                    if node.qty <= 0 && node.respawn_timer > 0 {
                        node.respawn_timer -= 1;
                        if node.respawn_timer == 0 {
                            node.qty = node.max_qty;
                            touched.insert(node.id.clone());
                        }
                    }
                }

                // Emit node state: a live node -> status_update; a depleted one -> despawn.
                for id in &touched {
                    if let Some(n) = nodes.get(id) {
                        if n.qty > 0 {
                            packets.push(node_status_json(n).to_string());
                        } else {
                            packets.push(json!({"type": "despawn", "player_id": n.id}).to_string());
                        }
                    }
                }
            }

            for id in &despawns {
                packets.push(json!({"type": "despawn", "player_id": id}).to_string());
            }
            for id in &died {
                packets.push(json!({"type": "you_died", "player_id": id}).to_string());
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
            let gathering = self.gathering.lock().unwrap();
            match entities.get(player_id) {
                Some(e) => entity_status_json(player_id, e, gathering.get(player_id)),
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
        self.spawn_nodes();
        self.spawn_storage_points();
        self.spawn_build_boards();
        self.spawn_plots();
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
        assert!(zone_for_region(Region { x0: 0, y0: 0, x1: 1600, y1: 6400 }).is_safe()); // market
        assert!(zone_for_region(Region { x0: 1600, y0: 1600, x1: 4800, y1: 4800 }).is_safe()); // civic
        assert!(zone_for_region(Region { x0: 4800, y0: 0, x1: 6400, y1: 6400 }).is_safe()); // suburbs
        assert!(zone_for_region(Region { x0: 1600, y0: 0, x1: 4800, y1: 1600 }).is_safe()); // craftworks
        assert!(zone_for_region(Region { x0: 1600, y0: 4800, x1: 4800, y1: 6400 }).is_safe()); // old_quarter
        // The default whole-world zone is safe (centre is the Civic Centre).
        assert!(zone_for_region(Region { x0: 0, y0: 0, x1: 6400, y1: 6400 }).is_safe());
        // A region whose centre falls outside the authored capital is wilds.
        assert!(!zone_for_region(Region { x0: 6600, y0: 6600, x1: 8000, y1: 8000 }).is_safe());
    }

    /// Acceptance (#5): in the safe capital, a mob sitting on top of a player deals
    /// no damage — the player's HP is untouched.
    #[tokio::test]
    async fn safe_zone_deals_no_player_damage() {
        // Civic Centre band, centred on the town centre — safe.
        let region = Region { x0: 1600, y0: 1600, x1: 4800, y1: 4800 };
        let hp = run_with_player_and_adjacent_mob(region, "p1", (3200, 3200), 8).await;
        assert_eq!(hp, PLAYER_MAX_HP, "a player took damage inside the safe capital");
    }

    /// Control: the same setup in a wilds region *does* damage the player, proving
    /// the test actually exercises the mob-aggression/damage path that #5 gates.
    #[tokio::test]
    async fn wilds_zone_damages_player() {
        // A region whose centre is outside the capital -> wilds. It still contains
        // the player's spot so the mob can reach them.
        let region = Region { x0: 6600, y0: 6600, x1: 8000, y1: 8000 };
        let hp = run_with_player_and_adjacent_mob(region, "p1", (6650, 6650), 8).await;
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
            Entity { hp: 0, ..Entity::player(3200, 3200, PLAYER_MAX_HP) },
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
    }

    // --- #7: resource gathering -----------------------------------------------

    const CIVIC: Region = Region { x0: 1600, y0: 1600, x1: 4800, y1: 4800 };
    const TREE: &str = "node_civic_tree_0"; // authored at (3140, 3140), wood, qty 5

    /// Civic zone with its authored nodes spawned and a player standing on the tree.
    fn civic_zone_on_tree() -> Arc<ZoneServer> {
        let zone = zone_for_region(CIVIC);
        zone.spawn_nodes();
        zone.entities.lock().unwrap().insert(
            "p1".to_string(),
            Entity::player(3140, 3140, PLAYER_MAX_HP),
        );
        zone
    }

    /// Run the game loop for `ticks` and return every text packet the zone emitted.
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

    fn count(packets: &[String], needle: &str) -> usize {
        packets.iter().filter(|p| p.contains(needle)).count()
    }

    #[tokio::test]
    async fn gather_yields_item_and_xp_then_continues() {
        let zone = civic_zone_on_tree();
        zone.gathering.lock().unwrap().insert(
            "p1".to_string(),
            GatherJob { node_id: TREE.to_string(), progress: 0 },
        );
        // One GATHER_PERIOD plus slack -> exactly one unit yielded.
        let packets = drive(zone.clone(), (GATHER_PERIOD as u32) + 4).await;

        assert!(count(&packets, "\"gather.progress\"") > 0, "no progress packets");
        assert_eq!(count(&packets, "\"gather.result\""), 1, "expected one yield");
        assert!(packets.iter().any(|p| p.contains("\"gather_yield\"")
            && p.contains("\"skill\":\"gathering\"") && p.contains("\"xp\":10")),
            "missing internal gather_yield with xp");
        // Node decremented but still alive; the job continues.
        assert_eq!(zone.nodes.lock().unwrap().get(TREE).unwrap().qty, 4);
        assert_eq!(count(&packets, "\"despawn\""), 0, "a live node should not despawn");
        assert!(zone.gathering.lock().unwrap().contains_key("p1"), "job should continue");
    }

    #[tokio::test]
    async fn gather_depletes_node_then_despawns_and_schedules_respawn() {
        let zone = civic_zone_on_tree();
        zone.nodes.lock().unwrap().get_mut(TREE).unwrap().qty = 1; // one unit left
        zone.gathering.lock().unwrap().insert(
            "p1".to_string(),
            GatherJob { node_id: TREE.to_string(), progress: 0 },
        );
        let packets = drive(zone.clone(), (GATHER_PERIOD as u32) + 4).await;

        assert_eq!(count(&packets, "\"gather.result\""), 1);
        assert!(packets.iter().any(|p| p.contains("\"despawn\"") && p.contains(TREE)),
            "depleted node should despawn");
        let nodes = zone.nodes.lock().unwrap();
        let n = nodes.get(TREE).unwrap();
        assert_eq!(n.qty, 0);
        assert!(n.respawn_timer > 0, "respawn should be scheduled");
        assert!(!zone.gathering.lock().unwrap().contains_key("p1"), "job ends on depletion");
    }

    #[tokio::test]
    async fn gather_out_of_range_is_cancelled() {
        let zone = civic_zone_on_tree();
        // Move the player far from the tree (still in-region, but out of gather range).
        zone.entities.lock().unwrap().get_mut("p1").unwrap().x = 3400;
        zone.gathering.lock().unwrap().insert(
            "p1".to_string(),
            GatherJob { node_id: TREE.to_string(), progress: 0 },
        );
        let packets = drive(zone.clone(), 6).await;

        assert_eq!(count(&packets, "\"gather.result\""), 0, "no yield out of range");
        assert!(!zone.gathering.lock().unwrap().contains_key("p1"), "job cancelled");
        assert_eq!(zone.nodes.lock().unwrap().get(TREE).unwrap().qty, 5, "node untouched");
    }

    #[tokio::test]
    async fn build_board_spawns_in_civic_and_gates_by_range() {
        let zone = zone_for_region(CIVIC);
        zone.spawn_build_boards();
        let boards = zone.build_boards.lock().unwrap().clone();
        assert!(!boards.is_empty(), "the civic centre has an authored build board");
        let b = boards[0];
        assert!(zone.near_board(b.x, b.y), "on the board is in range");
        assert!(zone.near_board(b.x + BOARD_RANGE - 1, b.y), "just inside range");
        assert!(!zone.near_board(b.x + BOARD_RANGE + 20, b.y), "out of range");
    }

    #[tokio::test]
    async fn plots_spawn_in_suburbs_and_gate_geometrically() {
        // Suburbs band — the only district with an authored plot grid.
        let region = Region { x0: 4800, y0: 0, x1: 6400, y1: 6400 };
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
        assert!(!civic_zone.on_a_plot(3200, 3200), "no plot grid in the civic centre");
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
