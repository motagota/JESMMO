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

const WORLD_SIZE: i32 = 1200;

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

fn entity_status_json(id: &str, e: &Entity) -> Value {
    json!({
        "type": "status_update",
        "player_id": id,
        "state": {
            "x": e.x, "y": e.y, "hp": e.hp, "max_hp": e.max_hp,
            "type": e.kind.as_str(),
            "facing": [e.facing.0, e.facing.1],
        },
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
        })
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
                // A freshly-split zone gets its own mobs within the new region.
                self.spawn_mobs(MOBS_PER_ZONE);
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
                    // A player entered our region (new spawn or migrated in).
                    let x = data.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                    let y = data.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                    let hp = data.get("hp").and_then(|v| v.as_i64()).unwrap_or(100) as i32;
                    let (x, y) = clamp_world(x, y);
                    self.entities.lock().unwrap().insert(player_id.clone(), Entity::player(x, y, hp));
                    println!("[Zone {}] Received player {player_id} at ({x}, {y})", self.zone_id);
                    self.send_status_update(&player_id).await;
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
                    // Nearest player within aggro range.
                    let mut best: Option<(String, i32, i32, i64)> = None;
                    for (pid, px, py) in &players {
                        let d2 = dist2(mx, my, *px, *py);
                        if d2 <= aggro2 && best.as_ref().map_or(true, |b| d2 < b.3) {
                            best = Some((pid.clone(), *px, *py, d2));
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

                // Apply mob contact damage to players.
                for (pid, dmg) in player_damage {
                    if let Some(e) = entities.get_mut(&pid) {
                        e.hp -= dmg;
                        changed.insert(pid);
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
                    let (rx, ry) = region.random_point();
                    if let Some(e) = entities.get_mut(id) {
                        e.hp = e.max_hp;
                        e.x = rx;
                        e.y = ry;
                    }
                    died.push(id.clone());
                    changed.insert(id.clone());
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
                let present: Vec<String> = entities
                    .iter()
                    .filter(|(_, e)| e.kind == EntityKind::Player)
                    .map(|(id, _)| id.clone())
                    .collect();
                // A single present player can make progress; 0 or 2+ (contested) cannot.
                let claimant = if present.len() == 1 { Some(present[0].clone()) } else { None };
                let mobs_clear = live_mobs <= CAPTURE_MOB_THRESHOLD;

                match (&claimant, mobs_clear) {
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

                // --- 6. Build outbound entity packets while holding the lock. ---
                for id in &changed {
                    if let Some(e) = entities.get(id) {
                        packets.push(entity_status_json(id, e).to_string());
                    }
                }
            } // entities lock released here

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

        // Seed mobs, then start the authoritative 20 Hz simulation.
        self.spawn_mobs(MOBS_PER_ZONE);
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
