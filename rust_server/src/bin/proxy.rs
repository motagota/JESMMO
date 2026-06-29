// Rust port of proxy.py
//
// Routes client websockets to registered zone servers and supports a 3-phase
// seamless zone migration driven by stdin commands.
//
//   migrate phase1 <zone_id>                    - mark zone for migration (buffer packets)
//   migrate phase2 <source_zone> <target_zone>  - transfer players
//   migrate phase3 <zone_id>                    - retire zone
//   migrate auto <source_zone> <target_zone>    - run all three phases
//
// Gateway hardening (vs. the original 1:1 port):
//   * Client (edge) connections use BOUNDED channels with try_send load-shedding,
//     so one slow/stalled client can never grow proxy memory without limit.
//   * Client connections get application-level ping/pong liveness, so half-open
//     sockets (closed laptop, dead wifi) are detected and reaped instead of
//     holding a task + fd + buffer forever.
//   * Zone and admin connections stay unbounded: they are trusted internal peers
//     with a single consumer each, where head-of-line stalling is not a DoS vector.

use std::collections::HashMap;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::Message;
use uuid::Uuid;

use mmo::auth;
use mmo::persistence::Db;
use mmo::protocol::{self, PROTOCOL_VERSION};

/// Spawn point for a brand-new character: the centre of the world.
const SPAWN_X: i32 = WORLD_SIZE / 2;
const SPAWN_Y: i32 = WORLD_SIZE / 2;
const SPAWN_HP: i32 = 100;

/// Outbound queue for a trusted internal peer (zone / admin). Unbounded is fine:
/// single consumer, head-of-line stalls are not an attack surface.
type Tx = mpsc::UnboundedSender<Message>;

/// Outbound queue for an edge client. BOUNDED so a slow client applies
/// backpressure / sheds frames instead of growing memory without limit.
type ClientTx = mpsc::Sender<Message>;

/// How many queued frames a single client may buffer before we start shedding.
/// Positional/state updates are disposable, so dropping the oldest-pending work
/// for a lagging client is the correct behaviour for a realtime sim.
const CLIENT_CHANNEL_CAP: usize = 256;

/// How often we ping each client and re-check liveness. A client that sends us
/// nothing (not even a pong) for two intervals is considered dead.
const PING_INTERVAL: Duration = Duration::from_secs(15);

/// First port used for gateway-spawned zone instances (rolling updates and
/// auto-scaling splits). Each new instance takes the next port up.
const FIRST_UPDATE_PORT: u16 = 19000;

/// Auto-scaling: a zone whose population exceeds this splits in two. Overridable
/// via the SPLIT_THRESHOLD env var.
const DEFAULT_SPLIT_THRESHOLD: usize = 5;
/// Never grow the fleet past this many zones (runaway guard).
const MAX_ZONES: usize = 8;
/// After splitting, give a zone this long to rebalance before it can split again.
const SPLIT_COOLDOWN: Duration = Duration::from_secs(8);
/// How often the auto-scaler checks zone populations.
const AUTOSCALE_INTERVAL: Duration = Duration::from_secs(2);

/// Edge length of the (square) world. Zones own rectangular sub-regions of it.
const WORLD_SIZE: i32 = 1200;

/// A half-open rectangular region of the world: [x0, x1) x [y0, y1).
#[derive(Clone, Copy)]
struct Region {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

impl Region {
    #[allow(dead_code)] // used in tests
    fn whole_world() -> Self {
        Region { x0: 0, y0: 0, x1: WORLD_SIZE, y1: WORLD_SIZE }
    }
    fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }
    /// Split along the longer axis into (low, high) halves.
    fn split(&self) -> (Region, Region) {
        if (self.x1 - self.x0) >= (self.y1 - self.y0) {
            let mid = (self.x0 + self.x1) / 2;
            (Region { x1: mid, ..*self }, Region { x0: mid, ..*self })
        } else {
            let mid = (self.y0 + self.y1) / 2;
            (Region { y1: mid, ..*self }, Region { y0: mid, ..*self })
        }
    }

    /// True if `self` and `other` share a full edge and the same span on the
    /// other axis — i.e. their union is exactly one rectangle (mergeable).
    fn mergeable_with(&self, o: &Region) -> bool {
        let side_by_side =
            self.y0 == o.y0 && self.y1 == o.y1 && (self.x1 == o.x0 || o.x1 == self.x0);
        let stacked =
            self.x0 == o.x0 && self.x1 == o.x1 && (self.y1 == o.y0 || o.y1 == self.y0);
        side_by_side || stacked
    }

    fn union(&self, o: &Region) -> Region {
        Region {
            x0: self.x0.min(o.x0),
            y0: self.y0.min(o.y0),
            x1: self.x1.max(o.x1),
            y1: self.y1.max(o.y1),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MigrationState {
    Normal,
    Marking,
    Migrating,
    Retired,
}

impl MigrationState {
    fn as_str(&self) -> &'static str {
        match self {
            MigrationState::Normal => "normal",
            MigrationState::Marking => "marking",
            MigrationState::Migrating => "migrating",
            MigrationState::Retired => "retired",
        }
    }
}

struct Zone {
    uri: String,
    tx: Tx,
    migration_state: MigrationState,
    packet_buffer: HashMap<String, Vec<Value>>,
    /// Entity count the zone reports for itself (humans + AI players alike).
    /// The zone owns the entities, so it is the authority on its own count.
    population: usize,
    /// Running version of the zone binary; bumped by each rolling update.
    version: u32,
    /// Path to the zone_server binary, for relaunching (rolling update / split).
    exe: String,
    /// The slice of the world this zone owns.
    region: Region,
    /// Territory control: which player (if any) currently owns this zone, and the
    /// 0-100 capture-bar progress. Reported by the zone server each tick.
    owner: Option<String>,
    capture_progress: f32,
}

struct ClientInfo {
    player_id: String,
    current_zone: String, // zone_id
    tx: ClientTx,
    /// True when `player_id` is a durable character backed by the database, so its
    /// position is written back on flush/disconnect. Guests are ephemeral.
    persistent: bool,
}

/// The outcome of the auth handshake: who this connection is and where to spawn.
struct Identity {
    /// Durable character id (DB) or an ephemeral `guest_*` id.
    character_id: String,
    name: String,
    x: i32,
    y: i32,
    hp: i32,
    persistent: bool,
    /// A legacy/bot client may send a gameplay frame instead of authenticating;
    /// we treat it as a guest and carry that first frame so it isn't dropped.
    pending: Option<Value>,
}

struct Proxy {
    host: String,
    port: u16,
    registration_port: u16,
    admin_port: u16,
    clients: Mutex<HashMap<String, ClientInfo>>, // keyed by player_id
    zones: Mutex<HashMap<String, Zone>>,
    zone_order: Mutex<Vec<String>>, // registration order, for picking the default zone
    /// Total frames shed across all clients due to full outbound queues.
    /// Surfaced in the admin status snapshot as a backpressure health signal.
    dropped_frames: AtomicU64,
    /// How often each client is pinged for liveness. A field (not just a const)
    /// so tests can drive the reaper on a short interval.
    ping_interval: Duration,
    /// Last position+hp the proxy saw for each entity (from status_updates),
    /// keyed by player_id. Used to recreate entities at their real position in a
    /// freshly-spawned zone instance during a rolling update.
    entity_state: Mutex<HashMap<String, (i32, i32, i32)>>,
    /// Child processes the gateway spawned (the current instance per zone id),
    /// so a later update can reap the one it replaces.
    children: Mutex<HashMap<String, Child>>,
    /// Next port to hand a gateway-spawned replacement instance.
    next_update_port: AtomicU16,
    /// Monotonic version stamped onto each rolling update.
    update_version: AtomicU32,
    /// Monotonic counter for naming auto-scaled shard zones.
    split_counter: AtomicU32,
    /// Per-zone "don't split again until" deadlines, to avoid thrashing.
    cooldowns: Mutex<HashMap<String, Instant>>,
    /// Population above which a zone auto-splits.
    split_threshold: usize,
    /// Handles for gateway-spawned load-test bots (so the admin can clear them).
    bot_handles: Mutex<Vec<JoinHandle<()>>>,
    /// Durable store for accounts/characters. `None` in unit tests (persistence
    /// no-ops) and if the DB can't be opened.
    db: Option<Arc<Db>>,
    /// Live session tokens: token -> character_id, for reconnect without re-login.
    /// In-memory and single-gateway for M0.
    sessions: Mutex<HashMap<String, String>>,
}

impl Proxy {
    fn new(
        host: &str,
        port: u16,
        registration_port: u16,
        admin_port: u16,
        db: Option<Arc<Db>>,
    ) -> Arc<Self> {
        Arc::new(Proxy {
            host: host.to_string(),
            port,
            registration_port,
            admin_port,
            clients: Mutex::new(HashMap::new()),
            zones: Mutex::new(HashMap::new()),
            zone_order: Mutex::new(Vec::new()),
            dropped_frames: AtomicU64::new(0),
            ping_interval: PING_INTERVAL,
            entity_state: Mutex::new(HashMap::new()),
            children: Mutex::new(HashMap::new()),
            next_update_port: AtomicU16::new(FIRST_UPDATE_PORT),
            update_version: AtomicU32::new(1),
            split_counter: AtomicU32::new(0),
            cooldowns: Mutex::new(HashMap::new()),
            split_threshold: std::env::var("SPLIT_THRESHOLD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_SPLIT_THRESHOLD),
            bot_handles: Mutex::new(Vec::new()),
            db,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// Pick the default landing zone for a newly connected client: the first
    /// zone (by registration order) that is still present in the zone map.
    fn pick_default_zone(&self) -> Option<String> {
        let zones = self.zones.lock().unwrap();
        let order = self.zone_order.lock().unwrap();
        order.iter().find(|z| zones.contains_key(*z)).cloned()
    }

    /// Push a message to a client's bounded outbound queue without ever blocking
    /// the caller. A full queue means the client is too slow to keep up, so we
    /// shed the frame (and account for it) rather than stall the whole broadcast.
    fn push_to_client(&self, info: &ClientInfo, msg: Message) {
        match info.tx.try_send(msg) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                let total = self.dropped_frames.fetch_add(1, Ordering::Relaxed) + 1;
                // Don't spam the log on every dropped frame for a lagging client.
                if total % 100 == 1 {
                    println!(
                        "[Proxy] Shedding frames to slow client {} (total dropped: {total})",
                        info.player_id
                    );
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Writer task is gone; the client's read loop will clean up the map entry.
            }
        }
    }

    /// Record a zone's self-reported population (humans + AI players).
    fn set_zone_population(&self, zone_id: &str, count: usize) {
        if let Some(z) = self.zones.lock().unwrap().get_mut(zone_id) {
            z.population = count;
        }
    }

    /// Build a snapshot of zones + per-zone player counts for the admin UI.
    /// Counts come from each zone's reported population, so AI players are
    /// included just like humans — every entity is a player.
    fn status_snapshot(&self) -> Value {
        let zones = self.zones.lock().unwrap();
        let order = self.zone_order.lock().unwrap();

        let mut total = 0usize;
        let zones_json: Vec<Value> = order
            .iter()
            .filter_map(|zid| {
                zones.get(zid).map(|z| {
                    total += z.population;
                    json!({
                        "zone_id": zid,
                        "uri": z.uri,
                        "migration_state": z.migration_state.as_str(),
                        "players": z.population,
                        "version": z.version,
                        "region": format!("({},{})-({},{})", z.region.x0, z.region.y0, z.region.x1, z.region.y1),
                    })
                })
            })
            .collect();

        json!({
            "type": "status",
            "total_players": total,
            "dropped_frames": self.dropped_frames.load(Ordering::Relaxed),
            "zones": zones_json,
        })
    }

    /// Admin connection: pushes a status snapshot every second and accepts
    /// migrate commands as JSON.
    async fn handle_admin(self: Arc<Self>, raw: TcpStream) {
        let ws = match tokio_tungstenite::accept_async(raw).await {
            Ok(ws) => ws,
            Err(e) => {
                println!("[Proxy] Admin handshake error: {e}");
                return;
            }
        };
        println!("[Proxy] Admin UI connected");
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

        // Writer task.
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
        });

        // Periodic status pusher.
        let push_tx = tx.clone();
        let me = self.clone();
        let pusher = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                if push_tx
                    .send(Message::Text(me.status_snapshot().to_string()))
                    .is_err()
                {
                    break;
                }
            }
        });

        // Command loop.
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
            // Load-test controls from the admin UI.
            match data.get("type").and_then(|v| v.as_str()) {
                Some("spawn_bots") => {
                    let count = data.get("count").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    let ramp = data.get("ramp_ms").and_then(|v| v.as_u64()).unwrap_or(300);
                    println!("[Proxy] Admin command: spawn {count} bots (ramp {ramp}ms)");
                    self.spawn_bots(count, ramp);
                    let _ = tx.send(Message::Text(
                        json!({"type": "ack", "message": format!("spawning {count} bots (ramp {ramp}ms)")}).to_string(),
                    ));
                    continue;
                }
                Some("clear_bots") => {
                    let n = self.clear_bots();
                    println!("[Proxy] Admin command: clear bots ({n})");
                    let _ = tx.send(Message::Text(
                        json!({"type": "ack", "message": format!("cleared {n} bots")}).to_string(),
                    ));
                    continue;
                }
                _ => {}
            }

            // Rolling-update command from the admin UI ("push an update").
            if data.get("type").and_then(|v| v.as_str()) == Some("update") {
                let target = data.get("target").and_then(|v| v.as_str()).unwrap_or("all");
                println!("[Proxy] Admin command: update {target}");
                let ack = if target == "all" {
                    self.rolling_update_all().await;
                    "rolling update of all zones complete".to_string()
                } else if self.rolling_update_one(target).await {
                    format!("rolling update of {target} complete")
                } else {
                    format!("rolling update of {target} failed")
                };
                let _ = tx.send(Message::Text(
                    json!({"type": "ack", "message": ack}).to_string(),
                ));
                continue;
            }
            if data.get("type").and_then(|v| v.as_str()) != Some("migrate") {
                continue;
            }
            let phase = data.get("phase").and_then(|v| v.as_str()).unwrap_or("");
            let source = data.get("source").and_then(|v| v.as_str()).unwrap_or("");
            let target = data.get("target").and_then(|v| v.as_str()).unwrap_or("");
            println!("[Proxy] Admin command: migrate {phase} {source} {target}");
            let ack = match phase {
                "phase1" => {
                    let ok = self.phase1_mark_for_migration(source).await;
                    format!("phase1 {source}: {}", if ok { "ok" } else { "failed" })
                }
                "phase2" => {
                    let ok = self.phase2_transfer_players(source, target).await;
                    format!("phase2 {source}->{target}: {}", if ok { "ok" } else { "failed" })
                }
                "phase3" => {
                    let ok = self.phase3_retire_zone(source).await;
                    format!("phase3 {source}: {}", if ok { "ok" } else { "failed" })
                }
                "auto" => {
                    let mut ok = self.phase1_mark_for_migration(source).await;
                    if ok {
                        sleep(Duration::from_secs(1)).await;
                        ok = self.phase2_transfer_players(source, target).await;
                        if ok {
                            sleep(Duration::from_secs(1)).await;
                            self.phase3_retire_zone(source).await;
                        }
                    }
                    format!("auto {source}->{target}: {}", if ok { "complete" } else { "failed" })
                }
                _ => format!("unknown phase: {phase}"),
            };
            let _ = tx.send(Message::Text(
                json!({"type": "ack", "message": ack}).to_string(),
            ));
        }

        pusher.abort();
        println!("[Proxy] Admin UI disconnected");
    }

    /// Connect outbound to a zone's data port, spawn its writer + listener, and
    /// return the send handle. Shared by registration and rolling updates.
    async fn connect_zone_data(self: &Arc<Self>, zone_id: String, uri: &str) -> Option<Tx> {
        let ws = match tokio_tungstenite::connect_async(uri).await {
            Ok((ws, _)) => ws,
            Err(e) => {
                println!("[Proxy] Failed to connect to zone {zone_id} at {uri}: {e}");
                return None;
            }
        };
        let (mut sink, stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

        // Writer task: serialize all outbound sends to this zone.
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
        });

        let me = self.clone();
        tokio::spawn(async move {
            me.zone_listener(zone_id, stream).await;
        });

        Some(tx)
    }

    /// Connect outbound to a freshly registered zone and add it to the map.
    async fn register_zone(
        self: &Arc<Self>,
        zone_id: String,
        uri: String,
        version: u32,
        exe: String,
        region: Region,
    ) {
        let Some(tx) = self.connect_zone_data(zone_id.clone(), &uri).await else {
            return;
        };

        {
            let mut zones = self.zones.lock().unwrap();
            zones.insert(
                zone_id.clone(),
                Zone {
                    uri: uri.clone(),
                    tx,
                    migration_state: MigrationState::Normal,
                    packet_buffer: HashMap::new(),
                    population: 0,
                    version,
                    exe,
                    region,
                    owner: None,
                    capture_progress: 0.0,
                },
            );
            self.zone_order.lock().unwrap().push(zone_id.clone());
        }

        println!("[Proxy] Registered zone {zone_id} at {uri} (v{version})");
        self.broadcast_partition();
    }

    /// Read messages coming back from a zone and route them to clients.
    async fn zone_listener<S>(self: Arc<Self>, zone_id: String, mut stream: S)
    where
        S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    {
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
            let mut data = data;
            let msg_type = data.get("type").and_then(|v| v.as_str()).map(str::to_string);
            let target_player = data.get("player_id").and_then(|v| v.as_str()).map(str::to_string);

            match msg_type.as_deref() {
                Some("status_update") => {
                    // Cache the entity's latest position so a rolling update can
                    // recreate it at the right spot in a new zone instance.
                    if let (Some(pid), Some(st)) =
                        (target_player.as_deref(), data.get("state"))
                    {
                        let x = st.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                        let y = st.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                        let hp = st.get("hp").and_then(|v| v.as_i64()).unwrap_or(100) as i32;
                        self.entity_state.lock().unwrap().insert(pid.to_string(), (x, y, hp));
                    }
                    // Stamp the owning zone and fan the update out to EVERY client,
                    // so each renders the whole world (entities carry world coords).
                    data["zone"] = json!(zone_id);
                    let tagged = data.to_string();
                    let clients = self.clients.lock().unwrap();
                    for info in clients.values() {
                        self.push_to_client(info, Message::Text(tagged.clone()));
                    }
                }
                Some("despawn") => {
                    // An entity (e.g. a killed mob) was removed. Fan out to every
                    // client so they all drop it, and clear it from the rolling-
                    // update cache so it isn't resurrected on a zone restart.
                    if let Some(pid) = target_player.as_deref() {
                        self.entity_state.lock().unwrap().remove(pid);
                    }
                    let tagged = data.to_string();
                    let clients = self.clients.lock().unwrap();
                    for info in clients.values() {
                        self.push_to_client(info, Message::Text(tagged.clone()));
                    }
                }
                Some("zone_capture") => {
                    // A zone reports its territory-control state. Store it and push a
                    // light update to all clients; if ownership flipped, also resend
                    // the partition so the canonical owner field stays correct.
                    let owner = data
                        .get("owner")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    let progress = data.get("progress").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                    let owner_changed = {
                        let mut zones = self.zones.lock().unwrap();
                        if let Some(z) = zones.get_mut(&zone_id) {
                            let changed = z.owner != owner;
                            z.owner = owner.clone();
                            z.capture_progress = progress;
                            changed
                        } else {
                            false
                        }
                    };
                    let update = json!({
                        "type": "zone_capture",
                        "zone_id": zone_id,
                        "owner": owner,
                        "progress": progress,
                    })
                    .to_string();
                    {
                        let clients = self.clients.lock().unwrap();
                        for info in clients.values() {
                            self.push_to_client(info, Message::Text(update.clone()));
                        }
                    }
                    if owner_changed {
                        self.broadcast_partition();
                    }
                }
                Some("migrate_request") => {
                    // A zone reports an entity left its region; route by position.
                    self.handle_migrate_request(&data);
                }
                Some("zone_stats") => {
                    // A zone reports its current population for the admin count.
                    let count = data.get("count").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    self.set_zone_population(&zone_id, count);
                }
                _ => {
                    // Any other player-addressed message: route to that client only.
                    if let Some(pid) = target_player {
                        let clients = self.clients.lock().unwrap();
                        for info in clients.values() {
                            if info.player_id == pid && info.current_zone == zone_id {
                                self.push_to_client(info, Message::Text(text.clone()));
                            }
                        }
                    }
                }
            }
        }
        println!("[Proxy] Zone {zone_id} disconnected");
    }

    /// A player left its zone's region at world (x, y). Find the zone that owns
    /// that point and hand the player to it, preserving exact world position so
    /// the crossing is seamless. Re-point the player's client session too.
    fn handle_migrate_request(&self, data: &Value) {
        let pid = match data.get("player_id").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => return,
        };
        let x = data.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let y = data.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let hp = data.get("hp").and_then(|v| v.as_i64()).unwrap_or(100) as i32;

        let target = match self.zone_at(x, y) {
            Some(t) => t,
            None => {
                println!("[Proxy] migrate_request: no zone owns ({x},{y}) for {pid}");
                return;
            }
        };

        let target_tx = self.zones.lock().unwrap().get(&target).map(|z| z.tx.clone());
        let Some(tx) = target_tx else { return };
        let _ = tx.send(Message::Text(
            json!({"type": "spawn_entity", "player_id": pid, "x": x, "y": y, "hp": hp}).to_string(),
        ));
        self.entity_state.lock().unwrap().insert(pid.clone(), (x, y, hp));

        // Follow the player's client session (every entity is a client).
        let mut clients = self.clients.lock().unwrap();
        if let Some(info) = clients.get_mut(&pid) {
            info.current_zone = target.clone();
            let _ = info.tx.try_send(Message::Text(
                json!({"type": "zone_migration", "zone": target}).to_string(),
            ));
        }
    }

    /// Find which zone owns world position (x, y).
    fn zone_at(&self, x: i32, y: i32) -> Option<String> {
        self.zones
            .lock()
            .unwrap()
            .iter()
            .find(|(_, z)| z.region.contains(x, y))
            .map(|(id, _)| id.clone())
    }

    /// Tell every client the current spatial partition so they can draw it.
    fn broadcast_partition(&self) {
        let zones: Vec<Value> = {
            let zones = self.zones.lock().unwrap();
            let order = self.zone_order.lock().unwrap();
            order
                .iter()
                .filter_map(|id| {
                    zones.get(id).map(|z| {
                        json!({
                            "zone_id": id,
                            "x0": z.region.x0, "y0": z.region.y0,
                            "x1": z.region.x1, "y1": z.region.y1,
                            "owner": z.owner,
                            "progress": z.capture_progress,
                        })
                    })
                })
                .collect()
        };
        let msg = Message::Text(
            json!({"type": "partition", "world": WORLD_SIZE, "zones": zones}).to_string(),
        );
        let clients = self.clients.lock().unwrap();
        for info in clients.values() {
            self.push_to_client(info, msg.clone());
        }
    }

    /// Registration service: zones connect here to announce themselves.
    async fn handle_zone_registration(self: Arc<Self>, raw: TcpStream) {
        let ws = match tokio_tungstenite::accept_async(raw).await {
            Ok(ws) => ws,
            Err(e) => {
                println!("[Proxy] Zone registration handshake error: {e}");
                return;
            }
        };
        let (_sink, mut stream) = ws.split();
        while let Some(Ok(msg)) = stream.next().await {
            let text = match msg {
                Message::Text(t) => t,
                _ => continue,
            };
            let data: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if data.get("type").and_then(|v| v.as_str()) == Some("register_zone") {
                let zone_id = data.get("zone_id").and_then(|v| v.as_str());
                let uri = data.get("uri").and_then(|v| v.as_str());
                let (zone_id, uri) = match (zone_id, uri) {
                    (Some(z), Some(u)) => (z.to_string(), u.to_string()),
                    _ => {
                        println!("[Proxy] Invalid zone registration payload: {data}");
                        continue;
                    }
                };
                let version = data.get("version").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
                let exe = data.get("exe").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let g = |k: &str, d: i32| data.get(k).and_then(|v| v.as_i64()).unwrap_or(d as i64) as i32;
                let region = Region {
                    x0: g("x0", 0),
                    y0: g("y0", 0),
                    x1: g("x1", WORLD_SIZE),
                    y1: g("y1", WORLD_SIZE),
                };
                let already = self.zones.lock().unwrap().contains_key(&zone_id);
                if !already {
                    self.register_zone(zone_id.clone(), uri.clone(), version, exe, region).await;
                    println!("[Proxy] Zone {zone_id} self-registered from {uri}");
                } else {
                    // A replacement instance from a rolling update re-registers with
                    // the same id; the gateway already wired its data connection, so
                    // just ignore the duplicate self-registration.
                    println!("[Proxy] Zone {zone_id} already registered (ignoring duplicate)");
                }
            }
        }
    }

    async fn phase1_mark_for_migration(&self, zone_id: &str) -> bool {
        if !self.zones.lock().unwrap().contains_key(zone_id) {
            println!("[Proxy] Zone {zone_id} not found");
            return false;
        }

        // Players currently in this zone.
        let players: Vec<String> = self
            .clients
            .lock()
            .unwrap()
            .values()
            .filter(|i| i.current_zone == zone_id)
            .map(|i| i.player_id.clone())
            .collect();

        let mut zones = self.zones.lock().unwrap();
        let zone = zones.get_mut(zone_id).unwrap();
        zone.migration_state = MigrationState::Marking;
        println!(
            "[Proxy] PHASE 1: Zone {zone_id} marked for migration - buffering client packets"
        );
        for pid in players {
            println!("[Proxy] Buffering enabled for player {pid}");
            zone.packet_buffer.insert(pid, Vec::new());
        }
        true
    }

    async fn phase2_transfer_players(&self, source_zone_id: &str, target_zone_id: &str) -> bool {
        {
            let zones = self.zones.lock().unwrap();
            if !zones.contains_key(source_zone_id) || !zones.contains_key(target_zone_id) {
                println!("[Proxy] Source or target zone not found");
                return false;
            }
        }
        self.zones
            .lock()
            .unwrap()
            .get_mut(source_zone_id)
            .unwrap()
            .migration_state = MigrationState::Migrating;
        println!("[Proxy] PHASE 2: Transferring players from {source_zone_id} to {target_zone_id}");

        let players: Vec<String> = self
            .clients
            .lock()
            .unwrap()
            .values()
            .filter(|i| i.current_zone == source_zone_id)
            .map(|i| i.player_id.clone())
            .collect();

        for player_id in players {
            // Notify source zone of leave.
            if let Some(tx) = self
                .zones
                .lock()
                .unwrap()
                .get(source_zone_id)
                .map(|z| z.tx.clone())
            {
                let _ = tx.send(Message::Text(
                    json!({"type": "player_leave", "player_id": player_id}).to_string(),
                ));
            }
            println!("[Proxy] Player {player_id} leaving {source_zone_id}");

            // Give the source zone time to clean up.
            sleep(Duration::from_millis(100)).await;

            // Pop buffered packets.
            let buffered: Vec<Value> = self
                .zones
                .lock()
                .unwrap()
                .get_mut(source_zone_id)
                .and_then(|z| z.packet_buffer.remove(&player_id))
                .unwrap_or_default();

            // Update the client's zone reference.
            if let Some(info) = self.clients.lock().unwrap().get_mut(&player_id) {
                info.current_zone = target_zone_id.to_string();
            }

            // Notify target zone of join, then replay buffered packets.
            if let Some(tx) = self
                .zones
                .lock()
                .unwrap()
                .get(target_zone_id)
                .map(|z| z.tx.clone())
            {
                let _ = tx.send(Message::Text(
                    json!({"type": "player_join", "player_id": player_id}).to_string(),
                ));
                println!("[Proxy] Player {player_id} joined {target_zone_id}");
                for buffered_msg in buffered {
                    let _ = tx.send(Message::Text(buffered_msg.to_string()));
                    println!("[Proxy] Replayed buffered packet for {player_id}");
                }
            }

            // Notify the client of the migration. This is a control-plane message,
            // so a full queue here means the client is already in trouble; we still
            // only try_send to avoid stalling the migration loop.
            if let Some(info_tx) = self.clients.lock().unwrap().get(&player_id).map(|i| i.tx.clone())
            {
                let _ = info_tx.try_send(Message::Text(
                    json!({
                        "type": "zone_migration",
                        "zone": target_zone_id,
                        "message": format!("Migrated to {target_zone_id}")
                    })
                    .to_string(),
                ));
            }
        }
        true
    }

    async fn phase3_retire_zone(&self, zone_id: &str) -> bool {
        let mut zones = self.zones.lock().unwrap();
        let zone = match zones.get_mut(zone_id) {
            Some(z) => z,
            None => {
                println!("[Proxy] Zone {zone_id} not found");
                return false;
            }
        };
        zone.migration_state = MigrationState::Retired;
        println!("[Proxy] PHASE 3: Zone {zone_id} retired");
        // Closing the channel ends the writer task, which drops the socket.
        let _ = zone.tx.send(Message::Close(None));
        println!("[Proxy] Closed connection to {zone_id}");
        true
    }

    /// Seamlessly roll a single zone onto a fresh (updated) instance with no
    /// client disconnects:
    ///   1. mark the zone so client packets buffer (no input lost),
    ///   2. spawn a new zone process (same id, new port, bumped version),
    ///   3. recreate every entity in it at its last-known position,
    ///   4. swap routing to the new instance and replay buffered packets,
    ///   5. shut the old instance down.
    /// Clients keep their socket and their zone id throughout.
    async fn rolling_update(self: &Arc<Self>, zone_id: &str, version: u32) -> bool {
        let (exe, region, old_tx) = match self.zones.lock().unwrap().get(zone_id) {
            Some(z) => (z.exe.clone(), z.region, z.tx.clone()),
            None => {
                println!("[Proxy] update: zone {zone_id} not found");
                return false;
            }
        };
        if exe.is_empty() {
            println!("[Proxy] update: zone {zone_id} has no launch spec (started without one?)");
            return false;
        }
        println!("[Proxy] ROLLING UPDATE: {zone_id} -> v{version}");

        // 1. Mark the zone: client packets now buffer instead of going to a zone
        //    that's about to be torn down.
        let players: Vec<String> = self
            .clients
            .lock()
            .unwrap()
            .values()
            .filter(|i| i.current_zone == zone_id)
            .map(|i| i.player_id.clone())
            .collect();
        {
            let mut zones = self.zones.lock().unwrap();
            if let Some(z) = zones.get_mut(zone_id) {
                z.migration_state = MigrationState::Marking;
                for p in &players {
                    z.packet_buffer.entry(p.clone()).or_default();
                }
            }
        }

        // 2. Spawn the replacement process (same id, new port, bumped version).
        //    No proxy URI: gateway-spawned instances don't self-register; the
        //    gateway connects out to them and already knows their spec.
        let new_port = self.next_update_port.fetch_add(1, Ordering::SeqCst);
        let mut cmd = Command::new(&exe);
        cmd.arg(zone_id).arg(new_port.to_string());
        cmd.arg("--region")
            .arg(region.x0.to_string())
            .arg(region.y0.to_string())
            .arg(region.x1.to_string())
            .arg(region.y1.to_string());
        cmd.env("ZONE_VERSION", version.to_string());
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                println!("[Proxy] update: failed to spawn {zone_id}: {e}");
                self.set_zone_population(zone_id, 0); // best-effort; leave state
                if let Some(z) = self.zones.lock().unwrap().get_mut(zone_id) {
                    z.migration_state = MigrationState::Normal;
                }
                return false;
            }
        };

        // 3. Connect to the new instance's data port (retry until it's listening).
        let new_uri = format!("ws://127.0.0.1:{}", new_port);
        let mut new_tx = None;
        for _ in 0..50 {
            if let Some(tx) = self.connect_zone_data(zone_id.to_string(), &new_uri).await {
                new_tx = Some(tx);
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
        let Some(new_tx) = new_tx else {
            println!("[Proxy] update: could not reach new {zone_id} at {new_uri}, aborting");
            if let Some(z) = self.zones.lock().unwrap().get_mut(zone_id) {
                z.migration_state = MigrationState::Normal;
            }
            return false;
        };

        // 4. Recreate every entity in the new instance at its cached position.
        for p in &players {
            let (x, y, hp) = self
                .entity_state
                .lock()
                .unwrap()
                .get(p)
                .copied()
                .unwrap_or((WORLD_SIZE / 2, WORLD_SIZE / 2, 100));
            let _ = new_tx.send(Message::Text(
                json!({"type": "spawn_entity", "player_id": p, "x": x, "y": y, "hp": hp})
                    .to_string(),
            ));
        }

        // 5. Swap routing to the new instance, mark Normal, and collect buffered
        //    client packets to replay.
        let buffered = {
            let mut zones = self.zones.lock().unwrap();
            if let Some(z) = zones.get_mut(zone_id) {
                z.tx = new_tx.clone();
                z.uri = new_uri;
                z.version = version;
                z.migration_state = MigrationState::Normal;
                std::mem::take(&mut z.packet_buffer)
            } else {
                HashMap::new()
            }
        };
        for (_pid, pkts) in buffered {
            for pkt in pkts {
                let _ = new_tx.send(Message::Text(pkt.to_string()));
            }
        }

        // 6. Shut the old instance down, and reap the previous gateway-spawned
        //    child for this zone (if any).
        let _ = old_tx.send(Message::Text(json!({"type": "shutdown"}).to_string()));
        if let Some(mut prev) = self.children.lock().unwrap().insert(zone_id.to_string(), child) {
            let _ = prev.kill();
            let _ = prev.wait();
        }

        println!("[Proxy] ROLLING UPDATE complete: {zone_id} now v{version} ({} entities kept)", players.len());
        true
    }

    /// Roll a single zone, allocating it the next version ("push" one zone).
    async fn rolling_update_one(self: &Arc<Self>, zone_id: &str) -> bool {
        let version = self.update_version.fetch_add(1, Ordering::SeqCst) + 1;
        self.rolling_update(zone_id, version).await
    }

    /// Roll every registered zone, one at a time, so the world stays up. The
    /// whole fleet is stamped with a single version (one deploy = one version).
    async fn rolling_update_all(self: &Arc<Self>) {
        let version = self.update_version.fetch_add(1, Ordering::SeqCst) + 1;
        let ids: Vec<String> = self.zone_order.lock().unwrap().clone();
        for id in ids {
            if self.zones.lock().unwrap().contains_key(&id) {
                self.rolling_update(&id, version).await;
                sleep(Duration::from_millis(500)).await;
            }
        }
        println!("[Proxy] Fleet rolling update complete (v{version})");
    }

    /// Shrink/retarget a running zone's region (and keep our record in sync).
    fn set_zone_region(&self, zone_id: &str, region: Region) {
        let mut zones = self.zones.lock().unwrap();
        if let Some(z) = zones.get_mut(zone_id) {
            z.region = region;
            let _ = z.tx.send(Message::Text(
                json!({
                    "type": "set_region",
                    "x0": region.x0, "y0": region.y0, "x1": region.x1, "y1": region.y1,
                })
                .to_string(),
            ));
        }
    }

    /// Background auto-scaler. Each tick: split the most overpopulated zone if
    /// any is over the threshold; otherwise merge an under-used adjacent pair.
    /// One action per tick keeps the partition from thrashing.
    async fn autoscale_monitor(self: Arc<Self>) {
        loop {
            sleep(AUTOSCALE_INTERVAL).await;
            let now = Instant::now();

            let infos: Vec<(String, Region, usize)> = self
                .zones
                .lock()
                .unwrap()
                .iter()
                .map(|(id, z)| (id.clone(), z.region, z.population))
                .collect();
            let cooling = |id: &str| {
                self.cooldowns.lock().unwrap().get(id).is_some_and(|t| *t > now)
            };

            // 1. Split the most overpopulated zone (if room in the fleet).
            if infos.len() < MAX_ZONES {
                let mut best: Option<(&str, usize)> = None;
                for (id, _, pop) in &infos {
                    if *pop > self.split_threshold && !cooling(id) {
                        if best.as_ref().is_none_or(|(_, bp)| *pop > *bp) {
                            best = Some((id, *pop));
                        }
                    }
                }
                if let Some((id, pop)) = best {
                    println!(
                        "[Proxy] AUTOSCALE: {id} overpopulated ({pop} > {}), splitting",
                        self.split_threshold
                    );
                    self.split_zone(id).await;
                    continue;
                }
            }

            // 2. Otherwise merge an under-used adjacent pair whose combined
            //    population stays at/under the threshold (so it won't re-split).
            'find: for i in 0..infos.len() {
                for j in (i + 1)..infos.len() {
                    let (a, b) = (&infos[i], &infos[j]);
                    if a.1.mergeable_with(&b.1)
                        && a.2 + b.2 <= self.split_threshold
                        && !cooling(&a.0)
                        && !cooling(&b.0)
                    {
                        // Keep the lower-origin zone; retire the other.
                        let (keep, drop) = if (a.1.x0, a.1.y0) <= (b.1.x0, b.1.y0) {
                            (&a.0, &b.0)
                        } else {
                            (&b.0, &a.0)
                        };
                        println!(
                            "[Proxy] AUTOSCALE: merging {drop} ({}) into {keep} ({}) — under-used",
                            if keep == &a.0 { b.2 } else { a.2 },
                            if keep == &a.0 { a.2 } else { b.2 }
                        );
                        self.merge_zones(keep, drop);
                        break 'find;
                    }
                }
            }
        }
    }

    /// Split an overpopulated zone in space: halve its region along the longer
    /// axis, spawn a new zone for the far half, and migrate the players who are
    /// in that half into it. The gateway routes by position, so no neighbour
    /// wiring is needed — density genuinely drops because each zone now owns a
    /// smaller area.
    async fn split_zone(self: &Arc<Self>, zone_id: &str) -> bool {
        let (exe, region, old_tx, version) = match self.zones.lock().unwrap().get(zone_id) {
            Some(z) => (z.exe.clone(), z.region, z.tx.clone(), z.version),
            None => return false,
        };
        if exe.is_empty() {
            println!("[Proxy] split: zone {zone_id} has no launch spec");
            return false;
        }

        let (keep, give) = region.split();
        if (give.x1 - give.x0) < 2 || (give.y1 - give.y0) < 2 {
            return false; // region too small to subdivide further
        }

        // Players currently in this zone, with cached world positions.
        let players: Vec<(String, i32, i32, i32)> = {
            let clients = self.clients.lock().unwrap();
            let state = self.entity_state.lock().unwrap();
            clients
                .values()
                .filter(|i| i.current_zone == zone_id)
                .map(|i| {
                    let (x, y, hp) = state.get(&i.player_id).copied().unwrap_or((region.x0, region.y0, 100));
                    (i.player_id.clone(), x, y, hp)
                })
                .collect()
        };

        let n = self.split_counter.fetch_add(1, Ordering::SeqCst) + 1;
        let new_id = format!("{zone_id}-{n}");
        let new_port = self.next_update_port.fetch_add(1, Ordering::SeqCst);

        // Spawn the new zone owning the `give` half. No proxy URI: the gateway
        // connects out to it rather than having it self-register.
        let mut cmd = Command::new(&exe);
        cmd.arg(&new_id).arg(new_port.to_string());
        cmd.arg("--region")
            .arg(give.x0.to_string())
            .arg(give.y0.to_string())
            .arg(give.x1.to_string())
            .arg(give.y1.to_string());
        cmd.env("ZONE_VERSION", version.to_string());
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                println!("[Proxy] split: failed to spawn {new_id}: {e}");
                return false;
            }
        };

        // Connect to the new zone's data port.
        let new_uri = format!("ws://127.0.0.1:{}", new_port);
        let mut new_tx = None;
        for _ in 0..50 {
            if let Some(tx) = self.connect_zone_data(new_id.clone(), &new_uri).await {
                new_tx = Some(tx);
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
        let Some(new_tx) = new_tx else {
            println!("[Proxy] split: could not reach new zone {new_id}");
            return false;
        };

        // Register the new zone (owning `give`) and add it to the order.
        {
            let mut zones = self.zones.lock().unwrap();
            zones.insert(
                new_id.clone(),
                Zone {
                    uri: new_uri,
                    tx: new_tx.clone(),
                    migration_state: MigrationState::Normal,
                    packet_buffer: HashMap::new(),
                    population: 0,
                    version,
                    exe: exe.clone(),
                    region: give,
                    owner: None,
                    capture_progress: 0.0,
                },
            );
            self.zone_order.lock().unwrap().push(new_id.clone());
        }

        // Shrink the original zone to the `keep` half.
        self.set_zone_region(zone_id, keep);

        // Migrate the players who now fall in the `give` half, at their exact
        // world position (seamless — no teleport).
        let mut moved = 0;
        for (pid, x, y, hp) in &players {
            if !give.contains(*x, *y) {
                continue;
            }
            let _ = old_tx.send(Message::Text(
                json!({"type": "player_leave", "player_id": pid}).to_string(),
            ));
            let _ = new_tx.send(Message::Text(
                json!({"type": "spawn_entity", "player_id": pid, "x": x, "y": y, "hp": hp})
                    .to_string(),
            ));
            if let Some(info) = self.clients.lock().unwrap().get_mut(pid) {
                info.current_zone = new_id.clone();
                let _ = info.tx.try_send(Message::Text(
                    json!({"type": "zone_migration", "zone": new_id}).to_string(),
                ));
            }
            moved += 1;
        }

        self.children.lock().unwrap().insert(new_id.clone(), child);
        {
            let until = Instant::now() + SPLIT_COOLDOWN;
            let mut cd = self.cooldowns.lock().unwrap();
            cd.insert(zone_id.to_string(), until);
            cd.insert(new_id.clone(), until);
        }
        self.broadcast_partition();

        println!(
            "[Proxy] SPLIT: {zone_id} region halved; new zone {new_id} ({},{})-({},{}) took {moved} players",
            give.x0, give.y0, give.x1, give.y1
        );
        true
    }

    /// Merge two adjacent zones: `keep` absorbs `drop`'s region and players, and
    /// `drop` is retired. The inverse of a split; reclaims an under-used server.
    fn merge_zones(&self, keep_id: &str, drop_id: &str) {
        let (keep_tx, keep_region, drop_tx, drop_region) = {
            let zones = self.zones.lock().unwrap();
            match (zones.get(keep_id), zones.get(drop_id)) {
                (Some(k), Some(d)) => (k.tx.clone(), k.region, d.tx.clone(), d.region),
                _ => return,
            }
        };
        let union = keep_region.union(&drop_region);

        // Players to move out of the retiring zone, with their world positions.
        let movers: Vec<(String, i32, i32, i32)> = {
            let clients = self.clients.lock().unwrap();
            let state = self.entity_state.lock().unwrap();
            clients
                .values()
                .filter(|i| i.current_zone == drop_id)
                .map(|i| {
                    let (x, y, hp) =
                        state.get(&i.player_id).copied().unwrap_or((union.x0, union.y0, 100));
                    (i.player_id.clone(), x, y, hp)
                })
                .collect()
        };

        // Atomically: keep grows to the union, drop disappears (no overlap/gap).
        {
            let mut zones = self.zones.lock().unwrap();
            if let Some(k) = zones.get_mut(keep_id) {
                k.region = union;
            }
            zones.remove(drop_id);
            self.zone_order.lock().unwrap().retain(|z| z != drop_id);
        }
        // Tell the surviving zone process its new (bigger) region.
        let _ = keep_tx.send(Message::Text(
            json!({
                "type": "set_region",
                "x0": union.x0, "y0": union.y0, "x1": union.x1, "y1": union.y1,
            })
            .to_string(),
        ));

        // Move the retiring zone's players into the survivor at their positions.
        for (pid, x, y, hp) in &movers {
            let _ = keep_tx.send(Message::Text(
                json!({"type": "spawn_entity", "player_id": pid, "x": x, "y": y, "hp": hp})
                    .to_string(),
            ));
            if let Some(info) = self.clients.lock().unwrap().get_mut(pid) {
                info.current_zone = keep_id.to_string();
                let _ = info.tx.try_send(Message::Text(
                    json!({"type": "zone_migration", "zone": keep_id}).to_string(),
                ));
            }
        }

        // Retire the drained zone.
        let _ = drop_tx.send(Message::Text(json!({"type": "shutdown"}).to_string()));
        if let Some(mut c) = self.children.lock().unwrap().remove(drop_id) {
            let _ = c.kill();
            let _ = c.wait();
        }

        self.cooldowns
            .lock()
            .unwrap()
            .insert(keep_id.to_string(), Instant::now() + SPLIT_COOLDOWN);
        self.broadcast_partition();

        println!(
            "[Proxy] MERGE: {drop_id} folded into {keep_id} -> ({},{})-({},{}), {} players moved",
            union.x0, union.y0, union.x1, union.y1, movers.len()
        );
    }

    /// Spawn `count` load-test bots that connect to our own client port and
    /// wander, staggered by `ramp_ms` so the population ramps up. Driven from the
    /// admin UI to watch auto-scaling live.
    fn spawn_bots(self: &Arc<Self>, count: usize, ramp_ms: u64) {
        let me = self.clone();
        let uri = format!("ws://{}:{}", self.host, self.port);
        tokio::spawn(async move {
            for _ in 0..count {
                let u = uri.clone();
                let handle = tokio::spawn(async move { run_internal_bot(u).await });
                me.bot_handles.lock().unwrap().push(handle);
                if ramp_ms > 0 {
                    sleep(Duration::from_millis(ramp_ms)).await;
                }
            }
        });
    }

    /// Disconnect all gateway-spawned bots (their sockets drop, so the zones
    /// drain and merge back down).
    fn clear_bots(&self) -> usize {
        let mut handles = self.bot_handles.lock().unwrap();
        let n = handles.len();
        for h in handles.drain(..) {
            h.abort();
        }
        n
    }

    /// Stamp the proxy-assigned id onto a client frame and route it to the
    /// player's current zone (buffering instead if that zone is mid-migration).
    /// Returns false if the client is no longer tracked (caller should stop).
    fn route_client_frame(&self, player_id: &str, mut data: Value) -> bool {
        // Never trust a client-supplied player_id.
        data["player_id"] = json!(player_id);
        let current_zone_id = match self
            .clients
            .lock()
            .unwrap()
            .get(player_id)
            .map(|i| i.current_zone.clone())
        {
            Some(z) => z,
            None => return false,
        };
        let mut zones = self.zones.lock().unwrap();
        if let Some(zone) = zones.get_mut(&current_zone_id) {
            if zone.migration_state == MigrationState::Marking {
                let type_str = data
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                zone.packet_buffer
                    .entry(player_id.to_string())
                    .or_default()
                    .push(data);
                println!("[Proxy] Buffered packet for {player_id}: {type_str}");
            } else {
                let _ = zone.tx.send(Message::Text(data.to_string()));
            }
        }
        true
    }

    /// Drive the auth handshake on a freshly connected client. Sends
    /// `auth_required`, then resolves register / login / token / guest (allowing
    /// retries on failure, up to a small cap). A non-auth first frame is treated as
    /// a guest, with that frame carried back so it isn't lost — this keeps the
    /// legacy 2D client and the load-test bots working without modification.
    async fn run_handshake<S>(&self, tx: &ClientTx, stream: &mut S) -> Option<Identity>
    where
        S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    {
        let _ = tx
            .send(Message::Text(
                json!({"type": protocol::S_AUTH_REQUIRED, "protocol_version": PROTOCOL_VERSION})
                    .to_string(),
            ))
            .await;

        let mut attempts = 0u32;
        loop {
            let frame = match tokio::time::timeout(Duration::from_secs(120), stream.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => t,
                // Closed, errored, or no auth within the window: give up on this socket.
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) | Err(_) => {
                    return None
                }
                Ok(Some(Ok(_))) => continue, // ping/pong/binary: ignore, keep waiting
            };
            let data: Value = match serde_json::from_str(&frame) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let kind = data.get("type").and_then(|v| v.as_str()).unwrap_or("");

            let result: Result<Identity, auth::AuthError> = if kind == protocol::C_GUEST {
                Ok(guest_identity(None))
            } else if kind == protocol::C_REGISTER || kind == protocol::C_LOGIN {
                let email = data.get("email").and_then(|v| v.as_str()).unwrap_or("");
                let password = data.get("password").and_then(|v| v.as_str()).unwrap_or("");
                match &self.db {
                    // No database configured: fall back to a guest session.
                    None => return Some(guest_identity(None)),
                    Some(db) => {
                        if kind == protocol::C_REGISTER {
                            let name = data.get("name").and_then(|v| v.as_str()).unwrap_or("");
                            auth::register(
                                db, email, password, name,
                                SPAWN_X as i64, SPAWN_Y as i64, SPAWN_HP as i64,
                            )
                            .await
                            .map(persistent_identity)
                        } else {
                            auth::login(db, email, password).await.map(persistent_identity)
                        }
                    }
                }
            } else if kind == protocol::C_TOKEN {
                let token = data.get("token").and_then(|v| v.as_str()).unwrap_or("");
                match self.resume_token(token).await {
                    Some(id) => Ok(id),
                    None => Err(auth::AuthError::InvalidCredentials),
                }
            } else {
                // Legacy/bot client sent a gameplay frame: guest, carrying the frame.
                return Some(guest_identity(Some(data)));
            };

            match result {
                Ok(identity) => {
                    if identity.persistent {
                        // Mint a session token and hand it back for reconnect.
                        let token = Uuid::new_v4().to_string();
                        self.sessions
                            .lock()
                            .unwrap()
                            .insert(token.clone(), identity.character_id.clone());
                        let _ = tx
                            .send(Message::Text(
                                json!({"type": protocol::S_AUTH_OK,
                                       "player_id": identity.character_id.clone(),
                                       "name": identity.name.clone(),
                                       "token": token})
                                .to_string(),
                            ))
                            .await;
                    }
                    return Some(identity);
                }
                Err(e) => {
                    println!("[Proxy] Auth failed ({kind}): {e:?}");
                    let _ = tx
                        .send(Message::Text(
                            json!({"type": protocol::S_AUTH_ERROR, "message": e.message()})
                                .to_string(),
                        ))
                        .await;
                    attempts += 1;
                    if attempts >= 5 {
                        return None;
                    }
                }
            }
        }
    }

    /// Resume a session from a previously issued token (reconnect without re-login).
    async fn resume_token(&self, token: &str) -> Option<Identity> {
        let character_id = self.sessions.lock().unwrap().get(token).cloned()?;
        let db = self.db.as_ref()?;
        let ch = db.character_by_id(&character_id).await.ok()??;
        Some(persistent_identity(ch))
    }

    /// Periodically persist every connected durable character's last-known state,
    /// so an unclean shutdown loses at most one interval of movement.
    async fn persistence_flush(self: Arc<Self>) {
        let db = match &self.db {
            Some(db) => db.clone(),
            None => return,
        };
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        interval.tick().await; // consume the immediate first tick
        loop {
            interval.tick().await;
            let targets: Vec<(String, String, i32, i32, i32)> = {
                let clients = self.clients.lock().unwrap();
                let state = self.entity_state.lock().unwrap();
                clients
                    .values()
                    .filter(|i| i.persistent)
                    .filter_map(|i| {
                        state
                            .get(&i.player_id)
                            .map(|&(x, y, hp)| (i.player_id.clone(), i.current_zone.clone(), x, y, hp))
                    })
                    .collect()
            };
            for (id, district, x, y, hp) in targets {
                let _ = db
                    .save_character(&id, x as i64, y as i64, hp as i64, &district)
                    .await;
            }
        }
    }

    async fn handle_client(self: Arc<Self>, raw: TcpStream) {
        let ws = match tokio_tungstenite::accept_async(raw).await {
            Ok(ws) => ws,
            Err(e) => {
                println!("[Proxy] Client handshake error: {e}");
                return;
            }
        };

        // Bounded outbound queue (backpressure / load-shedding) + writer task,
        // wired up before the handshake so we can talk to the client during it.
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::channel::<Message>(CLIENT_CHANNEL_CAP);
        // Separate handle used by the liveness pinger (writes go through the one writer task).
        let ping_tx = tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
        });

        // Authenticate: register / login / token / guest. Resolves a durable (or
        // ephemeral) identity and where the player should spawn.
        let identity = match self.run_handshake(&tx, &mut stream).await {
            Some(id) => id,
            None => return, // disconnected or gave up before authenticating
        };
        let player_id = identity.character_id.clone();

        // One active session per character: refuse a duplicate login.
        if self.clients.lock().unwrap().contains_key(&player_id) {
            let _ = tx
                .send(Message::Text(
                    json!({"type": protocol::S_AUTH_ERROR, "message": "this character is already online"})
                        .to_string(),
                ))
                .await;
            return;
        }

        // Wait up to 5s for at least one zone to be available.
        let wait_start = Instant::now();
        while self.zones.lock().unwrap().is_empty()
            && wait_start.elapsed() < Duration::from_secs(5)
        {
            sleep(Duration::from_millis(100)).await;
        }
        let default_zone_id = match self.pick_default_zone() {
            Some(z) => z,
            None => {
                println!("[Proxy] Rejecting client because no zones are registered");
                let _ = tx
                    .send(Message::Text(
                        json!({"type": protocol::S_AUTH_ERROR, "message": "no zones available"})
                            .to_string(),
                    ))
                    .await;
                return;
            }
        };

        // A returning character spawns in whichever zone owns its saved position;
        // a fresh character/guest lands in the default zone.
        let spawn_zone_id = if identity.persistent {
            self.zone_at(identity.x, identity.y)
                .unwrap_or_else(|| default_zone_id.clone())
        } else {
            default_zone_id.clone()
        };

        self.clients.lock().unwrap().insert(
            player_id.clone(),
            ClientInfo {
                player_id: player_id.clone(),
                current_zone: spawn_zone_id.clone(),
                tx,
                persistent: identity.persistent,
            },
        );

        // Tell the client its assigned id, zone, and the protocol version.
        let _ = ping_tx.try_send(Message::Text(
            json!({
                "type": protocol::S_WELCOME,
                "player_id": player_id,
                "zone": spawn_zone_id,
                "protocol_version": PROTOCOL_VERSION,
                "name": identity.name.clone(),
            })
            .to_string(),
        ));
        // Send the current world partition so the client can draw the zones.
        self.broadcast_partition();

        // Spawn into the world: a returning character is recreated at its exact
        // saved position; a guest/new player joins normally (the zone picks a point).
        {
            let zones = self.zones.lock().unwrap();
            if let Some(zone) = zones.get(&spawn_zone_id) {
                if identity.persistent {
                    self.entity_state
                        .lock()
                        .unwrap()
                        .insert(player_id.clone(), (identity.x, identity.y, identity.hp));
                    let _ = zone.tx.send(Message::Text(
                        json!({"type": "spawn_entity", "player_id": player_id,
                               "x": identity.x, "y": identity.y, "hp": identity.hp})
                        .to_string(),
                    ));
                } else {
                    let _ = zone.tx.send(Message::Text(
                        json!({"type": "player_join", "player_id": player_id}).to_string(),
                    ));
                }
            }
        }
        println!(
            "[Proxy] Client connected: {player_id} ({}) -> {spawn_zone_id}",
            if identity.persistent { "account" } else { "guest" }
        );

        // A legacy/bot client may have sent a gameplay frame during the handshake;
        // route it now so nothing is lost.
        if let Some(frame) = identity.pending.clone() {
            self.route_client_frame(&player_id, frame);
        }

        // Liveness: ping on an interval; if a full interval passes with no frame
        // at all from the client (not even a pong), treat the socket as dead.
        let mut ping_interval = tokio::time::interval(self.ping_interval);
        ping_interval.tick().await; // consume the immediate first tick
        let mut awaiting_pong = false;

        loop {
            tokio::select! {
                maybe = stream.next() => {
                    let msg = match maybe {
                        Some(Ok(m)) => m,
                        _ => break, // closed or errored
                    };
                    // Any frame (text, pong, ping, binary) proves the client is alive.
                    awaiting_pong = false;

                    let text = match msg {
                        Message::Text(t) => t,
                        Message::Close(_) => break,
                        _ => continue, // pong/ping/binary: liveness already recorded
                    };
                    let data: Value = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    // Route to the player's zone (or buffer if mid-migration). A
                    // false result means the client is no longer tracked.
                    if !self.route_client_frame(&player_id, data) {
                        break;
                    }
                }
                _ = ping_interval.tick() => {
                    if awaiting_pong {
                        // No frame arrived during the whole interval after our ping.
                        println!("[Proxy] Client {player_id} failed liveness check, disconnecting");
                        break;
                    }
                    // Route the ping through the writer task to keep a single writer.
                    if ping_tx.try_send(Message::Ping(Vec::new())).is_err() {
                        break; // writer gone or queue full -> treat as dead
                    }
                    awaiting_pong = true;
                }
            }
        }

        // Cleanup on disconnect. Persist the character's last-known state first so
        // a logout (or crash) restores it on next login.
        let last_state = self.entity_state.lock().unwrap().remove(&player_id);
        let info = self.clients.lock().unwrap().remove(&player_id);
        if let Some(info) = info {
            if info.persistent {
                if let Some(db) = &self.db {
                    let (x, y, hp) = last_state.unwrap_or((identity.x, identity.y, identity.hp));
                    match db
                        .save_character(&player_id, x as i64, y as i64, hp as i64, &info.current_zone)
                        .await
                    {
                        Ok(()) => println!("[Proxy] Persisted {player_id} at ({x},{y}) hp {hp}"),
                        Err(e) => println!("[Proxy] Failed to persist {player_id}: {e}"),
                    }
                }
            }
            let zones = self.zones.lock().unwrap();
            if let Some(zone) = zones.get(&info.current_zone) {
                let _ = zone.tx.send(Message::Text(
                    json!({"type": "player_leave", "player_id": player_id}).to_string(),
                ));
                println!(
                    "[Proxy] Client disconnected: {player_id} from {}",
                    info.current_zone
                );
            }
        }

        // Tell remaining clients to stop rendering this entity.
        let despawn = Message::Text(
            json!({"type": "despawn", "player_id": player_id}).to_string(),
        );
        let clients = self.clients.lock().unwrap();
        for c in clients.values() {
            self.push_to_client(c, despawn.clone());
        }
    }

    async fn command_listener(self: Arc<Self>) {
        let stdin = tokio::io::stdin();
        let mut lines = BufReader::new(stdin).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.first() == Some(&"update") {
                match parts.get(1).copied() {
                    Some("all") => self.rolling_update_all().await,
                    Some(zone) => {
                        self.rolling_update_one(zone).await;
                    }
                    None => println!("[Proxy] usage: update <zone_id> | update all"),
                }
            } else if parts.len() >= 2 && parts[0] == "migrate" {
                match (parts[1], parts.len()) {
                    ("phase1", 3) => {
                        self.phase1_mark_for_migration(parts[2]).await;
                    }
                    ("phase2", 4) => {
                        self.phase2_transfer_players(parts[2], parts[3]).await;
                    }
                    ("phase3", 3) => {
                        self.phase3_retire_zone(parts[2]).await;
                    }
                    ("auto", 4) => {
                        let (source, target) = (parts[2], parts[3]);
                        println!(
                            "[Proxy] Starting automated 3-phase migration from {source} to {target}"
                        );
                        if self.phase1_mark_for_migration(source).await {
                            sleep(Duration::from_secs(1)).await;
                            if self.phase2_transfer_players(source, target).await {
                                sleep(Duration::from_secs(1)).await;
                                self.phase3_retire_zone(source).await;
                                println!("[Proxy] Migration complete!");
                            }
                        }
                    }
                    _ => print_migration_help(),
                }
            } else {
                print_migration_help();
            }
        }
    }

    async fn start(self: Arc<Self>) {
        println!("[Proxy] Listening for clients on ws://{}:{}", self.host, self.port);
        println!(
            "[Proxy] Zone registration service on ws://{}:{}",
            self.host, self.registration_port
        );
        println!("[Proxy] Admin UI service on ws://{}:{}", self.host, self.admin_port);
        println!("[Proxy] Migration commands: migrate phase1 <zone> | migrate phase2 <src> <tgt> | migrate phase3 <zone> | migrate auto <src> <tgt>");
        println!("[Proxy] Rolling update commands: update <zone_id> | update all");

        let client_listener = TcpListener::bind((self.host.as_str(), self.port))
            .await
            .expect("bind client port");
        let reg_listener = TcpListener::bind((self.host.as_str(), self.registration_port))
            .await
            .expect("bind registration port");
        let admin_listener = TcpListener::bind((self.host.as_str(), self.admin_port))
            .await
            .expect("bind admin port");

        // Accept clients.
        let me = self.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = client_listener.accept().await {
                let me = me.clone();
                tokio::spawn(async move { me.handle_client(stream).await });
            }
        });

        // Accept zone registrations.
        let me = self.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = reg_listener.accept().await {
                let me = me.clone();
                tokio::spawn(async move { me.handle_zone_registration(stream).await });
            }
        });

        // Accept admin UI connections.
        let me = self.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = admin_listener.accept().await {
                let me = me.clone();
                tokio::spawn(async move { me.handle_admin(stream).await });
            }
        });

        // Periodic persistence flush for connected durable characters.
        let me = self.clone();
        tokio::spawn(async move { me.persistence_flush().await });

        // Auto-scaler: split overpopulated zones.
        let me = self.clone();
        tokio::spawn(async move { me.autoscale_monitor().await });
        println!(
            "[Proxy] Auto-scaling on: zones split when population > {}",
            self.split_threshold
        );

        // Run the stdin command loop on the main task.
        self.command_listener().await;
    }
}

/// Build an ephemeral guest identity (not persisted). `pending` carries a
/// gameplay frame a legacy client sent in place of authenticating.
fn guest_identity(pending: Option<Value>) -> Identity {
    Identity {
        character_id: format!("guest_{}", Uuid::new_v4()),
        name: "guest".to_string(),
        x: SPAWN_X,
        y: SPAWN_Y,
        hp: SPAWN_HP,
        persistent: false,
        pending,
    }
}

/// Build a durable identity from a loaded/created character row.
fn persistent_identity(ch: mmo::persistence::Character) -> Identity {
    Identity {
        character_id: ch.id,
        name: ch.name,
        x: ch.x as i32,
        y: ch.y as i32,
        hp: ch.hp as i32,
        persistent: true,
        pending: None,
    }
}

/// A random 8-direction heading (never standing still), for the internal bots.
fn random_heading() -> (i32, i32) {
    let dirs = [
        (1, 0), (-1, 0), (0, 1), (0, -1),
        (1, 1), (1, -1), (-1, 1), (-1, -1),
    ];
    dirs[rand::thread_rng().gen_range(0..dirs.len())]
}

/// One gateway-spawned load-test bot: connect to the client port and wander,
/// re-rolling its heading occasionally. Aborting the task disconnects it.
async fn run_internal_bot(uri: String) {
    loop {
        if let Ok((ws, _)) = connect_async(&uri).await {
            let (mut sink, mut stream) = ws.split();
            let mut tick = tokio::time::interval(Duration::from_millis(300));
            let (mut hx, mut hy) = random_heading();
            loop {
                tokio::select! {
                    incoming = stream.next() => {
                        match incoming {
                            Some(Ok(_)) => {}
                            _ => break,
                        }
                    }
                    _ = tick.tick() => {
                        if rand::thread_rng().gen_bool(0.15) {
                            let (nx, ny) = random_heading();
                            hx = nx;
                            hy = ny;
                        }
                        let mv = json!({"type": "move", "dx": hx * 10, "dy": hy * 10}).to_string();
                        if sink.send(Message::Text(mv)).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
        sleep(Duration::from_secs(2)).await;
    }
}

fn print_migration_help() {
    println!("[Proxy] Migration commands:");
    println!("  migrate phase1 <zone_id>                    - Phase 1: Mark zone for migration");
    println!("  migrate phase2 <source_zone> <target_zone>  - Phase 2: Transfer players");
    println!("  migrate phase3 <zone_id>                    - Phase 3: Retire zone");
    println!("  migrate auto <source_zone> <target_zone>    - Execute all 3 phases automatically");
}

#[tokio::main]
async fn main() {
    // Durable store: SQLite file by default; override with DATABASE_URL (e.g. a
    // Postgres URL in staging/prod). If it can't be opened we run without
    // persistence so the demo still comes up (guests only).
    let db_url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://mmo_dev.db".to_string());
    let db = match Db::connect(&db_url).await {
        Ok(db) => {
            println!("[Proxy] Database ready ({db_url})");
            Some(Arc::new(db))
        }
        Err(e) => {
            println!("[Proxy] WARNING: database unavailable ({e}); running without persistence");
            None
        }
    };

    let proxy = Proxy::new("127.0.0.1", 8766, 8764, 8767, db);
    proxy.start().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::timeout;
    use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

    // ----------------------------------------------------------------------
    // Construction / injection helpers
    // ----------------------------------------------------------------------

    /// Proxy with the default ping interval; ports are unused since tests never
    /// call `start()` (handlers are driven directly).
    fn test_proxy() -> Arc<Proxy> {
        Proxy::new("127.0.0.1", 0, 0, 0, None)
    }

    /// Proxy with a short ping interval so the liveness reaper fires fast.
    fn test_proxy_ping(ping: Duration) -> Arc<Proxy> {
        Arc::new(Proxy {
            host: "127.0.0.1".into(),
            port: 0,
            registration_port: 0,
            admin_port: 0,
            clients: Mutex::new(HashMap::new()),
            zones: Mutex::new(HashMap::new()),
            zone_order: Mutex::new(Vec::new()),
            dropped_frames: AtomicU64::new(0),
            ping_interval: ping,
            entity_state: Mutex::new(HashMap::new()),
            children: Mutex::new(HashMap::new()),
            next_update_port: AtomicU16::new(FIRST_UPDATE_PORT),
            update_version: AtomicU32::new(1),
            split_counter: AtomicU32::new(0),
            cooldowns: Mutex::new(HashMap::new()),
            split_threshold: DEFAULT_SPLIT_THRESHOLD,
            bot_handles: Mutex::new(Vec::new()),
            db: None,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// Inject a zone owning the whole world; returns the receiver for whatever
    /// the proxy "sends to the zone" (stands in for the zone server's socket).
    fn add_zone(p: &Proxy, id: &str) -> mpsc::UnboundedReceiver<Message> {
        add_zone_region(p, id, Region::whole_world())
    }

    /// Inject a zone owning a specific region.
    fn add_zone_region(p: &Proxy, id: &str, region: Region) -> mpsc::UnboundedReceiver<Message> {
        let (tx, rx) = mpsc::unbounded_channel();
        p.zones.lock().unwrap().insert(
            id.to_string(),
            Zone {
                uri: format!("ws://test/{id}"),
                tx,
                migration_state: MigrationState::Normal,
                packet_buffer: HashMap::new(),
                population: 0,
                version: 1,
                exe: String::new(),
                region,
                owner: None,
                capture_progress: 0.0,
            },
        );
        p.zone_order.lock().unwrap().push(id.to_string());
        rx
    }

    /// Inject a client directly; returns its bounded outbound receiver.
    fn add_client(p: &Proxy, id: &str, zone: &str, cap: usize) -> mpsc::Receiver<Message> {
        let (tx, rx) = mpsc::channel(cap);
        p.clients.lock().unwrap().insert(
            id.to_string(),
            ClientInfo {
                player_id: id.to_string(),
                current_zone: zone.to_string(),
                tx,
                persistent: false,
            },
        );
        rx
    }

    /// A standalone ClientInfo (not registered in the map) for push tests.
    fn make_client(id: &str, zone: &str, cap: usize) -> (ClientInfo, mpsc::Receiver<Message>) {
        let (tx, rx) = mpsc::channel(cap);
        (
            ClientInfo {
                player_id: id.to_string(),
                current_zone: zone.to_string(),
                tx,
                persistent: false,
            },
            rx,
        )
    }

    fn parse(s: String) -> Value {
        serde_json::from_str(&s).expect("valid json")
    }

    async fn next_zone_text(rx: &mut mpsc::UnboundedReceiver<Message>) -> Value {
        match timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Some(Message::Text(t))) => parse(t),
            other => panic!("expected text from zone, got {other:?}"),
        }
    }

    async fn next_client_text(rx: &mut mpsc::Receiver<Message>) -> Value {
        match timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Some(Message::Text(t))) => parse(t),
            other => panic!("expected text from client, got {other:?}"),
        }
    }

    async fn recv_value(rx: &mut mpsc::UnboundedReceiver<Value>) -> Value {
        match timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Some(v)) => v,
            other => panic!("expected value, got {other:?}"),
        }
    }

    /// Read the next JSON text frame from a client websocket, skipping the
    /// control frames (ping/pong) the proxy may inject.
    async fn recv_ws_value(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) -> Value {
        loop {
            match timeout(Duration::from_secs(2), ws.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => {
                    let v = parse(t);
                    // Partition broadcasts and handshake frames are housekeeping; skip them.
                    let ty = v.get("type").and_then(|x| x.as_str());
                    if matches!(ty, Some("partition") | Some("auth_required") | Some("auth_ok")) {
                        continue;
                    }
                    return v;
                }
                Ok(Some(Ok(Message::Ping(_)))) | Ok(Some(Ok(Message::Pong(_)))) => continue,
                other => panic!("expected text from ws, got {other:?}"),
            }
        }
    }

    async fn wait_until<F: Fn() -> bool>(cond: F, limit: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < limit {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cond()
    }

    /// A fake zone server: a real websocket server the proxy connects out to.
    /// Captures everything the proxy sends, and lets the test push frames back.
    struct FakeZone {
        uri: String,
        from_proxy: mpsc::UnboundedReceiver<Value>,
        to_proxy: mpsc::UnboundedSender<Message>,
    }

    async fn spawn_fake_zone() -> FakeZone {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let uri = format!("ws://{addr}");
        let (from_tx, from_rx) = mpsc::unbounded_channel::<Value>();
        let (to_tx, mut to_rx) = mpsc::unbounded_channel::<Message>();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut sink, mut read) = ws.split();

            // Forward proxy -> zone messages out to the test.
            tokio::spawn(async move {
                while let Some(Ok(msg)) = read.next().await {
                    if let Message::Text(t) = msg {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            let _ = from_tx.send(v);
                        }
                    }
                }
            });

            // Forward test -> proxy frames into the socket.
            while let Some(msg) = to_rx.recv().await {
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
        });

        FakeZone {
            uri,
            from_proxy: from_rx,
            to_proxy: to_tx,
        }
    }

    /// Drive a real `handle_client` on a fresh ephemeral port and return the
    /// connected client side of the websocket.
    async fn connect_client(proxy: Arc<Proxy>) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (srv, _) = listener.accept().await.unwrap();
            proxy.handle_client(srv).await;
        });
        let (mut ws, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        // Authenticate as a guest so the handshake completes and the player joins.
        ws.send(Message::Text(json!({"type": "guest"}).to_string()))
            .await
            .unwrap();
        ws
    }

    // ----------------------------------------------------------------------
    // Pure logic unit tests
    // ----------------------------------------------------------------------

    #[test]
    fn migration_state_strings() {
        assert_eq!(MigrationState::Normal.as_str(), "normal");
        assert_eq!(MigrationState::Marking.as_str(), "marking");
        assert_eq!(MigrationState::Migrating.as_str(), "migrating");
        assert_eq!(MigrationState::Retired.as_str(), "retired");
    }

    #[test]
    fn pick_default_zone_none_when_empty() {
        let p = test_proxy();
        assert_eq!(p.pick_default_zone(), None);
    }

    #[test]
    fn pick_default_zone_returns_first_in_order() {
        let p = test_proxy();
        let _a = add_zone(&p, "zone_a");
        let _b = add_zone(&p, "zone_b");
        assert_eq!(p.pick_default_zone().as_deref(), Some("zone_a"));
    }

    #[test]
    fn pick_default_zone_skips_retired_zone_still_in_order() {
        let p = test_proxy();
        let _a = add_zone(&p, "zone_a");
        let _b = add_zone(&p, "zone_b");
        // zone_a removed from the map but left in zone_order (e.g. retired).
        p.zones.lock().unwrap().remove("zone_a");
        assert_eq!(p.pick_default_zone().as_deref(), Some("zone_b"));
    }

    #[test]
    fn push_to_client_delivers_message() {
        let p = test_proxy();
        let (info, mut rx) = make_client("p1", "z", 4);
        p.push_to_client(&info, Message::Text("hi".into()));
        assert_eq!(rx.try_recv().unwrap(), Message::Text("hi".into()));
        assert_eq!(p.dropped_frames.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn push_to_client_sheds_when_full_and_counts_drops() {
        let p = test_proxy();
        let cap = 2usize;
        let (info, mut rx) = make_client("p1", "z", cap);
        // Never drain while pushing -> queue fills, the rest are shed.
        for i in 0..10 {
            p.push_to_client(&info, Message::Text(format!("m{i}").into()));
        }
        let mut got = 0;
        while rx.try_recv().is_ok() {
            got += 1;
        }
        assert_eq!(got, cap, "exactly `cap` frames should be buffered");
        assert_eq!(
            p.dropped_frames.load(Ordering::Relaxed),
            (10 - cap) as u64,
            "the overflow should be counted as shed frames"
        );
    }

    #[test]
    fn push_to_client_closed_receiver_is_not_counted_as_shed() {
        let p = test_proxy();
        let (info, rx) = make_client("p1", "z", 4);
        drop(rx); // simulate the writer task / socket being gone
        p.push_to_client(&info, Message::Text("x".into()));
        assert_eq!(p.dropped_frames.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn status_snapshot_reports_zone_reported_populations() {
        let p = test_proxy();
        let _za = add_zone(&p, "zone_a");
        let _zb = add_zone(&p, "zone_b");
        // Zones report their populations (humans + AI players alike).
        p.set_zone_population("zone_a", 5);
        p.set_zone_population("zone_b", 2);

        let snap = p.status_snapshot();
        assert_eq!(snap["type"], "status");
        assert_eq!(snap["total_players"], 7);

        let zones = snap["zones"].as_array().unwrap();
        assert_eq!(zones.len(), 2);
        assert_eq!(zones[0]["zone_id"], "zone_a");
        assert_eq!(zones[0]["players"], 5);
        assert_eq!(zones[1]["zone_id"], "zone_b");
        assert_eq!(zones[1]["players"], 2);
    }

    #[test]
    fn zone_stats_updates_population_and_total() {
        let p = test_proxy();
        let _z = add_zone(&p, "zone_a");
        p.set_zone_population("zone_a", 9);
        let snap = p.status_snapshot();
        assert_eq!(snap["zones"][0]["players"], 9);
        assert_eq!(snap["total_players"], 9);
    }

    #[test]
    fn status_snapshot_includes_dropped_frames() {
        let p = test_proxy();
        p.dropped_frames.store(7, Ordering::Relaxed);
        assert_eq!(p.status_snapshot()["dropped_frames"], 7);
    }

    // ----------------------------------------------------------------------
    // Migration phase tests
    // ----------------------------------------------------------------------

    #[tokio::test]
    async fn phase1_marks_zone_and_buffers_only_its_players() {
        let p = test_proxy();
        let _z = add_zone(&p, "zone_a");
        let _c1 = add_client(&p, "p1", "zone_a", 4);
        let _c2 = add_client(&p, "p2", "zone_a", 4);
        let _c3 = add_client(&p, "p3", "zone_b", 4); // different zone

        assert!(p.phase1_mark_for_migration("zone_a").await);

        let zones = p.zones.lock().unwrap();
        let z = zones.get("zone_a").unwrap();
        assert_eq!(z.migration_state, MigrationState::Marking);
        assert!(z.packet_buffer.contains_key("p1"));
        assert!(z.packet_buffer.contains_key("p2"));
        assert!(!z.packet_buffer.contains_key("p3"));
    }

    #[tokio::test]
    async fn phase1_unknown_zone_fails() {
        let p = test_proxy();
        assert!(!p.phase1_mark_for_migration("nope").await);
    }

    #[tokio::test]
    async fn phase2_transfers_player_and_notifies_all_parties() {
        let p = test_proxy();
        let mut src_rx = add_zone(&p, "src");
        let mut tgt_rx = add_zone(&p, "tgt");
        let mut client_rx = add_client(&p, "p1", "src", 8);

        assert!(p.phase2_transfer_players("src", "tgt").await);

        // Client's authoritative zone is updated.
        assert_eq!(
            p.clients.lock().unwrap().get("p1").unwrap().current_zone,
            "tgt"
        );

        // Source zone is told the player left.
        let leave = next_zone_text(&mut src_rx).await;
        assert_eq!(leave["type"], "player_leave");
        assert_eq!(leave["player_id"], "p1");

        // Target zone is told the player joined.
        let join = next_zone_text(&mut tgt_rx).await;
        assert_eq!(join["type"], "player_join");
        assert_eq!(join["player_id"], "p1");

        // Client is notified of the migration.
        let mig = next_client_text(&mut client_rx).await;
        assert_eq!(mig["type"], "zone_migration");
        assert_eq!(mig["zone"], "tgt");
    }

    #[tokio::test]
    async fn phase2_replays_buffered_packets_to_target() {
        let p = test_proxy();
        let mut src_rx = add_zone(&p, "src");
        let mut tgt_rx = add_zone(&p, "tgt");
        let _c = add_client(&p, "p1", "src", 8);

        // Phase 1 sets up buffering; inject a buffered move as if it arrived
        // while the zone was marked.
        p.phase1_mark_for_migration("src").await;
        {
            let mut zones = p.zones.lock().unwrap();
            let z = zones.get_mut("src").unwrap();
            z.packet_buffer
                .get_mut("p1")
                .unwrap()
                .push(json!({"type": "move", "dx": 5, "player_id": "p1"}));
        }

        assert!(p.phase2_transfer_players("src", "tgt").await);

        // src: player_leave.
        let leave = next_zone_text(&mut src_rx).await;
        assert_eq!(leave["type"], "player_leave");

        // tgt: player_join, then the replayed buffered move (in that order).
        let join = next_zone_text(&mut tgt_rx).await;
        assert_eq!(join["type"], "player_join");
        let replay = next_zone_text(&mut tgt_rx).await;
        assert_eq!(replay["type"], "move");
        assert_eq!(replay["dx"], 5);
    }

    #[tokio::test]
    async fn phase2_missing_target_fails() {
        let p = test_proxy();
        let _z = add_zone(&p, "src");
        assert!(!p.phase2_transfer_players("src", "nope").await);
    }

    #[tokio::test]
    async fn phase3_retires_and_closes_zone() {
        let p = test_proxy();
        let mut z_rx = add_zone(&p, "zone_a");

        assert!(p.phase3_retire_zone("zone_a").await);
        assert_eq!(
            p.zones.lock().unwrap().get("zone_a").unwrap().migration_state,
            MigrationState::Retired
        );

        // The zone's writer is told to close.
        match timeout(Duration::from_secs(1), z_rx.recv()).await {
            Ok(Some(Message::Close(_))) => {}
            other => panic!("expected a Close frame to the zone, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn phase3_unknown_zone_fails() {
        let p = test_proxy();
        assert!(!p.phase3_retire_zone("nope").await);
    }

    // ----------------------------------------------------------------------
    // End-to-end integration tests (real websockets)
    // ----------------------------------------------------------------------

    #[tokio::test]
    async fn client_join_is_announced_and_status_is_routed_back() {
        let proxy = test_proxy();
        let mut zone = spawn_fake_zone().await;
        proxy.register_zone("zone_a".to_string(), zone.uri.clone(), 1, String::new(), Region::whole_world()).await;

        let mut ws = connect_client(proxy.clone()).await;

        // On connect, the proxy announces player_join to the zone.
        let join = recv_value(&mut zone.from_proxy).await;
        assert_eq!(join["type"], "player_join");
        let pid = join["player_id"].as_str().unwrap().to_string();

        // The client first receives a welcome with its assigned id + zone.
        let welcome = recv_ws_value(&mut ws).await;
        assert_eq!(welcome["type"], "welcome");
        assert_eq!(welcome["player_id"], pid);
        assert_eq!(welcome["zone"], "zone_a");

        // The zone emits a status_update; the client should receive it, now
        // tagged with the originating zone for the dual-zone view.
        zone.to_proxy
            .send(Message::Text(
                json!({
                    "type": "status_update",
                    "player_id": pid,
                    "state": {"x": 1, "y": 2, "hp": 100, "type": "player"}
                })
                .to_string(),
            ))
            .unwrap();

        let got = recv_ws_value(&mut ws).await;
        assert_eq!(got["type"], "status_update");
        assert_eq!(got["player_id"], pid);
        assert_eq!(got["zone"], "zone_a");
    }

    #[tokio::test]
    async fn migrate_request_routes_by_position_and_repoints_routing() {
        let p = test_proxy();
        // Two disjoint halves of the world.
        let _left = add_zone_region(&p, "zone_a", Region { x0: 0, y0: 0, x1: 600, y1: 1200 });
        let mut right = add_zone_region(&p, "zone_b", Region { x0: 600, y0: 0, x1: 1200, y1: 1200 });
        let mut client_rx = add_client(&p, "p1", "zone_a", 8);

        // p1 left zone_a at world (650, 200) — that point is owned by zone_b.
        let msg = json!({"type": "migrate_request", "player_id": "p1", "from": "zone_a", "x": 650, "y": 200, "hp": 100});
        p.handle_migrate_request(&msg);

        // zone_b is told to spawn it at the exact world position (seamless).
        let spawn = next_zone_text(&mut right).await;
        assert_eq!(spawn["type"], "spawn_entity");
        assert_eq!(spawn["player_id"], "p1");
        assert_eq!(spawn["x"], 650);
        assert_eq!(spawn["y"], 200);

        // Routing follows the player to the owning zone.
        assert_eq!(
            p.clients.lock().unwrap().get("p1").unwrap().current_zone,
            "zone_b"
        );
        let note = next_client_text(&mut client_rx).await;
        assert_eq!(note["type"], "zone_migration");
        assert_eq!(note["zone"], "zone_b");
    }

    #[tokio::test]
    async fn migrate_request_for_unowned_position_is_a_noop() {
        let p = test_proxy();
        let _left = add_zone_region(&p, "zone_a", Region { x0: 0, y0: 0, x1: 600, y1: 1200 });
        let mut client_rx = add_client(&p, "p1", "zone_a", 8);

        // (900, 200) is owned by no zone (only the left half exists).
        let msg = json!({"type": "migrate_request", "player_id": "p1", "from": "zone_a", "x": 900, "y": 200, "hp": 100});
        p.handle_migrate_request(&msg);

        // Routing unchanged; client not notified.
        assert_eq!(
            p.clients.lock().unwrap().get("p1").unwrap().current_zone,
            "zone_a"
        );
        assert!(client_rx.try_recv().is_err());
    }

    #[test]
    fn region_split_halves_longer_axis() {
        let r = Region { x0: 0, y0: 0, x1: 1200, y1: 1200 };
        let (a, b) = r.split();
        // Square splits along x (>=).
        assert_eq!((a.x0, a.x1), (0, 600));
        assert_eq!((b.x0, b.x1), (600, 1200));
        // A wide region splits along x; a tall one along y.
        let wide = Region { x0: 0, y0: 0, x1: 800, y1: 200 };
        assert_eq!(wide.split().0.x1, 400);
        let tall = Region { x0: 0, y0: 0, x1: 200, y1: 800 };
        assert_eq!(tall.split().0.y1, 400);
    }

    #[test]
    fn region_mergeable_and_union() {
        let left = Region { x0: 0, y0: 0, x1: 600, y1: 1200 };
        let right = Region { x0: 600, y0: 0, x1: 1200, y1: 1200 };
        let top = Region { x0: 0, y0: 0, x1: 600, y1: 600 };
        let bottom = Region { x0: 0, y0: 600, x1: 600, y1: 1200 };

        // Edge-adjacent with matching span -> mergeable.
        assert!(left.mergeable_with(&right));
        assert!(right.mergeable_with(&left));
        assert!(top.mergeable_with(&bottom));

        // Halves recombine to the original rectangle.
        let u = left.union(&right);
        assert_eq!((u.x0, u.y0, u.x1, u.y1), (0, 0, 1200, 1200));

        // A split's two halves are always mergeable back.
        let (a, b) = Region { x0: 0, y0: 0, x1: 1200, y1: 1200 }.split();
        assert!(a.mergeable_with(&b));

        // Not adjacent / mismatched spans -> not mergeable.
        let far = Region { x0: 700, y0: 700, x1: 800, y1: 800 };
        assert!(!left.mergeable_with(&far));
        // Touching but different spans (an L-shape) -> not mergeable.
        let small = Region { x0: 600, y0: 0, x1: 1200, y1: 600 };
        assert!(!left.mergeable_with(&small));
    }

    #[tokio::test]
    async fn merge_zones_folds_drop_into_keep() {
        let p = test_proxy();
        let mut keep_rx = add_zone_region(&p, "keep", Region { x0: 0, y0: 0, x1: 600, y1: 1200 });
        let mut drop_rx = add_zone_region(&p, "drop", Region { x0: 600, y0: 0, x1: 1200, y1: 1200 });
        let mut client_rx = add_client(&p, "p1", "drop", 8);
        // p1 is at a world position inside `drop`.
        p.entity_state.lock().unwrap().insert("p1".into(), (650, 300, 100));

        p.merge_zones("keep", "drop");

        // The survivor is told its new region (the union of both halves)...
        let set = next_zone_text(&mut keep_rx).await;
        assert_eq!(set["type"], "set_region");
        assert_eq!((set["x0"].as_i64(), set["x1"].as_i64()), (Some(0), Some(1200)));
        // ...then receives the migrated player at its exact world position.
        let spawn = next_zone_text(&mut keep_rx).await;
        assert_eq!(spawn["type"], "spawn_entity");
        assert_eq!(spawn["player_id"], "p1");
        assert_eq!(spawn["x"], 650);
        assert_eq!(spawn["y"], 300);

        // The retired zone is told to shut down.
        let bye = next_zone_text(&mut drop_rx).await;
        assert_eq!(bye["type"], "shutdown");

        // Partition state: drop is gone, keep owns the union.
        {
            let zones = p.zones.lock().unwrap();
            assert!(!zones.contains_key("drop"));
            let k = zones.get("keep").unwrap();
            assert_eq!((k.region.x0, k.region.x1), (0, 1200));
        }
        assert!(!p.zone_order.lock().unwrap().iter().any(|z| z == "drop"));

        // The player's session now points at the survivor and was notified.
        assert_eq!(p.clients.lock().unwrap().get("p1").unwrap().current_zone, "keep");
        let note = next_client_text(&mut client_rx).await;
        assert_eq!(note["type"], "zone_migration");
        assert_eq!(note["zone"], "keep");
    }

    #[tokio::test]
    async fn client_move_is_stamped_with_real_id_and_forwarded() {
        let proxy = test_proxy();
        let mut zone = spawn_fake_zone().await;
        proxy.register_zone("zone_a".to_string(), zone.uri.clone(), 1, String::new(), Region::whole_world()).await;

        let mut ws = connect_client(proxy.clone()).await;
        let join = recv_value(&mut zone.from_proxy).await;
        let pid = join["player_id"].as_str().unwrap().to_string();

        // Client sends a move with a SPOOFED player_id; proxy must overwrite it.
        ws.send(Message::Text(
            json!({"type": "move", "dx": 3, "dy": -2, "player_id": "HACKER"}).to_string(),
        ))
        .await
        .unwrap();

        let fwd = recv_value(&mut zone.from_proxy).await;
        assert_eq!(fwd["type"], "move");
        assert_eq!(fwd["dx"], 3);
        assert_eq!(fwd["dy"], -2);
        assert_eq!(fwd["player_id"], pid, "spoofed id must be replaced");
    }

    #[tokio::test]
    async fn dead_client_failing_liveness_is_reaped() {
        let proxy = test_proxy_ping(Duration::from_millis(150));
        let mut zone = spawn_fake_zone().await;
        proxy.register_zone("zone_a".to_string(), zone.uri.clone(), 1, String::new(), Region::whole_world()).await;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let p = proxy.clone();
        tokio::spawn(async move {
            let (srv, _) = listener.accept().await.unwrap();
            p.handle_client(srv).await;
        });

        // Connect and authenticate as a guest, but then NEVER poll the stream ->
        // tungstenite never auto-pongs, so the proxy's pings go unanswered.
        let mut ws = connect_async(format!("ws://{addr}")).await.unwrap().0;
        ws.send(Message::Text(json!({"type": "guest"}).to_string()))
            .await
            .unwrap();

        let join = recv_value(&mut zone.from_proxy).await;
        let pid = join["player_id"].as_str().unwrap().to_string();
        assert_eq!(proxy.clients.lock().unwrap().len(), 1);

        // Two missed ping intervals (~300ms) should trip the reaper.
        let reaped = wait_until(
            || proxy.clients.lock().unwrap().is_empty(),
            Duration::from_secs(3),
        )
        .await;
        assert!(reaped, "dead client was not reaped after failing liveness");

        // The zone is informed the player left during cleanup.
        let leave = recv_value(&mut zone.from_proxy).await;
        assert_eq!(leave["type"], "player_leave");
        assert_eq!(leave["player_id"], pid);
    }

    // ----------------------------------------------------------------------
    // M0: persistence + auth
    // ----------------------------------------------------------------------

    /// A unique sqlite file url for a test (relative to the crate dir).
    fn temp_db_url() -> String {
        format!("sqlite://mmo_test_{}.db", Uuid::new_v4().simple())
    }

    fn cleanup_db(url: &str) {
        let file = url.trim_start_matches("sqlite://");
        let _ = std::fs::remove_file(file);
        let _ = std::fs::remove_file(format!("{file}-wal"));
        let _ = std::fs::remove_file(format!("{file}-shm"));
    }

    /// Data-layer durability: state written by one `Db` is readable by a fresh
    /// `Db` opened on the same file — i.e. it survives a process restart.
    #[tokio::test]
    async fn persistence_survives_reopen() {
        let url = temp_db_url();
        let email = format!("a_{}@t.test", Uuid::new_v4().simple());

        let cid = {
            let db = Db::connect(&url).await.unwrap();
            let ch = auth::register(&db, &email, "pw12", "Hero", 100, 200, 100)
                .await
                .unwrap();
            db.save_character(&ch.id, 321, 654, 77, "zone_a").await.unwrap();
            ch.id
        }; // pool dropped — simulates shutdown

        // Reopen the same file: the character is still there.
        let db2 = Db::connect(&url).await.unwrap();
        let ch = db2
            .character_by_id(&cid)
            .await
            .unwrap()
            .expect("character persisted across reopen");
        assert_eq!((ch.x, ch.y, ch.hp), (321, 654, 77));

        // Login returns the same saved character; bad password / duplicate email fail.
        let logged = auth::login(&db2, &email, "pw12").await.unwrap();
        assert_eq!(logged.id, cid);
        assert_eq!(logged.x, 321);
        assert!(auth::login(&db2, &email, "wrong").await.is_err());
        assert!(auth::register(&db2, &email, "pw12", "Dup", 0, 0, 100).await.is_err());

        drop(db2);
        cleanup_db(&url);
    }

    /// End-to-end through the real gateway handshake: register, have the zone
    /// report a position, disconnect, then log back in and confirm the character
    /// is recreated at its saved position with the same durable id.
    #[tokio::test]
    async fn register_then_login_restores_saved_position() {
        let url = temp_db_url();
        let db = Arc::new(Db::connect(&url).await.unwrap());
        let proxy = Proxy::new("127.0.0.1", 0, 0, 0, Some(db.clone()));
        let mut zone = spawn_fake_zone().await;
        proxy
            .register_zone("zone_a".to_string(), zone.uri.clone(), 1, String::new(), Region::whole_world())
            .await;

        let email = format!("p_{}@t.test", Uuid::new_v4().simple());

        // --- Session 1: register, report a position, disconnect. ---
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let p = proxy.clone();
        tokio::spawn(async move {
            let (srv, _) = listener.accept().await.unwrap();
            p.handle_client(srv).await;
        });
        let (mut ws, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Hero"}).to_string(),
        ))
        .await
        .unwrap();
        let welcome = recv_ws_value(&mut ws).await;
        assert_eq!(welcome["type"], "welcome");
        let pid = welcome["player_id"].as_str().unwrap().to_string();

        // The zone reports the character at a non-spawn position.
        zone.to_proxy
            .send(Message::Text(
                json!({"type": "status_update", "player_id": pid,
                       "state": {"x": 321, "y": 654, "hp": 88, "type": "player"}})
                .to_string(),
            ))
            .unwrap();
        let cached = wait_until(
            || proxy.entity_state.lock().unwrap().get(&pid).map(|&(x, _, _)| x) == Some(321),
            Duration::from_secs(2),
        )
        .await;
        assert!(cached, "gateway did not cache the reported position");

        // Disconnect -> the gateway persists the last-known position.
        drop(ws);
        let mut saved = false;
        for _ in 0..100 {
            if let Some(ch) = db.character_by_id(&pid).await.unwrap() {
                if (ch.x, ch.y, ch.hp) == (321, 654, 88) {
                    saved = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(saved, "position was not persisted on disconnect");

        // Drain session-1 frames (its spawn + player_leave) so the next
        // spawn_entity we read is unambiguously from the login.
        while zone.from_proxy.try_recv().is_ok() {}

        // --- Session 2: login restores the saved position. ---
        let listener2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr2 = listener2.local_addr().unwrap();
        let p2 = proxy.clone();
        tokio::spawn(async move {
            let (srv, _) = listener2.accept().await.unwrap();
            p2.handle_client(srv).await;
        });
        let (mut ws2, _) = connect_async(format!("ws://{addr2}")).await.unwrap();
        ws2.send(Message::Text(
            json!({"type": "login", "email": email, "password": "pw12"}).to_string(),
        ))
        .await
        .unwrap();
        let welcome2 = recv_ws_value(&mut ws2).await;
        assert_eq!(welcome2["player_id"], pid, "same durable character id on re-login");

        // The gateway recreates the character at its exact saved position.
        let spawn = loop {
            let v = recv_value(&mut zone.from_proxy).await;
            if v["type"] == "spawn_entity" && v["player_id"] == pid {
                break v;
            }
        };
        assert_eq!(spawn["x"], 321);
        assert_eq!(spawn["y"], 654);
        assert_eq!(spawn["hp"], 88);

        drop(ws2);
        cleanup_db(&url);
    }

    // --- #2 acceptance: identity & sessions ------------------------------

    /// Stand up a proxy backed by a fresh db with one whole-world zone, plus the
    /// fake zone so the handshake can complete. Returns (proxy, db url, zone).
    async fn proxy_with_db() -> (Arc<Proxy>, String, FakeZone) {
        let url = temp_db_url();
        let db = Arc::new(Db::connect(&url).await.unwrap());
        let proxy = Proxy::new("127.0.0.1", 0, 0, 0, Some(db));
        let zone = spawn_fake_zone().await;
        proxy
            .register_zone("zone_a".to_string(), zone.uri.clone(), 1, String::new(), Region::whole_world())
            .await;
        (proxy, url, zone)
    }

    /// Spawn a one-shot acceptor running `handle_client` for the next connection,
    /// and return a client websocket connected to it.
    async fn dial(proxy: &Arc<Proxy>) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let p = proxy.clone();
        tokio::spawn(async move {
            let (srv, _) = listener.accept().await.unwrap();
            p.handle_client(srv).await;
        });
        let (ws, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        ws
    }

    /// Read frames until one of the given `type` arrives (skipping any others,
    /// including handshake/partition housekeeping).
    async fn recv_until(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>, ty: &str) -> Value {
        loop {
            match timeout(Duration::from_secs(2), ws.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => {
                    let v = parse(t);
                    if v.get("type").and_then(|x| x.as_str()) == Some(ty) {
                        return v;
                    }
                }
                Ok(Some(Ok(Message::Ping(_)))) | Ok(Some(Ok(Message::Pong(_)))) => continue,
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) => panic!("ws closed waiting for {ty}"),
                other => panic!("expected text waiting for {ty}, got {other:?}"),
            }
        }
    }

    /// Acceptance: an unknown account is rejected (no welcome, an auth_error).
    #[tokio::test]
    async fn unknown_account_is_rejected() {
        let (proxy, url, _zone) = proxy_with_db().await;
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "login", "email": "nobody@nowhere.test", "password": "whatever"}).to_string(),
        ))
        .await
        .unwrap();
        let err = recv_until(&mut ws, "auth_error").await;
        assert!(
            err["message"].as_str().unwrap().to_lowercase().contains("invalid"),
            "unexpected message: {err}"
        );
        drop(ws);
        cleanup_db(&url);
    }

    /// Acceptance: two logins for the same account collapse to one session — the
    /// second is refused while the first is online, and allowed again once it ends.
    #[tokio::test]
    async fn duplicate_login_collapses_to_one_session() {
        let (proxy, url, _zone) = proxy_with_db().await;
        let email = format!("dup_{}@t.test", Uuid::new_v4().simple());

        // Session 1: register and stay connected.
        let mut ws1 = dial(&proxy).await;
        ws1.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Hero"}).to_string(),
        ))
        .await
        .unwrap();
        let welcome = recv_until(&mut ws1, "welcome").await;
        let pid = welcome["player_id"].as_str().unwrap().to_string();
        assert!(proxy.clients.lock().unwrap().contains_key(&pid));

        // Session 2: a concurrent login for the same account is refused.
        let mut ws2 = dial(&proxy).await;
        ws2.send(Message::Text(
            json!({"type": "login", "email": email, "password": "pw12"}).to_string(),
        ))
        .await
        .unwrap();
        let err = recv_until(&mut ws2, "auth_error").await;
        assert!(
            err["message"].as_str().unwrap().contains("already online"),
            "unexpected message: {err}"
        );
        drop(ws2);

        // End session 1 -> the gateway drops the client, freeing the character.
        drop(ws1);
        let freed = wait_until(
            || !proxy.clients.lock().unwrap().contains_key(&pid),
            Duration::from_secs(2),
        )
        .await;
        assert!(freed, "character was not freed after the first session ended");

        // Session 3: login now succeeds as the same durable character.
        let mut ws3 = dial(&proxy).await;
        ws3.send(Message::Text(
            json!({"type": "login", "email": email, "password": "pw12"}).to_string(),
        ))
        .await
        .unwrap();
        let welcome3 = recv_until(&mut ws3, "welcome").await;
        assert_eq!(welcome3["player_id"], pid, "same character on re-login");
        drop(ws3);
        cleanup_db(&url);
    }

    /// Acceptance support: a reconnect with a valid session token resumes the same
    /// character without re-entering credentials.
    #[tokio::test]
    async fn token_reconnect_resumes_same_character() {
        let (proxy, url, _zone) = proxy_with_db().await;
        let email = format!("tok_{}@t.test", Uuid::new_v4().simple());

        // Register and capture the issued session token.
        let mut ws1 = dial(&proxy).await;
        ws1.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Hero"}).to_string(),
        ))
        .await
        .unwrap();
        let ok = recv_until(&mut ws1, "auth_ok").await;
        let token = ok["token"].as_str().unwrap().to_string();
        let pid = ok["player_id"].as_str().unwrap().to_string();
        assert!(!token.is_empty());

        // Disconnect and wait for the gateway to release the character.
        drop(ws1);
        let freed = wait_until(
            || !proxy.clients.lock().unwrap().contains_key(&pid),
            Duration::from_secs(2),
        )
        .await;
        assert!(freed);

        // Reconnect with the token alone -> same character, no credentials.
        let mut ws2 = dial(&proxy).await;
        ws2.send(Message::Text(json!({"type": "token", "token": token}).to_string()))
            .await
            .unwrap();
        let welcome = recv_until(&mut ws2, "welcome").await;
        assert_eq!(welcome["player_id"], pid, "token resumed the same character");
        drop(ws2);
        cleanup_db(&url);
    }
}
