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

use std::collections::{HashMap, VecDeque};
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

/// Spawn point for a brand-new character: the capital's town centre (the spawn
/// anchor authored in `mmo::world`). Kept in sync via `spawn_matches_town_centre`.
const SPAWN_X: i32 = WORLD_SIZE / 2;
const SPAWN_Y: i32 = WORLD_SIZE / 2;
const SPAWN_HP: i32 = 100;

/// Rent period seeded on a freshly claimed starter plot, and the period a
/// payment (manual or auto-pay) extends it by (#14).
const STARTER_RENT_PERIOD_SECS: i64 = 7 * 24 * 3600;
/// Gold deducted per rent period.
const RENT_COST_GOLD: i64 = 50;
/// How long a lapsed plot sits in grace before it's reclaimed.
const RENT_GRACE_SECS: i64 = 2 * 24 * 3600;
/// How far ahead of `rent_due_at` a one-time `rent.warning` fires.
const RENT_WARNING_LEAD_SECS: i64 = 24 * 3600;
/// How often the rent ticker checks every owned plot.
const RENT_TICK_INTERVAL: Duration = Duration::from_secs(60);

/// Current unix time in seconds. Used by the rent ticker (#14).
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

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
/// Mirrors `mmo::world::WORLD_SIZE` / `zone_server.rs`'s copy — keep in sync.
const WORLD_SIZE: i32 = 6400;

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

/// The cached last-known state of an entity, as reported by its owning zone's
/// `status_update`s — position/hp for recreating it elsewhere, plus an
/// in-progress gather job (if any) so a split/merge/rolling-update doesn't
/// silently drop it (#16). `gather` is `(node_id, progress)`.
#[derive(Clone)]
struct EntityCache {
    x: i32,
    y: i32,
    hp: i32,
    gather: Option<(String, i32)>,
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
    /// Last position+hp (and in-progress gather job, if any, #16) the proxy saw
    /// for each entity (from status_updates), keyed by player_id. Used to
    /// recreate entities at their real position — and resume gathering — in a
    /// freshly-spawned zone instance during a split/merge/rolling update.
    entity_state: Mutex<HashMap<String, EntityCache>>,
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
    /// The authored capital (named districts, road graph, plot grid, town centre).
    /// District identity is keyed to world geometry, so the gateway can name the
    /// district owning any zone region regardless of how the sim is sharded.
    capital: mmo::world::Capital,
    /// Unix-second timestamp of every rent reclaim (#16 ops counter, "reclaims in
    /// the last 24h"). In-memory only, like `dropped_frames` — a pure metric, not
    /// durable state (the reclaim itself is already durable via the DB).
    rent_reclaim_log: Mutex<VecDeque<i64>>,
    /// Rolling window of recent DB write durations in ms (#16 ops counter),
    /// sampled from the already-periodic `persistence_flush`/rent-ticker writes
    /// rather than instrumenting every call site.
    db_write_latencies_ms: Mutex<VecDeque<u64>>,
}

/// Cap on the rolling DB-latency sample window (#16) — recent-enough to be a
/// useful health signal without growing unbounded.
const DB_LATENCY_SAMPLES: usize = 50;
/// Window for the "rent reclaims" ops counter.
const RECLAIM_LOG_WINDOW_SECS: i64 = 24 * 3600;

/// Render a build order as the client-facing board entry used by `build.list`.
fn build_order_json(o: &mmo::persistence::BuildOrder) -> Value {
    json!({
        "order_id": o.id,
        "kind": o.kind,
        "required": serde_json::from_str::<Value>(&o.required_json).unwrap_or_else(|_| json!({})),
        "progress": serde_json::from_str::<Value>(&o.progress_json).unwrap_or_else(|_| json!({})),
        "state": o.state,
        // Skill gate (0 = ungated). The client greys the order and shows
        // "requires <skill> <level>" for players below the threshold.
        "required_skill": o.required_skill,
        "required_level": o.required_level,
    })
}

/// An `item -> qty` cost map as a JSON object (for `build.progress`).
fn cost_json(cost: &std::collections::BTreeMap<String, i64>) -> Value {
    Value::Object(cost.iter().map(|(k, v)| (k.clone(), json!(v))).collect())
}

/// Render one district-roster row (DB ownership + authored world-space bounds)
/// as the client-facing `plot.district` entry (#18).
fn plot_roster_entry_json(cell: &mmo::world::PlotCell, p: &mmo::persistence::PlotRosterRow) -> Value {
    json!({
        "plot_id": p.id, "owner_id": p.owner_character_id, "owner_name": p.owner_name,
        "bounds": {"x": cell.x, "y": cell.y, "w": cell.w, "h": cell.h}, "tier": p.tier,
    })
}

/// A completed city structure as a render entity (`status_update`, `state.type =
/// "structure"`). Its id is stable per order kind so re-sends update in place.
fn structure_status_json(spec: &mmo::world::SeedBuildOrder) -> Value {
    json!({
        "type": "status_update",
        "player_id": format!("structure_{}", spec.kind),
        "state": {
            "x": spec.structure_x, "y": spec.structure_y,
            "type": "structure", "kind": spec.structure_kind, "facing": [0, 0],
        },
    })
}

/// A home structure row (`build.placed`'s `structure` field) — plain fields, not
/// the `status_update` wrapper used for live rendering (#12).
fn structure_json(s: &mmo::persistence::Structure) -> Value {
    json!({
        "id": s.id, "plot_id": s.plot_id, "kind": s.kind,
        "x": s.x, "y": s.y, "rot": s.rot, "built_by": s.built_by,
    })
}

/// A home structure row as a `status_update` entity. Its own `kind` *is* the
/// entity's `state.type` (`bed`/`storage`/`crafting`) — deliberately distinct
/// from city structures, which all share `state.type == "structure"` — so a
/// player-placed home never collides with the "authored, never cached" bucket
/// city structures use, and a home storage chest transparently reuses the
/// existing `storage`-kind proximity/rendering plumbing (#12).
fn home_structure_status_json(s: &mmo::persistence::Structure) -> Value {
    json!({
        "type": "status_update",
        "player_id": s.id,
        "state": {
            "x": s.x, "y": s.y, "type": s.kind, "rot": s.rot,
            "built_by": s.built_by, "facing": [0, 0],
        },
    })
}

/// A plot's rent status as `rent.status`. `gold` is the *character's* balance,
/// not plot-scoped, but travels with rent status since it's what "can I pay"
/// hinges on client-side (#14).
fn rent_status_json(plot: &mmo::persistence::Plot, gold: i64) -> Value {
    json!({
        "type": "rent.status",
        "plot_id": plot.id, "due_at": plot.rent_due_at, "paid_through": plot.rent_paid_through,
        "state": plot.state, "auto_pay": plot.auto_pay, "gold": gold,
    })
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
            capital: mmo::world::capital(),
            rent_reclaim_log: Mutex::new(VecDeque::new()),
            db_write_latencies_ms: Mutex::new(VecDeque::new()),
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

    /// Build a snapshot of zones + per-zone player counts, plus gameplay
    /// counters (#16), for the admin UI. Player counts come from each zone's
    /// reported population, so AI players are included just like humans —
    /// every entity is a player.
    async fn status_snapshot(&self) -> Value {
        let mut total = 0usize;
        let zones_json: Vec<Value> = {
            let zones = self.zones.lock().unwrap();
            let order = self.zone_order.lock().unwrap();
            order
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
                .collect()
        };

        let (active_plots, open_build_orders) = match &self.db {
            Some(db) => (
                db.rent_active_plots().await.map(|p| p.len()).unwrap_or(0),
                db.count_open_build_orders().await.unwrap_or(0),
            ),
            None => (0, 0),
        };

        json!({
            "type": "status",
            "total_players": total,
            "dropped_frames": self.dropped_frames.load(Ordering::Relaxed),
            "zones": zones_json,
            "active_plots": active_plots,
            "open_build_orders": open_build_orders,
            "rent_reclaims_last_24h": self.reclaims_last_24h(),
            "db_write_latency_ms": self.avg_db_latency_ms(),
        })
    }

    /// Record a rent reclaim for the "reclaims in the last 24h" ops counter
    /// (#16) — in-memory only, like `dropped_frames`; the reclaim itself is
    /// already durable via the DB regardless of this log.
    fn record_reclaim(&self) {
        self.rent_reclaim_log.lock().unwrap().push_back(now_secs());
    }

    /// Reclaims recorded in the last 24h, pruning older entries as it reads.
    fn reclaims_last_24h(&self) -> usize {
        let mut log = self.rent_reclaim_log.lock().unwrap();
        let cutoff = now_secs() - RECLAIM_LOG_WINDOW_SECS;
        while log.front().is_some_and(|&t| t < cutoff) {
            log.pop_front();
        }
        log.len()
    }

    /// Record a DB write's duration for the rolling write-latency ops counter
    /// (#16), sampled from the already-periodic `persistence_flush`/rent-ticker
    /// writes rather than instrumenting every call site.
    fn record_db_latency(&self, elapsed: Duration) {
        let mut samples = self.db_write_latencies_ms.lock().unwrap();
        samples.push_back(elapsed.as_millis() as u64);
        while samples.len() > DB_LATENCY_SAMPLES {
            samples.pop_front();
        }
    }

    /// Rolling average DB write latency in ms (0.0 with no samples yet).
    fn avg_db_latency_ms(&self) -> f64 {
        let samples = self.db_write_latencies_ms.lock().unwrap();
        if samples.is_empty() {
            return 0.0;
        }
        samples.iter().sum::<u64>() as f64 / samples.len() as f64
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
                    .send(Message::Text(me.status_snapshot().await.to_string()))
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
        self.sync_home_structures_to_zone(&zone_id, region).await;
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
                    // recreate it at the right spot in a new zone instance. Resource
                    // nodes are authored, not player entities — never cache/recreate them.
                    if let (Some(pid), Some(st)) =
                        (target_player.as_deref(), data.get("state"))
                    {
                        let kind = st.get("type").and_then(|v| v.as_str());
                        // Authored, non-player world entities are re-sent by the zone
                        // on (re)spawn; never cache them as player state (which would
                        // resurrect them as fake players on a rolling update).
                        let authored = matches!(
                            kind,
                            Some("resource") | Some("storage") | Some("build_board") | Some("structure")
                        );
                        if !authored {
                            let x = st.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                            let y = st.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                            let hp = st.get("hp").and_then(|v| v.as_i64()).unwrap_or(100) as i32;
                            // In-progress gather job, if the zone included one (#16) — so
                            // a split/merge/rolling-update can resume it instead of
                            // silently dropping it.
                            let gather = st.get("gather_node").and_then(|v| v.as_str()).map(|node| {
                                (node.to_string(), st.get("gather_progress").and_then(|v| v.as_i64()).unwrap_or(0) as i32)
                            });
                            self.entity_state.lock().unwrap().insert(pid.to_string(), EntityCache { x, y, hp, gather });
                        }
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
                Some("gather_yield") => {
                    // Internal: a zone yielded a gathered unit. Persist it and push
                    // the authoritative inventory/skill to the client (not forwarded).
                    if let Some(pid) = target_player.as_deref() {
                        let item = data.get("item_id").and_then(|v| v.as_str()).unwrap_or("");
                        let qty = data.get("qty").and_then(|v| v.as_i64()).unwrap_or(0);
                        let skill = data.get("skill").and_then(|v| v.as_str()).unwrap_or("gathering");
                        let xp = data.get("xp").and_then(|v| v.as_i64()).unwrap_or(0);
                        self.apply_gather_yield(pid, item, qty, skill, xp).await;
                    }
                }
                Some("store_op") => {
                    // Internal: a zone validated a deposit/withdraw at a storage point.
                    // Perform the durable transfer and push the result (not forwarded).
                    if let Some(pid) = target_player.as_deref() {
                        let op = data.get("op").and_then(|v| v.as_str()).unwrap_or("");
                        let item = data.get("item_id").and_then(|v| v.as_str()).unwrap_or("");
                        let qty = data.get("qty").and_then(|v| v.as_i64()).unwrap_or(0);
                        self.apply_store_op(pid, op, item, qty).await;
                    }
                }
                Some("build_contribute") => {
                    // Internal: a zone validated a contribution at a build board. Apply
                    // the durable pooled contribution and push the result (not forwarded).
                    if let Some(pid) = target_player.as_deref() {
                        let order_id = data.get("order_id").and_then(|v| v.as_str()).unwrap_or("");
                        let item = data.get("item_id").and_then(|v| v.as_str()).unwrap_or("");
                        let qty = data.get("qty").and_then(|v| v.as_i64()).unwrap_or(0);
                        self.apply_build_contribute(pid, order_id, item, qty).await;
                    }
                }
                Some("migrate_request") => {
                    // A zone reports an entity left its region; route by position.
                    self.handle_migrate_request(&data);
                }
                Some("build_place") => {
                    // Internal: a zone validated the target point is on some plot.
                    // Ownership, footprint bounds/overlap, and the durable write are
                    // authoritative here (#12).
                    if let Some(pid) = target_player.as_deref() {
                        let kind = data.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                        let x = data.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                        let y = data.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                        let rot = data.get("rot").and_then(|v| v.as_i64()).unwrap_or(0);
                        self.apply_build_place(pid, kind, x, y, rot).await;
                    }
                }
                Some("craft_make") => {
                    // Internal: a zone validated the player is standing on some plot.
                    // Whether they own a crafting station there is authoritative here.
                    if let Some(pid) = target_player.as_deref() {
                        let recipe_id = data.get("recipe_id").and_then(|v| v.as_str()).unwrap_or("");
                        self.apply_craft_make(pid, recipe_id).await;
                    }
                }
                Some("player_died") => {
                    // A zone reports a death; the gateway alone decides where the
                    // player reappears (their bed, if set, else the default spawn) and
                    // hands off to whichever zone owns that point (#12).
                    self.handle_player_died(&data).await;
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
    /// the crossing is seamless.
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
        self.relocate_player(&pid, x, y, hp, &target);
    }

    /// A player died; the gateway alone decides where they reappear (their bed,
    /// if `home.set_respawn` was ever called, else the default town-centre
    /// spawn), then hands off to whichever zone owns that point — the same
    /// primitive `handle_migrate_request` uses, since the bed may be owned by a
    /// zone other than the one where the death happened (#12).
    async fn handle_player_died(&self, data: &Value) {
        let Some(pid) = data.get("player_id").and_then(|v| v.as_str()) else { return };
        let hp = data.get("hp").and_then(|v| v.as_i64()).unwrap_or(SPAWN_HP as i64) as i32;

        let (x, y) = match &self.db {
            Some(db) => match db.respawn_point_for_character(pid).await {
                Ok(Some((rx, ry))) => (rx as i32, ry as i32),
                _ => (SPAWN_X, SPAWN_Y),
            },
            None => (SPAWN_X, SPAWN_Y),
        };
        let target = match self.zone_at(x, y).or_else(|| self.pick_default_zone()) {
            Some(t) => t,
            None => {
                println!("[Proxy] player_died: no zone available to respawn {pid}");
                return;
            }
        };
        self.relocate_player(pid, x, y, hp, &target);
    }

    /// Place `pid` at world (x, y) in `target` zone: send it the entity, cache the
    /// authoritative position, and follow the player's client session (re-pointing
    /// `current_zone` and notifying it of the crossing). Shared by
    /// `handle_migrate_request` (a live region-boundary crossing) and
    /// `handle_player_died` (a respawn, which may also cross zones). Carries
    /// forward any in-progress gather job (#16) so the new zone can resume it.
    fn relocate_player(&self, pid: &str, x: i32, y: i32, hp: i32, target: &str) {
        let target_tx = self.zones.lock().unwrap().get(target).map(|z| z.tx.clone());
        let Some(tx) = target_tx else { return };
        let gather = self.entity_state.lock().unwrap().get(pid).and_then(|c| c.gather.clone());
        let mut msg = json!({"type": "spawn_entity", "player_id": pid, "x": x, "y": y, "hp": hp});
        if let Some((node_id, progress)) = &gather {
            msg["gather_node"] = json!(node_id);
            msg["gather_progress"] = json!(progress);
        }
        let _ = tx.send(Message::Text(msg.to_string()));
        self.entity_state.lock().unwrap().insert(pid.to_string(), EntityCache { x, y, hp, gather });

        // Follow the player's client session (every entity is a client).
        let mut clients = self.clients.lock().unwrap();
        if let Some(info) = clients.get_mut(pid) {
            info.current_zone = target.to_string();
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

    /// Build the current spatial partition: world size + each shard's region,
    /// owner, capture progress, and the **district**/**safety** it belongs to (by
    /// region centre, so the capital reads as named/multi-district and safe/wilds
    /// however it's sharded).
    fn partition_snapshot(&self) -> Value {
        let zones: Vec<Value> = {
            let zones = self.zones.lock().unwrap();
            let order = self.zone_order.lock().unwrap();
            order
                .iter()
                .filter_map(|id| {
                    zones.get(id).map(|z| {
                        let d = self.capital.district_for_region(mmo::world::Rect::new(
                            z.region.x0, z.region.y0, z.region.x1, z.region.y1,
                        ));
                        // safe inside the capital, wilds outside it (Phase 2 material).
                        let safety = match d.map(|d| d.safety) {
                            Some(mmo::world::Safety::Safe) => "safe",
                            Some(mmo::world::Safety::Wilds) | None => "wilds",
                        };
                        json!({
                            "zone_id": id,
                            "x0": z.region.x0, "y0": z.region.y0,
                            "x1": z.region.x1, "y1": z.region.y1,
                            "owner": z.owner,
                            "progress": z.capture_progress,
                            "district": d.map(|d| d.name),
                            "safety": safety,
                        })
                    })
                })
                .collect()
        };
        json!({"type": "partition", "world": WORLD_SIZE, "zones": zones})
    }

    /// Tell every client the current spatial partition so they can draw it.
    fn broadcast_partition(&self) {
        let msg = Message::Text(self.partition_snapshot().to_string());
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

        // 4. Recreate every entity in the new instance at its cached position,
        //    resuming an in-progress gather job if it had one (#16).
        for p in &players {
            let cached = self.entity_state.lock().unwrap().get(p).cloned();
            let (x, y, hp, gather) = match cached {
                Some(c) => (c.x, c.y, c.hp, c.gather),
                None => (WORLD_SIZE / 2, WORLD_SIZE / 2, 100, None),
            };
            let mut msg = json!({"type": "spawn_entity", "player_id": p, "x": x, "y": y, "hp": hp});
            if let Some((node_id, progress)) = &gather {
                msg["gather_node"] = json!(node_id);
                msg["gather_progress"] = json!(progress);
            }
            let _ = new_tx.send(Message::Text(msg.to_string()));
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
                        self.merge_zones(keep, drop).await;
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

        // Players currently in this zone, with cached world positions (and any
        // in-progress gather job, #16).
        let players: Vec<(String, i32, i32, i32, Option<(String, i32)>)> = {
            let clients = self.clients.lock().unwrap();
            let state = self.entity_state.lock().unwrap();
            clients
                .values()
                .filter(|i| i.current_zone == zone_id)
                .map(|i| match state.get(&i.player_id).cloned() {
                    Some(c) => (i.player_id.clone(), c.x, c.y, c.hp, c.gather),
                    None => (i.player_id.clone(), region.x0, region.y0, 100, None),
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
        self.sync_home_structures_to_zone(&new_id, give).await;

        // Shrink the original zone to the `keep` half.
        self.set_zone_region(zone_id, keep);
        self.sync_home_structures_to_zone(zone_id, keep).await;

        // Migrate the players who now fall in the `give` half, at their exact
        // world position (seamless — no teleport).
        let mut moved = 0;
        for (pid, x, y, hp, gather) in &players {
            if !give.contains(*x, *y) {
                continue;
            }
            let _ = old_tx.send(Message::Text(
                json!({"type": "player_leave", "player_id": pid}).to_string(),
            ));
            let mut msg = json!({"type": "spawn_entity", "player_id": pid, "x": x, "y": y, "hp": hp});
            if let Some((node_id, progress)) = gather {
                msg["gather_node"] = json!(node_id);
                msg["gather_progress"] = json!(progress);
            }
            let _ = new_tx.send(Message::Text(msg.to_string()));
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
    async fn merge_zones(&self, keep_id: &str, drop_id: &str) {
        let (keep_tx, keep_region, drop_tx, drop_region) = {
            let zones = self.zones.lock().unwrap();
            match (zones.get(keep_id), zones.get(drop_id)) {
                (Some(k), Some(d)) => (k.tx.clone(), k.region, d.tx.clone(), d.region),
                _ => return,
            }
        };
        let union = keep_region.union(&drop_region);

        // Players to move out of the retiring zone, with their world positions
        // (and any in-progress gather job, #16).
        let movers: Vec<(String, i32, i32, i32, Option<(String, i32)>)> = {
            let clients = self.clients.lock().unwrap();
            let state = self.entity_state.lock().unwrap();
            clients
                .values()
                .filter(|i| i.current_zone == drop_id)
                .map(|i| match state.get(&i.player_id).cloned() {
                    Some(c) => (i.player_id.clone(), c.x, c.y, c.hp, c.gather),
                    None => (i.player_id.clone(), union.x0, union.y0, 100, None),
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
        self.sync_home_structures_to_zone(keep_id, union).await;

        // Move the retiring zone's players into the survivor at their positions.
        for (pid, x, y, hp, gather) in &movers {
            let mut msg = json!({"type": "spawn_entity", "player_id": pid, "x": x, "y": y, "hp": hp});
            if let Some((node_id, progress)) = gather {
                msg["gather_node"] = json!(node_id);
                msg["gather_progress"] = json!(progress);
            }
            let _ = keep_tx.send(Message::Text(msg.to_string()));
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

            // Protocol-version gate. A client that declares its version must match
            // the gateway's, or it is refused cleanly (retrying can't fix a version
            // skew, so we close). Legacy/bot clients omit the field and fall through
            // to the guest path below — preserving backward compatibility.
            if let Some(v) = data.get("protocol_version").and_then(|v| v.as_u64()) {
                if v as u32 != PROTOCOL_VERSION {
                    let _ = tx
                        .send(Message::Text(
                            json!({"type": protocol::S_AUTH_ERROR,
                                   "message": format!(
                                       "protocol version mismatch: server {PROTOCOL_VERSION}, client {v}")})
                            .to_string(),
                        ))
                        .await;
                    return None;
                }
            }

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
            self.flush_once(&db).await;
        }
    }

    /// One pass of the periodic persistence flush: save every connected durable
    /// character's last-known cached position. Factored out of
    /// `persistence_flush`'s loop so a graceful shutdown (#44) can run exactly
    /// this same pass once, on demand, instead of waiting for the next tick.
    async fn flush_once(&self, db: &Db) {
        let targets: Vec<(String, String, i32, i32, i32)> = {
            let clients = self.clients.lock().unwrap();
            let state = self.entity_state.lock().unwrap();
            clients
                .values()
                .filter(|i| i.persistent)
                .filter_map(|i| {
                    state
                        .get(&i.player_id)
                        .map(|c| (i.player_id.clone(), i.current_zone.clone(), c.x, c.y, c.hp))
                })
                .collect()
        };
        for (id, district, x, y, hp) in targets {
            let started = Instant::now();
            let _ = db
                .save_character(&id, x as i64, y as i64, hp as i64, &district)
                .await;
            self.record_db_latency(started.elapsed());
        }
    }

    /// Best-effort final persistence pass on graceful shutdown (#44). Logout
    /// and migration already flush write-through; this covers the
    /// write-behind position/hp state the periodic ticker would otherwise
    /// only save on its next (up to 10s away) tick, so a clean stop never
    /// loses more than what was already in flight.
    async fn final_flush(&self) {
        let Some(db) = self.db.clone() else { return };
        self.flush_once(&db).await;
    }

    /// Send one JSON message to whichever connected client owns `pid`.
    fn push_to_player(&self, pid: &str, msg: Value) {
        let text = msg.to_string();
        let clients = self.clients.lock().unwrap();
        for info in clients.values() {
            if info.player_id == pid {
                self.push_to_client(info, Message::Text(text.clone()));
            }
        }
    }

    /// Push a character's current inventory (with carry capacity) to its client as
    /// `inv.update`.
    async fn send_inventory(&self, pid: &str) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        if let Ok(items) = db.inventory_for_character(pid).await {
            let used: i64 = items.iter().map(|it| it.qty).sum();
            let arr: Vec<Value> = items
                .iter()
                .map(|it| json!({"item_id": it.item_id, "qty": it.qty, "slot": it.slot}))
                .collect();
            self.push_to_player(pid, json!({
                "type": "inv.update", "player_id": pid, "items": arr,
                "used": used, "capacity": mmo::persistence::MAX_CARRY,
            }));
        }
    }

    /// Push a character's safe storage contents to its client as `store.update`.
    async fn send_storage(&self, pid: &str) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        if let Ok(items) = db.storage_for_character(pid).await {
            let arr: Vec<Value> = items
                .iter()
                .map(|it| json!({"item_id": it.item_id, "qty": it.qty}))
                .collect();
            self.push_to_player(pid, json!({"type": "store.update", "player_id": pid, "items": arr}));
        }
    }

    /// Perform a storage transfer reported by a zone (`store_op`) and push the
    /// updated inventory + storage to the client. The zone validated proximity; the
    /// gateway owns the durable, transactional move. No-op for guests / no DB.
    async fn apply_store_op(&self, pid: &str, op: &str, item_id: &str, qty: i64) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        let persistent = self
            .clients
            .lock()
            .unwrap()
            .get(pid)
            .map(|i| i.persistent)
            .unwrap_or(false);
        if !persistent {
            return;
        }
        let moved = match op {
            "deposit" => db.deposit(pid, item_id, qty).await,
            "withdraw" => db.withdraw(pid, item_id, qty).await,
            _ => Ok(0),
        };
        if moved.is_ok() {
            self.send_inventory(pid).await;
            self.send_storage(pid).await;
        }
    }

    // --- Build orders (city authority; #9) --------------------------------

    /// The district id owning a zone's region (by region centre), or `None` if the
    /// zone is unknown / outside the authored capital.
    fn district_for_zone(&self, zone_id: &str) -> Option<String> {
        let region = self.zones.lock().unwrap().get(zone_id).map(|z| z.region)?;
        self.capital
            .district_for_region(mmo::world::Rect::new(region.x0, region.y0, region.x1, region.y1))
            .map(|d| d.id.to_string())
    }

    /// Send one JSON message to every connected client whose current zone sits in
    /// `district`. Build-order state is district-scoped, so progress/completion/unlock
    /// notices go to exactly the players who share that district's board.
    fn broadcast_to_district(&self, district: &str, msg: Value) {
        let text = msg.to_string();
        let zone_ids = self.zones_in_district(district);
        let clients = self.clients.lock().unwrap();
        for info in clients.values() {
            if zone_ids.contains(&info.current_zone) {
                self.push_to_client(info, Message::Text(text.clone()));
            }
        }
    }

    /// The ids of every zone whose region **overlaps** `district` at all — the set
    /// a district-scoped push (build-order board, home structures) needs to reach.
    /// Deliberately overlap, not "this zone's primary district" (which is by
    /// region *centre* — see `district_for_zone`): a single zone can span every
    /// district at once (e.g. the default whole-world zone before any auto-scaling
    /// split), and it must still receive pushes for districts other than whichever
    /// one its centre happens to fall in.
    fn zones_in_district(&self, district: &str) -> Vec<String> {
        let Some(target) = self.capital.districts.iter().find(|d| d.id == district) else {
            return Vec::new();
        };
        let zones = self.zones.lock().unwrap();
        zones
            .iter()
            .filter(|(_, z)| {
                target.region.overlaps(mmo::world::Rect::new(
                    z.region.x0, z.region.y0, z.region.x1, z.region.y1,
                ))
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Push the open + completed build orders for a player's district as `build.list`.
    /// (Locked tech-tree dependents are omitted; they appear via `build.unlocked`.)
    async fn send_build_orders(&self, pid: &str) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        let zone_id = match self.clients.lock().unwrap().get(pid).map(|i| i.current_zone.clone()) {
            Some(z) => z,
            None => return,
        };
        let district = match self.district_for_zone(&zone_id) {
            Some(d) => d,
            None => return,
        };
        if let Ok(orders) = db.build_orders_for_district(&district).await {
            let arr: Vec<Value> = orders.iter().filter(|o| o.state != "locked").map(build_order_json).collect();
            self.push_to_player(pid, json!({"type": "build.list", "player_id": pid, "orders": arr}));
        }
    }

    /// Broadcast the refreshed board to everyone sharing `district` (after an unlock).
    async fn broadcast_build_list(&self, district: &str) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        if let Ok(orders) = db.build_orders_for_district(district).await {
            let arr: Vec<Value> = orders.iter().filter(|o| o.state != "locked").map(build_order_json).collect();
            self.broadcast_to_district(district, json!({"type": "build.list", "orders": arr}));
        }
    }

    /// Render every already-completed city structure for a just-joined client, so
    /// existing buildings appear on login (the durable source is the completed
    /// `build_order`; positions are authored).
    async fn send_completed_structures(&self, pid: &str) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        let mut districts: Vec<&str> = self.capital.build_orders.iter().map(|o| o.district).collect();
        districts.sort_unstable();
        districts.dedup();
        let mut completed: std::collections::HashSet<String> = std::collections::HashSet::new();
        for d in districts {
            if let Ok(orders) = db.build_orders_for_district(d).await {
                for o in orders {
                    if o.state == "completed" {
                        completed.insert(o.kind);
                    }
                }
            }
        }
        for spec in &self.capital.build_orders {
            if completed.contains(spec.kind) {
                self.push_to_player(pid, structure_status_json(spec));
            }
        }
    }

    /// Apply a `build_contribute` reported by a zone (which validated board proximity):
    /// the durable transactional contribution, then push the freed inventory + broadcast
    /// progress; on completion, pay lump-sum building XP, spawn the structure, and unlock
    /// dependents. No-op for guests (no durable inventory).
    async fn apply_build_contribute(&self, pid: &str, order_id: &str, item_id: &str, qty: i64) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        let persistent = self
            .clients
            .lock()
            .unwrap()
            .get(pid)
            .map(|i| i.persistent)
            .unwrap_or(false);
        if !persistent {
            return;
        }
        let res = match db.contribute(pid, order_id, item_id, qty).await {
            Ok(r) => r,
            Err(_) => return,
        };
        if res.moved > 0 {
            // Contributed items left the player's carry — refresh it, then tell the
            // district how the order's progress advanced.
            self.send_inventory(pid).await;
            self.broadcast_to_district(&res.district, json!({
                "type": "build.progress", "order_id": order_id,
                "required": cost_json(&res.required), "progress": cost_json(&res.progress),
            }));
        }
        if !res.completed {
            return;
        }

        // Lump-sum building XP to each contributor, split by units contributed.
        for (cid, units) in &res.contributors {
            let amount = units * mmo::persistence::BUILD_XP_PER_UNIT;
            if let Ok(gain) = db.grant_skill_xp(cid, "building", amount).await {
                self.push_skill_gain(cid, &gain);
            }
        }

        // The authored structure for this order kind.
        let spec = self.capital.build_orders.iter().find(|o| o.kind == res.kind).copied();
        let structures: Vec<Value> = spec
            .iter()
            .map(|s| json!({"kind": s.structure_kind, "x": s.structure_x, "y": s.structure_y}))
            .collect();
        self.broadcast_to_district(&res.district, json!({
            "type": "build.completed", "order_id": order_id, "structures": structures,
        }));
        // Render the new building for every connected client.
        if let Some(s) = spec {
            let entity = structure_status_json(&s).to_string();
            let clients = self.clients.lock().unwrap();
            for info in clients.values() {
                self.push_to_client(info, Message::Text(entity.clone()));
            }
        }

        // Unlock dependents (authored orders gated behind this kind).
        let dependents: Vec<(&str, &str)> = self
            .capital
            .build_orders
            .iter()
            .filter(|o| o.prereq == Some(res.kind.as_str()))
            .map(|o| (o.district, o.kind))
            .collect();
        let mut unlocked_ids: Vec<String> = Vec::new();
        for (d, k) in dependents {
            if let Ok(Some(o)) = db.open_build_order(d, k).await {
                unlocked_ids.push(o.id);
            }
        }
        if !unlocked_ids.is_empty() {
            self.broadcast_to_district(&res.district, json!({
                "type": "build.unlocked", "order_ids": unlocked_ids,
            }));
        }
        // Refresh the board for the district (the newly opened orders now appear).
        self.broadcast_build_list(&res.district).await;
    }

    /// Emit a `skill.update` for a just-granted skill, plus a `skill.levelup` when the
    /// grant crossed a level boundary. Centralises the two events so every XP source
    /// (gather, build, …) feeds the client identically.
    fn push_skill_gain(&self, cid: &str, gain: &mmo::persistence::SkillGain) {
        let s = &gain.skill;
        self.push_to_player(cid, json!({
            "type": "skill.update", "player_id": cid,
            "skill_id": s.skill_id, "xp": s.xp, "level": s.level,
        }));
        if gain.leveled_up {
            self.push_to_player(cid, json!({
                "type": "skill.levelup", "player_id": cid,
                "skill_id": s.skill_id, "level": s.level,
            }));
        }
    }

    /// Push a character's current skills to its client as `skill.update`s.
    async fn send_skills(&self, pid: &str) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        if let Ok(skills) = db.skills_for_character(pid).await {
            for s in skills {
                self.push_to_player(pid, json!({
                    "type": "skill.update", "player_id": pid,
                    "skill_id": s.skill_id, "xp": s.xp, "level": s.level,
                }));
            }
        }
    }

    // --- Starter plot allocation (#11) --------------------------------------

    /// Idempotently allocate a character's starter plot (in the district that
    /// authors a plot grid — currently just the Suburbs) and push it as
    /// `plot.assigned`. Called on login and in answer to `plot.info`, so a
    /// reconnect or an explicit request both just re-send the same plot.
    /// `just_claimed` tells the client whether this is the very first grant
    /// (drives the one-time "here's your plot" moment) versus a re-send.
    /// Also broadcasts the refreshed district roster (#18): a claim always
    /// changes some plot's ownership, so everyone else already standing in
    /// the district should see it go from free to taken without waiting for
    /// their own next login/district-crossing.
    async fn send_plot(&self, pid: &str) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        let persistent = self
            .clients
            .lock()
            .unwrap()
            .get(pid)
            .map(|i| i.persistent)
            .unwrap_or(false);
        if !persistent {
            return; // guests hold no land
        }
        let Some(district) = self.capital.districts.iter().find(|d| d.plot_grid.is_some()) else {
            return;
        };
        let had_plot = matches!(db.plot_for_character(pid).await, Ok(Some(_)));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let Ok(Some(plot)) = db.claim_plot(pid, district.id, STARTER_RENT_PERIOD_SECS, now).await
        else {
            return; // pool exhausted
        };
        let Some(cell) = district
            .plots()
            .into_iter()
            .find(|c| c.grid_x as i64 == plot.grid_x && c.grid_y as i64 == plot.grid_y)
        else {
            return;
        };
        self.push_to_player(pid, json!({
            "type": "plot.assigned", "plot_id": plot.id, "district": plot.district,
            "bounds": {"x": cell.x, "y": cell.y, "w": cell.w, "h": cell.h},
            "tier": plot.tier, "just_claimed": !had_plot,
        }));
        self.broadcast_plot_roster(district.id).await;
    }

    /// Push every plot in a player's district (owned or not, with owner names
    /// resolved) as `plot.district` — lets the client render a roster of
    /// everyone's land, not just the player's own (#18).
    async fn send_plot_roster(&self, pid: &str) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        // Resolved by the player's actual cached position, not their zone's
        // region *centre* (`district_for_zone`) — the latter only tells
        // apart districts when each is backed by its own zone shard (the
        // real auto-scaled deployment model). A single zone spanning every
        // district (the common small/dev deployment) has one fixed centre,
        // so `district_for_zone` would report the same district regardless
        // of where the player actually walks — invisible for `build.list`
        // (every Phase 1 build order is in Civic anyway) but very visible
        // here, since plots exist only in the Suburbs.
        let Some((x, y)) = self.entity_state.lock().unwrap().get(pid).map(|c| (c.x, c.y)) else { return };
        let Some(district_id) = self.capital.district_at(x, y).map(|d| d.id) else { return };
        let Some(district) = self.capital.districts.iter().find(|d| d.id == district_id) else { return };
        let cells = district.plots();
        if let Ok(rows) = db.plots_for_district(&district_id).await {
            let arr: Vec<Value> = rows
                .iter()
                .filter_map(|p| {
                    cells
                        .iter()
                        .find(|c| c.grid_x as i64 == p.grid_x && c.grid_y as i64 == p.grid_y)
                        .map(|cell| plot_roster_entry_json(cell, p))
                })
                .collect();
            self.push_to_player(pid, json!({"type": "plot.district", "plots": arr}));
        }
    }

    /// Broadcast the refreshed plot roster to everyone sharing `district` — a
    /// plot just changed hands via claim or reclaim, so their view shouldn't
    /// go stale until their next login/district-crossing.
    async fn broadcast_plot_roster(&self, district_id: &str) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        let Some(district) = self.capital.districts.iter().find(|d| d.id == district_id) else { return };
        let cells = district.plots();
        if let Ok(rows) = db.plots_for_district(district_id).await {
            let arr: Vec<Value> = rows
                .iter()
                .filter_map(|p| {
                    cells
                        .iter()
                        .find(|c| c.grid_x as i64 == p.grid_x && c.grid_y as i64 == p.grid_y)
                        .map(|cell| plot_roster_entry_json(cell, p))
                })
                .collect();
            self.broadcast_to_district(district_id, json!({"type": "plot.district", "plots": arr}));
        }
    }

    // --- Home structures: bed, storage, crafting station (#12) --------------

    /// Push every structure placed anywhere in the Suburbs (every character's
    /// home, not just `pid`'s own) as `status_update`s, so a just-joined player
    /// sees everyone's already-built homes. Called once on login.
    async fn send_home_structures(&self, pid: &str) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        let Some(district) = self.capital.districts.iter().find(|d| d.plot_grid.is_some()) else {
            return;
        };
        if let Ok(structures) = db.structures_in_district(district.id).await {
            for s in &structures {
                self.push_to_player(pid, home_structure_status_json(s));
            }
        }
    }

    /// Apply a `build_place` reported by a zone (which validated only that the
    /// *target* point sits on some plot — geometry, not ownership). Resolve the
    /// caller's own plot and validate kind/bounds/overlap here, where ownership
    /// and durable state actually live. Silent no-op on any failure — no error
    /// protocol surface, matching `store_op`/`build_contribute`'s convention.
    async fn apply_build_place(&self, pid: &str, kind: &str, x: i32, y: i32, rot: i64) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        let Some((w, h)) = mmo::world::structure_footprint(kind) else { return };
        let Ok(Some(plot)) = db.plot_for_character(pid).await else { return };
        let Some(district) = self.capital.districts.iter().find(|d| d.id == plot.district) else {
            return;
        };
        let Some(cell) = district
            .plots()
            .into_iter()
            .find(|c| c.grid_x as i64 == plot.grid_x && c.grid_y as i64 == plot.grid_y)
        else {
            return;
        };
        let bounds = cell.rect();
        if x < bounds.x0 || y < bounds.y0 || x + w > bounds.x1 || y + h > bounds.y1 {
            return; // footprint would escape the owner's own plot
        }
        let Ok(existing) = db.structures_for_plot(&plot.id).await else { return };
        for s in &existing {
            let Some((ew, eh)) = mmo::world::structure_footprint(&s.kind) else { continue };
            let overlap_x = (x as i64) < s.x + ew as i64 && s.x < (x + w) as i64;
            let overlap_y = (y as i64) < s.y + eh as i64 && s.y < (y + h) as i64;
            if overlap_x && overlap_y {
                return; // would overlap something already on the plot
            }
        }
        let Ok(structure) = db
            .place_structure(&plot.id, kind, x as i64, y as i64, rot, 100, Some(pid), "{}")
            .await
        else {
            return;
        };
        self.push_to_player(pid, json!({"type": "build.placed", "structure": structure_json(&structure)}));
        self.broadcast_to_district(&plot.district, home_structure_status_json(&structure));
        self.push_home_structure_to_zones(&plot.district, &structure);
    }

    /// Tell every zone sharding `district` about one newly-placed structure, so
    /// they can gate deposit/withdraw/craft on proximity to it without ever
    /// touching the DB themselves (#13). The zone has no DB access; this (plus
    /// `sync_home_structures_to_zone` on registration/split) is how it learns
    /// where structures are.
    fn push_home_structure_to_zones(&self, district: &str, s: &mmo::persistence::Structure) {
        let msg = json!({
            "type": "home_structure_added", "id": s.id, "kind": s.kind, "x": s.x, "y": s.y,
        })
        .to_string();
        let zone_ids = self.zones_in_district(district);
        let zones = self.zones.lock().unwrap();
        for id in &zone_ids {
            if let Some(z) = zones.get(id) {
                let _ = z.tx.send(Message::Text(msg.clone()));
            }
        }
    }

    /// Push the full set of home structures inside `region` to the zone that owns
    /// it — called whenever a zone registers or its region changes (split/merge),
    /// mirroring how `storage_points`/`build_boards` are (re)derived on those
    /// events, except this data lives in the DB (not static world authoring), so
    /// the gateway must push it rather than the zone deriving it itself (#13).
    async fn sync_home_structures_to_zone(&self, zone_id: &str, region: Region) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        let Some(district) = self.capital.districts.iter().find(|d| d.plot_grid.is_some()) else {
            return;
        };
        let Ok(structures) = db.structures_in_district(district.id).await else { return };
        let in_region: Vec<Value> = structures
            .iter()
            .filter(|s| region.contains(s.x as i32, s.y as i32))
            .map(|s| json!({"id": s.id, "kind": s.kind, "x": s.x, "y": s.y}))
            .collect();
        let tx = self.zones.lock().unwrap().get(zone_id).map(|z| z.tx.clone());
        if let Some(tx) = tx {
            let _ = tx.send(Message::Text(
                json!({"type": "home_structures_sync", "structures": in_region}).to_string(),
            ));
        }
    }

    /// Apply a `craft_make` reported by a zone (which validated only that the
    /// player is standing on some plot). Confirm they own a `crafting`-kind
    /// structure somewhere on their own plot, then attempt the craft. Silent
    /// no-op on failure (no station, unknown recipe, insufficient ingredients).
    async fn apply_craft_make(&self, pid: &str, recipe_id: &str) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        let Some(recipe) = mmo::world::recipe(recipe_id) else { return };
        let Ok(Some(plot)) = db.plot_for_character(pid).await else { return };
        let Ok(structures) = db.structures_for_plot(&plot.id).await else { return };
        if !structures.iter().any(|s| s.kind == "crafting") {
            return;
        }
        let Ok(true) = db.craft(pid, recipe.inputs, recipe.output_item, recipe.output_qty).await else {
            return;
        };
        self.send_inventory(pid).await;
        self.push_to_player(pid, json!({
            "type": "craft.made", "recipe_id": recipe.id,
            "item_id": recipe.output_item, "qty": recipe.output_qty,
        }));
        if let Ok(gain) = db.grant_skill_xp(pid, "crafting", mmo::persistence::CRAFT_XP_PER_CRAFT).await {
            self.push_skill_gain(pid, &gain);
        }
    }

    /// Answer `craft.list` with the static recipe registry. Stateless — no DB or
    /// position involved, so this never needs to touch the zone.
    fn send_recipes(&self, pid: &str) {
        let recipes: Vec<Value> = mmo::world::recipes()
            .iter()
            .map(|r| json!({
                "id": r.id, "name": r.name,
                "inputs": r.inputs.iter().map(|(item, qty)| json!({"item_id": item, "qty": qty})).collect::<Vec<_>>(),
                "output_item": r.output_item, "output_qty": r.output_qty,
            }))
            .collect();
        self.push_to_player(pid, json!({"type": "craft.recipes", "recipes": recipes}));
    }

    /// Apply `home.set_respawn`: `bed_id` must name a `bed`-kind structure on the
    /// caller's own plot. Silent no-op otherwise (no error protocol surface).
    async fn apply_set_respawn(&self, pid: &str, bed_id: &str) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        if bed_id.is_empty() {
            return;
        }
        let Ok(Some(plot)) = db.plot_for_character(pid).await else { return };
        let Ok(structures) = db.structures_for_plot(&plot.id).await else { return };
        let is_own_bed = structures.iter().any(|s| s.id == bed_id && s.kind == "bed");
        if !is_own_bed {
            return;
        }
        if db.set_respawn_structure(pid, Some(bed_id)).await.is_ok() {
            self.push_to_player(pid, json!({"type": "home.respawn_set", "bed_id": bed_id}));
        }
    }

    // --- Rent: ticker, pay/auto-pay, lapse -> reclaim (#14) ------------------

    /// Push a character's own plot's rent status (and current gold balance) as
    /// `rent.status`. Called on login and after any rent-affecting action.
    async fn send_rent_status(&self, pid: &str) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        let Ok(Some(plot)) = db.plot_for_character(pid).await else { return };
        let gold = db.character_gold(pid).await.unwrap_or(0);
        self.push_to_player(pid, rent_status_json(&plot, gold));
    }

    /// Apply `rent.pay`: deduct gold and extend the plot, only if `pid` owns it
    /// and can afford `RENT_COST_GOLD`. Silent no-op otherwise — no error
    /// protocol surface, matching `store_op`/`build_contribute`'s convention.
    async fn apply_rent_pay(&self, pid: &str, plot_id: &str) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        let Ok(Some(plot)) = db
            .pay_rent_with_gold(pid, plot_id, RENT_COST_GOLD, STARTER_RENT_PERIOD_SECS, now_secs())
            .await
        else {
            return;
        };
        let gold = db.character_gold(pid).await.unwrap_or(0);
        self.push_to_player(pid, rent_status_json(&plot, gold));
    }

    /// Apply `rent.set_autopay`: toggle whether the ticker should auto-deduct
    /// gold for this plot when due. Ownership-checked; silent no-op otherwise.
    async fn apply_rent_set_autopay(&self, pid: &str, plot_id: &str, enabled: bool) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        if db.set_auto_pay(pid, plot_id, enabled).await.unwrap_or(false) {
            self.send_rent_status(pid).await;
        }
    }

    /// Tell every zone sharding `district` that a home structure is gone (a
    /// reclaim demolished it, #14) — the removal counterpart to
    /// `push_home_structure_to_zones`, keeping a zone's proximity cache (#13)
    /// from gating deposit/withdraw/craft on a structure that no longer exists.
    fn push_home_structure_removed(&self, district: &str, structure_id: &str) {
        let msg = json!({"type": "home_structure_removed", "id": structure_id}).to_string();
        let zone_ids = self.zones_in_district(district);
        let zones = self.zones.lock().unwrap();
        for id in &zone_ids {
            if let Some(z) = zones.get(id) {
                let _ = z.tx.send(Message::Text(msg.clone()));
            }
        }
    }

    /// Carry out a plot's reclaim once `apply_rent_tick` has already made the
    /// state transition durable: demolish its structures (flair is preserved,
    /// unattached — see `Db::reclaim_plot_belongings`), tell bystanders and the
    /// district's zones those structures are gone, and notify the former owner.
    /// `moved_to_storage` is genuinely empty — home storage is character-global,
    /// not plot-scoped (#12/#13), so nothing needed converting into it.
    async fn reclaim_plot(&self, former_owner: &str, plot: &mmo::persistence::Plot) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        let Ok(deleted_ids) = db.reclaim_plot_belongings(&plot.id, former_owner).await else {
            return;
        };
        for id in &deleted_ids {
            self.broadcast_to_district(&plot.district, json!({"type": "despawn", "player_id": id}));
            self.push_home_structure_removed(&plot.district, id);
        }
        self.push_to_player(former_owner, json!({
            "type": "rent.reclaimed", "plot_id": plot.id, "moved_to_storage": Vec::<String>::new(),
        }));
        self.record_reclaim();
        self.broadcast_plot_roster(&plot.district).await;
    }

    /// The per-plot rent logic for one ticker pass, at `now`: auto-pay if due
    /// and enabled/affordable, warn once as the due date approaches, otherwise
    /// advance the lapse/reclaim state machine. Takes `now` as a parameter
    /// (rather than reading the clock internally) so tests can drive the whole
    /// lapse→reclaim path with a fabricated timeline, mirroring
    /// `Db::apply_rent_tick`'s existing testable shape.
    async fn tick_one_plot(&self, plot: &mmo::persistence::Plot, now: i64) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        let Some(owner) = plot.owner_character_id.clone() else { return };
        let due = plot.rent_due_at.unwrap_or(i64::MAX);

        if plot.state == "active" {
            if now >= due && plot.auto_pay {
                if let Ok(Some(paid)) = db
                    .pay_rent_with_gold(&owner, &plot.id, RENT_COST_GOLD, STARTER_RENT_PERIOD_SECS, now)
                    .await
                {
                    let gold = db.character_gold(&owner).await.unwrap_or(0);
                    self.push_to_player(&owner, rent_status_json(&paid, gold));
                    return;
                }
                // Couldn't afford it: fall through to the lapse path below.
            } else if now < due
                && now >= due.saturating_sub(RENT_WARNING_LEAD_SECS)
                && !plot.warned
            {
                if db.mark_rent_warned(&plot.id).await.is_ok() {
                    self.push_to_player(&owner, json!({
                        "type": "rent.warning", "plot_id": plot.id, "due_at": due,
                    }));
                }
                return;
            }
        }

        let Ok(Some(new_state)) = db.apply_rent_tick(&plot.id, now, RENT_GRACE_SECS).await else {
            return;
        };
        match new_state.as_str() {
            "lapsed" if plot.state == "active" => {
                if let Ok(Some(fresh)) = db.load_plot(&plot.id).await {
                    let gold = db.character_gold(&owner).await.unwrap_or(0);
                    self.push_to_player(&owner, rent_status_json(&fresh, gold));
                }
            }
            "reclaimed" => self.reclaim_plot(&owner, plot).await,
            _ => {}
        }
    }

    /// One rent-ticker pass over every owned plot, at `now`.
    async fn tick_rent(&self, now: i64) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        let started = Instant::now();
        let Ok(plots) = db.rent_active_plots().await else { return };
        for plot in &plots {
            self.tick_one_plot(plot, now).await;
        }
        self.record_db_latency(started.elapsed());
    }

    /// Periodic rent ticker (#14): every owned plot, whether or not its owner
    /// is currently connected — auto-pay if enabled and affordable, warn as the
    /// due date approaches, and advance the lapse/reclaim state machine
    /// otherwise. Mirrors `persistence_flush`'s interval-loop shape.
    async fn rent_monitor(self: Arc<Self>) {
        let mut interval = tokio::time::interval(RENT_TICK_INTERVAL);
        interval.tick().await; // consume the immediate first tick
        loop {
            interval.tick().await;
            self.tick_rent(now_secs()).await;
        }
    }

    /// Persist a gather yield reported by a zone (`gather_yield`) and push the
    /// authoritative inventory + skill back to the client. The zone is authoritative
    /// for the *simulation* (range, depletion); the gateway owns the *durable* write,
    /// mirroring how character position is persisted. No-op for guests / no DB.
    async fn apply_gather_yield(&self, pid: &str, item_id: &str, qty: i64, skill: &str, xp: i64) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        let persistent = self
            .clients
            .lock()
            .unwrap()
            .get(pid)
            .map(|i| i.persistent)
            .unwrap_or(false);
        if !persistent {
            return; // guests gather visually (gather.result) but nothing is persisted
        }
        if db.add_to_inventory(pid, item_id, qty).await.is_err() {
            return;
        }
        self.send_inventory(pid).await;
        if let Ok(gain) = db.grant_skill_xp(pid, skill, xp).await {
            self.push_skill_gain(pid, &gain);
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
                    self.entity_state.lock().unwrap().insert(
                        player_id.clone(),
                        EntityCache { x: identity.x, y: identity.y, hp: identity.hp, gather: None },
                    );
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

        // Hydrate the client's gameplay state: inventory, storage, skills, the
        // district's build-order board, any already-completed city structures, the
        // character's starter plot (allocating one on a brand-new character),
        // every home structure in the district (everyone's homes, not just
        // theirs), and their own plot's rent status.
        if identity.persistent {
            self.send_inventory(&player_id).await;
            self.send_storage(&player_id).await;
            self.send_skills(&player_id).await;
            self.send_build_orders(&player_id).await;
            self.send_completed_structures(&player_id).await;
            self.send_plot(&player_id).await;
            self.send_home_structures(&player_id).await;
            self.send_rent_status(&player_id).await;
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
                    // `build.list` is a pure read of gateway-owned city state — answer
                    // it directly rather than routing to the zone.
                    if data.get("type").and_then(|v| v.as_str()) == Some("build.list") {
                        self.send_build_orders(&player_id).await;
                        continue;
                    }
                    // `plot.info` is a pure re-send of the character's current plot —
                    // answer it directly rather than routing to the zone.
                    if data.get("type").and_then(|v| v.as_str()) == Some("plot.info") {
                        self.send_plot(&player_id).await;
                        continue;
                    }
                    // `plot.district` is a pure read of the current district's plot
                    // roster (#18) — answer it directly, same as `plot.info`.
                    if data.get("type").and_then(|v| v.as_str()) == Some("plot.district") {
                        self.send_plot_roster(&player_id).await;
                        continue;
                    }
                    // `craft.list` is a stateless read of the static recipe registry —
                    // no player position/proximity is relevant, so answer directly.
                    if data.get("type").and_then(|v| v.as_str()) == Some("craft.list") {
                        self.send_recipes(&player_id);
                        continue;
                    }
                    // `home.set_respawn` only needs DB ownership checking (is this bed
                    // mine?), not live position, so it's answered directly too.
                    if data.get("type").and_then(|v| v.as_str()) == Some("home.set_respawn") {
                        let bed_id = data.get("bed_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        self.apply_set_respawn(&player_id, &bed_id).await;
                        continue;
                    }
                    // `rent.pay`/`rent.set_autopay` are both DB-ownership-checked with
                    // no live-position dependency, so they're answered directly too.
                    if data.get("type").and_then(|v| v.as_str()) == Some("rent.pay") {
                        let plot_id = data.get("plot_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        self.apply_rent_pay(&player_id, &plot_id).await;
                        continue;
                    }
                    if data.get("type").and_then(|v| v.as_str()) == Some("rent.set_autopay") {
                        let plot_id = data.get("plot_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let enabled = data.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
                        self.apply_rent_set_autopay(&player_id, &plot_id, enabled).await;
                        continue;
                    }
                    // `district.enter` is the client announcing (self-detected from the
                    // `partition` it already has) that it crossed a district gate and is
                    // showing a transition curtain. The actual position/zone handoff
                    // already happened via the ordinary migrate-request path — this is
                    // purely the client-facing load/ready handshake (#15): refresh the
                    // district-scoped content (the build board) for wherever the player
                    // actually now is, then ack so the client can drop the curtain.
                    if data.get("type").and_then(|v| v.as_str()) == Some("district.enter") {
                        self.send_build_orders(&player_id).await;
                        self.send_plot_roster(&player_id).await;
                        self.push_to_player(&player_id, json!({"type": "district.ready"}));
                        continue;
                    }
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
                    let (x, y, hp) = last_state
                        .map(|c| (c.x, c.y, c.hp))
                        .unwrap_or((identity.x, identity.y, identity.hp));
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

        // Rent ticker: pay/auto-pay, lapse -> reclaim (#14).
        let me = self.clone();
        tokio::spawn(async move { me.rent_monitor().await });

        // Run the stdin command loop on the main task, alongside a listener for
        // an OS shutdown signal (Ctrl+C, or SIGTERM from a process manager) —
        // whichever comes first ends the process, but either way we get one
        // last chance to flush write-behind state before exiting (#44).
        tokio::select! {
            _ = self.clone().command_listener() => {
                println!("[Proxy] stdin closed, shutting down");
            }
            _ = shutdown_signal() => {
                println!("[Proxy] Shutdown signal received");
            }
        }
        self.final_flush().await;
        println!("[Proxy] Persistence flushed, exiting");
    }
}

/// Resolves on Ctrl+C, or (on Unix) SIGTERM — the two signals a graceful stop
/// (a terminal interrupt, or a process manager like systemd/Docker/k8s asking
/// the process to shut down) is expected to send (#44). Windows has no SIGTERM
/// equivalent that Tokio exposes, so that branch never resolves there.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
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
            // Seed the authored capital (plot grid + first build orders) on boot.
            // Idempotent, so a restart never duplicates it.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            match db.seed_capital(&mmo::world::capital(), now).await {
                Ok(()) => println!("[Proxy] Capital seeded ({} starter plots)", mmo::world::capital().starter_plots().len()),
                Err(e) => println!("[Proxy] WARNING: capital seeding failed: {e}"),
            }
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
            capital: mmo::world::capital(),
            rent_reclaim_log: Mutex::new(VecDeque::new()),
            db_write_latencies_ms: Mutex::new(VecDeque::new()),
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

    #[tokio::test]
    async fn status_snapshot_reports_zone_reported_populations() {
        let p = test_proxy();
        let _za = add_zone(&p, "zone_a");
        let _zb = add_zone(&p, "zone_b");
        // Zones report their populations (humans + AI players alike).
        p.set_zone_population("zone_a", 5);
        p.set_zone_population("zone_b", 2);

        let snap = p.status_snapshot().await;
        assert_eq!(snap["type"], "status");
        assert_eq!(snap["total_players"], 7);

        let zones = snap["zones"].as_array().unwrap();
        assert_eq!(zones.len(), 2);
        assert_eq!(zones[0]["zone_id"], "zone_a");
        assert_eq!(zones[0]["players"], 5);
        assert_eq!(zones[1]["zone_id"], "zone_b");
        assert_eq!(zones[1]["players"], 2);
    }

    #[tokio::test]
    async fn zone_stats_updates_population_and_total() {
        let p = test_proxy();
        let _z = add_zone(&p, "zone_a");
        p.set_zone_population("zone_a", 9);
        let snap = p.status_snapshot().await;
        assert_eq!(snap["zones"][0]["players"], 9);
        assert_eq!(snap["total_players"], 9);
    }

    #[tokio::test]
    async fn status_snapshot_includes_dropped_frames() {
        let p = test_proxy();
        p.dropped_frames.store(7, Ordering::Relaxed);
        assert_eq!(p.status_snapshot().await["dropped_frames"], 7);
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

    /// #16 (migration safety): a player's in-progress gather job is carried
    /// forward across a migration rather than silently dropped — the gateway
    /// caches it from `status_update`s (extended to report it) and includes it
    /// in the `spawn_entity` it sends to whichever zone the player lands in.
    #[tokio::test]
    async fn migrate_request_carries_an_in_progress_gather_job_forward() {
        let p = test_proxy();
        let _left = add_zone_region(&p, "zone_a", Region { x0: 0, y0: 0, x1: 600, y1: 1200 });
        let mut right = add_zone_region(&p, "zone_b", Region { x0: 600, y0: 0, x1: 1200, y1: 1200 });
        let _client_rx = add_client(&p, "p1", "zone_a", 8);

        // The gateway already cached p1's gather job from an earlier status_update
        // (zone_a's tick loop now reports it — see `entity_status_json`).
        p.entity_state.lock().unwrap().insert(
            "p1".into(),
            EntityCache { x: 640, y: 200, hp: 100, gather: Some(("node_suburbs_tree_0".to_string(), 7)) },
        );

        let msg = json!({"type": "migrate_request", "player_id": "p1", "from": "zone_a", "x": 650, "y": 200, "hp": 100});
        p.handle_migrate_request(&msg);

        let spawn = next_zone_text(&mut right).await;
        assert_eq!(spawn["type"], "spawn_entity");
        assert_eq!(spawn["gather_node"], "node_suburbs_tree_0");
        assert_eq!(spawn["gather_progress"], 7);

        // The cache carries it forward too (e.g. for a *second* migration in a row).
        let cached = p.entity_state.lock().unwrap().get("p1").unwrap().gather.clone();
        assert_eq!(cached, Some(("node_suburbs_tree_0".to_string(), 7)));
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
        p.entity_state.lock().unwrap().insert("p1".into(), EntityCache { x: 650, y: 300, hp: 100, gather: None });

        p.merge_zones("keep", "drop").await;

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

    /// An RAII temp sqlite database for gateway tests. The file lives under the
    /// system temp dir (never the crate dir) and is removed — with its `-wal`/`-shm`
    /// sidecars — when the guard drops, so cleanup happens even if a test panics.
    struct TestDb {
        url: String,
    }
    impl TestDb {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("mmo_test_{}.db", Uuid::new_v4().simple()));
            TestDb { url: format!("sqlite://{}", path.to_string_lossy()) }
        }
        fn url(&self) -> &str {
            &self.url
        }
    }
    impl Drop for TestDb {
        fn drop(&mut self) {
            let file = self.url.trim_start_matches("sqlite://");
            let _ = std::fs::remove_file(file);
            let _ = std::fs::remove_file(format!("{file}-wal"));
            let _ = std::fs::remove_file(format!("{file}-shm"));
        }
    }

    /// Data-layer durability: state written by one `Db` is readable by a fresh
    /// `Db` opened on the same file — i.e. it survives a process restart.
    #[tokio::test]
    async fn persistence_survives_reopen() {
        let dbf = TestDb::new();
        let url = dbf.url();
        let email = format!("a_{}@t.test", Uuid::new_v4().simple());

        let cid = {
            let db = Db::connect(url).await.unwrap();
            let ch = auth::register(&db, &email, "pw12", "Hero", 100, 200, 100)
                .await
                .unwrap();
            db.save_character(&ch.id, 321, 654, 77, "zone_a").await.unwrap();
            ch.id
        }; // pool dropped — simulates shutdown

        // Reopen the same file: the character is still there.
        let db2 = Db::connect(url).await.unwrap();
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
    }

    /// #44: a graceful shutdown must not lose the write-behind position/hp the
    /// periodic ticker would otherwise sit on for up to 10s — `final_flush`
    /// saves it immediately instead of waiting for the next tick.
    #[tokio::test]
    async fn final_flush_saves_cached_position_immediately() {
        let (proxy, dbf, _zone) = proxy_with_db().await;
        let db = Db::connect(dbf.url()).await.unwrap();

        let email = format!("shutdown_{}@t.test", Uuid::new_v4().simple());
        let ch = auth::register(&db, &email, "pw12", "Hero", 100, 200, 100).await.unwrap();

        // Simulate a connected player whose cached (moved-since-last-flush)
        // position has never made it to the periodic ticker yet.
        proxy.clients.lock().unwrap().insert(
            ch.id.clone(),
            ClientInfo {
                player_id: ch.id.clone(),
                current_zone: "zone_a".to_string(),
                tx: mpsc::channel(8).0,
                persistent: true,
            },
        );
        proxy.entity_state.lock().unwrap().insert(
            ch.id.clone(),
            EntityCache { x: 4242, y: 1337, hp: 55, gather: None },
        );

        proxy.final_flush().await;

        let saved = db.character_by_id(&ch.id).await.unwrap().expect("character exists");
        assert_eq!((saved.x, saved.y, saved.hp), (4242, 1337, 55), "the cached position was saved immediately, not left for the next 10s tick");
    }

    /// A guest (non-persistent) connection has nothing to save — `final_flush`
    /// must skip it rather than erroring on a character row that doesn't exist.
    #[tokio::test]
    async fn final_flush_skips_non_persistent_clients() {
        let (proxy, _dbf, _zone) = proxy_with_db().await;
        proxy.clients.lock().unwrap().insert(
            "guest_1".to_string(),
            ClientInfo {
                player_id: "guest_1".to_string(),
                current_zone: "zone_a".to_string(),
                tx: mpsc::channel(8).0,
                persistent: false,
            },
        );
        proxy.entity_state.lock().unwrap().insert(
            "guest_1".to_string(),
            EntityCache { x: 1, y: 2, hp: 100, gather: None },
        );

        // Should not panic, and should leave no character row behind.
        proxy.final_flush().await;
        let db = &proxy.db.as_ref().unwrap();
        assert!(db.character_by_id("guest_1").await.unwrap().is_none());
    }

    /// End-to-end through the real gateway handshake: register, have the zone
    /// report a position, disconnect, then log back in and confirm the character
    /// is recreated at its saved position with the same durable id.
    #[tokio::test]
    async fn register_then_login_restores_saved_position() {
        let dbf = TestDb::new();
        let db = Arc::new(Db::connect(dbf.url()).await.unwrap());
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
            || proxy.entity_state.lock().unwrap().get(&pid).map(|c| c.x) == Some(321),
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
    }

    // --- #2 acceptance: identity & sessions ------------------------------

    /// Stand up a proxy backed by a fresh db with one whole-world zone, plus the
    /// fake zone so the handshake can complete. The returned `TestDb` guard must be
    /// held for the test's lifetime; it deletes the db file on drop.
    async fn proxy_with_db() -> (Arc<Proxy>, TestDb, FakeZone) {
        let dbf = TestDb::new();
        let db = Arc::new(Db::connect(dbf.url()).await.unwrap());
        let proxy = Proxy::new("127.0.0.1", 0, 0, 0, Some(db));
        let zone = spawn_fake_zone().await;
        proxy
            .register_zone("zone_a".to_string(), zone.uri.clone(), 1, String::new(), Region::whole_world())
            .await;
        (proxy, dbf, zone)
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

    /// Read the next text frame (skipping ping/pong), or `None` on timeout/close.
    async fn recv_frame(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) -> Option<Value> {
        loop {
            match timeout(Duration::from_secs(2), ws.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => return Some(parse(t)),
                Ok(Some(Ok(Message::Ping(_)))) | Ok(Some(Ok(Message::Pong(_)))) => continue,
                _ => return None,
            }
        }
    }

    /// #4: the gateway's spawn constant must agree with the authored town centre.
    #[test]
    fn spawn_matches_town_centre() {
        let c = mmo::world::capital();
        assert_eq!((SPAWN_X, SPAWN_Y), c.town_centre);
        // And the town centre is a real, named district.
        assert!(c.district_at(SPAWN_X, SPAWN_Y).is_some());
    }

    /// #4: the partition the gateway broadcasts names each shard's district, so the
    /// capital reads as named & multi-district regardless of sharding.
    #[tokio::test]
    async fn partition_labels_districts() {
        let proxy = test_proxy();
        // Three shards, one per authored district band.
        add_zone_region(&proxy, "z_market", Region { x0: 0, y0: 0, x1: 1600, y1: 6400 });
        add_zone_region(&proxy, "z_civic", Region { x0: 1600, y0: 1600, x1: 4800, y1: 4800 });
        add_zone_region(&proxy, "z_suburbs", Region { x0: 4800, y0: 0, x1: 6400, y1: 6400 });

        let snap = proxy.partition_snapshot();
        let by_zone = |zid: &str| -> String {
            snap["zones"]
                .as_array()
                .unwrap()
                .iter()
                .find(|z| z["zone_id"] == zid)
                .unwrap()["district"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(by_zone("z_market"), "Market District");
        assert_eq!(by_zone("z_civic"), "Civic Centre");
        assert_eq!(by_zone("z_suburbs"), "Starter Suburbs");
    }

    /// Acceptance (#3): a client that declares a mismatched protocol version is
    /// cleanly refused, while a matching version (and the legacy no-version path)
    /// is accepted.
    #[tokio::test]
    async fn protocol_version_mismatch_is_refused() {
        let (proxy, _dbf, _zone) = proxy_with_db().await;

        // Mismatched version -> auth_error, no welcome.
        let mut bad = dial(&proxy).await;
        bad.send(Message::Text(
            json!({"type": "guest", "protocol_version": PROTOCOL_VERSION + 1}).to_string(),
        ))
        .await
        .unwrap();
        let err = recv_until(&mut bad, "auth_error").await;
        assert!(
            err["message"].as_str().unwrap().contains("version mismatch"),
            "unexpected message: {err}"
        );
        drop(bad);

        // Matching version -> normal welcome.
        let mut good = dial(&proxy).await;
        good.send(Message::Text(
            json!({"type": "guest", "protocol_version": PROTOCOL_VERSION}).to_string(),
        ))
        .await
        .unwrap();
        let welcome = recv_until(&mut good, "welcome").await;
        assert_eq!(welcome["protocol_version"], PROTOCOL_VERSION);
        drop(good);
    }

    /// Acceptance: an unknown account is rejected (no welcome, an auth_error).
    #[tokio::test]
    async fn unknown_account_is_rejected() {
        let (proxy, _dbf, _zone) = proxy_with_db().await;
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
    }

    /// Acceptance: two logins for the same account collapse to one session — the
    /// second is refused while the first is online, and allowed again once it ends.
    #[tokio::test]
    async fn duplicate_login_collapses_to_one_session() {
        let (proxy, _dbf, _zone) = proxy_with_db().await;
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
    }

    /// #7: a `gather_yield` reported by a zone is persisted and the authoritative
    /// inventory + skill are pushed back to the gathering client.
    #[tokio::test]
    async fn gather_yield_persists_and_notifies_client() {
        let (proxy, _dbf, zone) = proxy_with_db().await;
        let email = format!("g_{}@t.test", Uuid::new_v4().simple());

        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Hero"}).to_string(),
        ))
        .await
        .unwrap();
        let welcome = recv_until(&mut ws, "welcome").await;
        let pid = welcome["player_id"].as_str().unwrap().to_string();

        // The (fake) zone reports a gathered unit of wood for this player.
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "gather_yield", "player_id": pid,
                "item_id": "wood", "qty": 1, "skill": "gathering", "xp": 10,
            }).to_string()))
            .unwrap();

        // The client receives an inv.update carrying the wood (proves the DB round
        // trip: add_to_inventory -> read back -> push), then a skill.update.
        let mut got_wood = false;
        for _ in 0..10 {
            let v = recv_until(&mut ws, "inv.update").await;
            let items = v["items"].as_array().cloned().unwrap_or_default();
            if items.iter().any(|it| it["item_id"] == "wood" && it["qty"].as_i64() == Some(1)) {
                got_wood = true;
                break;
            }
        }
        assert!(got_wood, "client never received an inv.update with the gathered wood");

        let s = recv_until(&mut ws, "skill.update").await;
        assert_eq!(s["skill_id"], "gathering");
        assert_eq!(s["xp"].as_i64(), Some(10));

        drop(ws);
    }

    /// #8: a `store_op` deposit reported by a zone moves carried items into safe
    /// storage and pushes the updated inventory + storage to the client.
    #[tokio::test]
    async fn store_deposit_persists_and_notifies_client() {
        let (proxy, _dbf, zone) = proxy_with_db().await;
        let email = format!("s_{}@t.test", Uuid::new_v4().simple());

        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Hero"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();

        // Give the character some wood (as a gather would), then deposit 3.
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "gather_yield", "player_id": pid,
                "item_id": "wood", "qty": 5, "skill": "gathering", "xp": 10,
            }).to_string()))
            .unwrap();
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "store_op", "player_id": pid,
                "op": "deposit", "item_id": "wood", "qty": 3,
            }).to_string()))
            .unwrap();

        // Storage ends up with 3 wood; carried inventory drops to 2 (and the
        // deposited wood no longer counts against carry capacity). The two updates
        // interleave, so scan frames once and check both.
        let mut stored_ok = false;
        let mut carry_ok = false;
        for _ in 0..30 {
            let Some(v) = recv_frame(&mut ws).await else { break };
            match v["type"].as_str() {
                Some("store.update") => {
                    let items = v["items"].as_array().cloned().unwrap_or_default();
                    if items.iter().any(|it| it["item_id"] == "wood" && it["qty"].as_i64() == Some(3)) {
                        stored_ok = true;
                    }
                }
                Some("inv.update") => {
                    let items = v["items"].as_array().cloned().unwrap_or_default();
                    let wood = items.iter().find(|it| it["item_id"] == "wood");
                    if wood.map(|w| w["qty"].as_i64()) == Some(Some(2)) {
                        assert_eq!(v["used"].as_i64(), Some(2), "carry usage should drop with the deposit");
                        carry_ok = true;
                    }
                }
                _ => {}
            }
            if stored_ok && carry_ok {
                break;
            }
        }
        assert!(stored_ok, "storage never showed the deposited wood");
        assert!(carry_ok, "inventory never reflected the deposit");

        drop(ws);
    }

    /// #9 headline: pooling gathered items into the Town Well fills it, then completion
    /// pays building XP, spawns the structure, and unlocks the next order — the full
    /// gateway path a zone's `build_contribute` drives.
    #[tokio::test]
    async fn build_contribute_completes_order_pays_xp_and_unlocks() {
        let (proxy, dbf, zone) = proxy_with_db().await;
        // Seed the capital's build-order tech tree into the shared db file.
        let db = Db::connect(dbf.url()).await.unwrap();
        db.seed_capital(&mmo::world::capital(), 0).await.unwrap();
        let well_id = db
            .build_orders_for_district("civic")
            .await
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "town_well")
            .unwrap()
            .id;

        let email = format!("b_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Builder"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();

        // Stock exactly the Town Well's cost (wood 20 + stone 10), as gathering would.
        for (item, qty) in [("wood", 20), ("stone", 10)] {
            zone.to_proxy
                .send(Message::Text(json!({
                    "type": "gather_yield", "player_id": pid,
                    "item_id": item, "qty": qty, "skill": "gathering", "xp": 1,
                }).to_string()))
                .unwrap();
        }
        // Contribute both items to the well (zone already validated board proximity).
        for (item, qty) in [("wood", 20), ("stone", 10)] {
            zone.to_proxy
                .send(Message::Text(json!({
                    "type": "build_contribute", "player_id": pid,
                    "order_id": well_id, "item_id": item, "qty": qty,
                }).to_string()))
                .unwrap();
        }

        // Expect: progress, completion (with the well structure), a building skill gain,
        // a building level-up (30 units → 150 XP → Building 1), and the wall_section
        // unlock. Frames interleave — scan once, check all.
        let (mut progressed, mut completed, mut built_xp, mut leveled, mut unlocked) =
            (false, false, false, false, false);
        for _ in 0..80 {
            let Some(v) = recv_frame(&mut ws).await else { break };
            match v["type"].as_str() {
                Some("build.progress") if v["order_id"] == json!(well_id) => progressed = true,
                Some("build.completed") if v["order_id"] == json!(well_id) => {
                    let structs = v["structures"].as_array().cloned().unwrap_or_default();
                    assert!(structs.iter().any(|s| s["kind"] == "well"), "well structure missing");
                    completed = true;
                }
                Some("skill.update") if v["skill_id"] == "building" => {
                    if v["xp"].as_i64().unwrap_or(0) > 0 {
                        built_xp = true;
                    }
                }
                Some("skill.levelup") if v["skill_id"] == "building" => {
                    assert_eq!(v["level"].as_i64(), Some(1), "well completion reaches Building 1");
                    leveled = true;
                }
                Some("build.unlocked") => unlocked = true,
                _ => {}
            }
            if progressed && completed && built_xp && leveled && unlocked {
                break;
            }
        }
        assert!(progressed, "never saw build.progress");
        assert!(completed, "the Town Well never completed");
        assert!(built_xp, "contributor never gained building XP");
        assert!(leveled, "contributor never got a building level-up");
        assert!(unlocked, "the dependent order never unlocked");

        // Durable: the order is completed and wall_section is now open.
        let orders = db.build_orders_for_district("civic").await.unwrap();
        assert_eq!(orders.iter().find(|o| o.id == well_id).unwrap().state, "completed");
        assert_eq!(orders.iter().find(|o| o.kind == "wall_section").unwrap().state, "open");

        drop(ws);
    }

    /// Acceptance support: a reconnect with a valid session token resumes the same
    /// character without re-entering credentials.
    #[tokio::test]
    async fn token_reconnect_resumes_same_character() {
        let (proxy, _dbf, _zone) = proxy_with_db().await;
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
    }

    /// #11 acceptance: a brand-new character is handed a distinct, outlined starter
    /// plot in the Suburbs on first login (with `bounds` it can walk back to); a
    /// reconnect re-sends the *same* plot rather than granting a second one.
    #[tokio::test]
    async fn starter_plot_allocated_on_first_login_and_idempotent_on_reconnect() {
        let (proxy, dbf, _zone) = proxy_with_db().await;
        let db = Db::connect(dbf.url()).await.unwrap();
        db.seed_capital(&mmo::world::capital(), 0).await.unwrap();

        let email = format!("plot_{}@t.test", Uuid::new_v4().simple());
        let mut ws1 = dial(&proxy).await;
        ws1.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Settler"}).to_string(),
        ))
        .await
        .unwrap();
        let welcome = recv_until(&mut ws1, "welcome").await;
        let pid = welcome["player_id"].as_str().unwrap().to_string();
        let assigned = recv_until(&mut ws1, "plot.assigned").await;
        assert_eq!(assigned["district"], "suburbs");
        assert_eq!(assigned["just_claimed"], true);
        let bounds = &assigned["bounds"];
        assert!(bounds["w"].as_i64().unwrap() > 0 && bounds["h"].as_i64().unwrap() > 0);
        let plot_id = assigned["plot_id"].as_str().unwrap().to_string();

        // Disconnect and wait for the gateway to release the character.
        drop(ws1);
        let freed = wait_until(
            || !proxy.clients.lock().unwrap().contains_key(&pid),
            Duration::from_secs(2),
        )
        .await;
        assert!(freed);

        // Reconnect: the same plot comes back, flagged as not a fresh grant.
        let mut ws2 = dial(&proxy).await;
        ws2.send(Message::Text(
            json!({"type": "login", "email": email, "password": "pw12"}).to_string(),
        ))
        .await
        .unwrap();
        let again = recv_until(&mut ws2, "plot.assigned").await;
        assert_eq!(again["plot_id"], json!(plot_id), "reconnect should not grant a second plot");
        assert_eq!(again["just_claimed"], false);

        // Durable: only one plot is owned by this character.
        assert_eq!(
            db.plot_for_character(&pid).await.unwrap().map(|p| p.id),
            Some(plot_id)
        );

        drop(ws2);
    }

    /// Register a character and return `(player_id, plot bounds)`, having already
    /// drained the initial `spawn_entity` the registration itself sends to the
    /// fake zone — so a test's own `zone.from_proxy.recv()` sees only messages
    /// caused by what it does next.
    async fn registered_with_plot(
        proxy: &Arc<Proxy>,
        zone: &mut FakeZone,
        ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
        name: &str,
    ) -> (String, Value) {
        let email = format!("{name}_{}@t.test", Uuid::new_v4().simple());
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": name}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(ws, "welcome").await["player_id"].as_str().unwrap().to_string();
        let bounds = recv_until(ws, "plot.assigned").await["bounds"].clone();
        // Login hydration also pushes this character's (just-claimed) rent
        // status (#14) — drain it so a caller's next `recv_frame` isn't tripped
        // up by the leftover.
        recv_until(ws, "rent.status").await;
        let _ = proxy; // kept for symmetry with other test helpers that take it
        while zone.from_proxy.try_recv().is_ok() {}
        (pid, bounds)
    }

    /// Send a `build_place` and wait for it to land, draining the two frames a
    /// *successful* placement always produces on the client socket — `build.placed`
    /// and the district-wide `status_update` broadcast (#12/#13; order isn't
    /// guaranteed) — so a caller's next `recv_frame` isn't tripped up by a leftover.
    /// Also drains the matching `home_structure_added` pushed to the zone.
    async fn place_home_structure(
        zone: &mut FakeZone,
        ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
        pid: &str,
        kind: &str,
        x: i32,
        y: i32,
    ) -> Value {
        zone.to_proxy.send(Message::Text(json!({
            "type": "build_place", "player_id": pid, "kind": kind, "x": x, "y": y, "rot": 0,
        }).to_string())).unwrap();
        let (mut placed, mut saw_status) = (None, false);
        while placed.is_none() || !saw_status {
            let v = recv_frame(ws).await.expect("expected build.placed/status_update");
            match v["type"].as_str() {
                Some("build.placed") => placed = Some(v),
                Some("status_update") => saw_status = true,
                _ => {}
            }
        }
        recv_value(&mut zone.from_proxy).await; // home_structure_added
        placed.unwrap()
    }

    /// #12 acceptance: a player can place a bed, storage chest, and crafting
    /// station on their own plot; multiple structures of a kind are fine as long
    /// as they don't overlap, but placement outside the plot's bounds, or onto
    /// something already there, is a silent no-op.
    #[tokio::test]
    async fn build_place_validates_bounds_and_overlap_but_allows_multiple_per_kind() {
        let (proxy, dbf, mut zone) = proxy_with_db().await;
        let db = Db::connect(dbf.url()).await.unwrap();
        db.seed_capital(&mmo::world::capital(), 0).await.unwrap();

        let mut ws = dial(&proxy).await;
        let (pid, bounds) = registered_with_plot(&proxy, &mut zone, &mut ws, "Builder").await;
        let (bx, by) = (bounds["x"].as_i64().unwrap() as i32, bounds["y"].as_i64().unwrap() as i32);

        // A bed well inside the plot succeeds.
        let placed = place_home_structure(&mut zone, &mut ws, &pid, "bed", bx + 5, by + 5).await;
        assert_eq!(placed["structure"]["kind"], "bed");
        assert_eq!(placed["structure"]["x"], bx as i64 + 5);

        // A second, non-overlapping bed elsewhere on the same plot also succeeds
        // (multiple per kind are allowed — only overlap is rejected).
        let placed2 = place_home_structure(&mut zone, &mut ws, &pid, "bed", bx + 40, by + 40).await;
        assert_ne!(placed2["structure"]["id"], placed["structure"]["id"]);

        // Overlapping the first bed's footprint is a silent no-op.
        zone.to_proxy.send(Message::Text(json!({
            "type": "build_place", "player_id": pid, "kind": "storage", "x": bx + 10, "y": by + 10, "rot": 0,
        }).to_string())).unwrap();
        assert!(recv_frame(&mut ws).await.is_none(), "overlapping placement should not succeed");

        // Outside the plot's bounds entirely is also a silent no-op.
        zone.to_proxy.send(Message::Text(json!({
            "type": "build_place", "player_id": pid, "kind": "crafting", "x": 0, "y": 0, "rot": 0,
        }).to_string())).unwrap();
        assert!(recv_frame(&mut ws).await.is_none(), "placement off the owner's plot should not succeed");

        // Durable: exactly the two beds landed, nothing else.
        let plot = db.plot_for_character(&pid).await.unwrap().unwrap();
        let structures = db.structures_for_plot(&plot.id).await.unwrap();
        assert_eq!(structures.len(), 2);
        assert!(structures.iter().all(|s| s.kind == "bed"));

        drop(ws);
    }

    /// #13: the zone has no DB access, so the gateway pushes it the position of
    /// every newly-placed structure (`home_structure_added`) — the mechanism that
    /// lets the zone gate deposit/withdraw/craft on proximity to the *specific*
    /// structure rather than just "on some plot".
    #[tokio::test]
    async fn build_place_pushes_the_new_structure_to_the_owning_zone() {
        let (proxy, dbf, mut zone) = proxy_with_db().await;
        let db = Db::connect(dbf.url()).await.unwrap();
        db.seed_capital(&mmo::world::capital(), 0).await.unwrap();

        let mut ws = dial(&proxy).await;
        let (pid, bounds) = registered_with_plot(&proxy, &mut zone, &mut ws, "Pusher").await;
        let (bx, by) = (bounds["x"].as_i64().unwrap() as i32, bounds["y"].as_i64().unwrap() as i32);

        zone.to_proxy.send(Message::Text(json!({
            "type": "build_place", "player_id": pid, "kind": "crafting", "x": bx + 5, "y": by + 5, "rot": 0,
        }).to_string())).unwrap();
        let placed = recv_until(&mut ws, "build.placed").await;
        let structure_id = placed["structure"]["id"].as_str().unwrap().to_string();

        let pushed = recv_value(&mut zone.from_proxy).await;
        assert_eq!(pushed["type"], "home_structure_added");
        assert_eq!(pushed["id"], structure_id);
        assert_eq!(pushed["kind"], "crafting");
        assert_eq!(pushed["x"], bx as i64 + 5);
        assert_eq!(pushed["y"], by as i64 + 5);

        drop(ws);
    }

    /// #12/#13 acceptance: crafting a basic item requires owning a crafting
    /// station on your own plot and having the ingredients; either gap is a
    /// silent no-op, and a successful craft debits inputs, credits the output
    /// atomically, and grants crafting XP.
    #[tokio::test]
    async fn craft_make_requires_a_station_and_ingredients() {
        let (proxy, dbf, mut zone) = proxy_with_db().await;
        let db = Db::connect(dbf.url()).await.unwrap();
        db.seed_capital(&mmo::world::capital(), 0).await.unwrap();

        let mut ws = dial(&proxy).await;
        let (pid, bounds) = registered_with_plot(&proxy, &mut zone, &mut ws, "Crafter").await;
        let (bx, by) = (bounds["x"].as_i64().unwrap() as i32, bounds["y"].as_i64().unwrap() as i32);

        // Stock plenty of wood, as gathering would. This also emits a gathering
        // skill.update alongside inv.update (order isn't guaranteed) — drain both
        // before asserting silence in the next step.
        zone.to_proxy.send(Message::Text(json!({
            "type": "gather_yield", "player_id": pid,
            "item_id": "wood", "qty": 4, "skill": "gathering", "xp": 1,
        }).to_string())).unwrap();
        let (mut saw_inv, mut saw_skill) = (false, false);
        while !(saw_inv && saw_skill) {
            match recv_frame(&mut ws).await.expect("expected inv.update/skill.update")["type"].as_str() {
                Some("inv.update") => saw_inv = true,
                Some("skill.update") => saw_skill = true,
                _ => {}
            }
        }

        // No crafting station yet: craft.make is a no-op.
        zone.to_proxy.send(Message::Text(json!({
            "type": "craft_make", "player_id": pid, "recipe_id": "plank",
        }).to_string())).unwrap();
        assert!(recv_frame(&mut ws).await.is_none(), "no station should mean no craft");

        // Place the station, then craft succeeds (plank needs 2 wood).
        place_home_structure(&mut zone, &mut ws, &pid, "crafting", bx + 5, by + 5).await;

        zone.to_proxy.send(Message::Text(json!({
            "type": "craft_make", "player_id": pid, "recipe_id": "plank",
        }).to_string())).unwrap();
        // craft.made, inv.update, and the crafting skill.update interleave in any
        // order — scan all three before asserting silence in the next step.
        let (mut made, mut inv, mut skill) = (None, None, None);
        while made.is_none() || inv.is_none() || skill.is_none() {
            let v = recv_frame(&mut ws).await.expect("expected craft.made/inv.update/skill.update");
            match v["type"].as_str() {
                Some("craft.made") => made = Some(v),
                Some("inv.update") => inv = Some(v),
                Some("skill.update") if v["skill_id"] == "crafting" => skill = Some(v),
                _ => {}
            }
        }
        let made = made.unwrap();
        assert_eq!(made["item_id"], "plank");
        assert_eq!(made["qty"], 2);
        let items = inv.unwrap()["items"].as_array().cloned().unwrap_or_default();
        assert_eq!(items.iter().find(|it| it["item_id"] == "wood").unwrap()["qty"], 2, "2 wood debited");
        assert_eq!(items.iter().find(|it| it["item_id"] == "plank").unwrap()["qty"], 2, "2 plank credited");
        assert_eq!(
            skill.unwrap()["xp"], mmo::persistence::CRAFT_XP_PER_CRAFT,
            "a successful craft grants crafting XP"
        );

        // Insufficient ingredients now (only 2 wood left, tool_kit needs wood+stone): no-op.
        zone.to_proxy.send(Message::Text(json!({
            "type": "craft_make", "player_id": pid, "recipe_id": "tool_kit",
        }).to_string())).unwrap();
        assert!(recv_frame(&mut ws).await.is_none(), "missing stone should mean no craft");

        drop(ws);
    }

    /// #12 acceptance: a player who has set a bed respawns exactly at it (even
    /// though the death is reported by a zone that doesn't know where beds are);
    /// without a bed set, death falls back to the default town-centre spawn.
    #[tokio::test]
    async fn player_died_respawns_at_the_set_bed_or_falls_back_to_town_centre() {
        let (proxy, dbf, mut zone) = proxy_with_db().await;
        let db = Db::connect(dbf.url()).await.unwrap();
        db.seed_capital(&mmo::world::capital(), 0).await.unwrap();

        let mut ws = dial(&proxy).await;
        let (pid, bounds) = registered_with_plot(&proxy, &mut zone, &mut ws, "Sleeper").await;
        let (bx, by) = (bounds["x"].as_i64().unwrap() as i32, bounds["y"].as_i64().unwrap() as i32);

        // Fall back to the town centre before any bed is set.
        zone.to_proxy.send(Message::Text(
            json!({"type": "player_died", "player_id": pid, "hp": 100}).to_string(),
        )).unwrap();
        let spawn = recv_value(&mut zone.from_proxy).await;
        assert_eq!(spawn["type"], "spawn_entity");
        assert_eq!(spawn["x"], SPAWN_X as i64);
        assert_eq!(spawn["y"], SPAWN_Y as i64);

        // Place a bed and claim it as the respawn point.
        let (bed_x, bed_y) = (bx + 6, by + 6);
        let placed = place_home_structure(&mut zone, &mut ws, &pid, "bed", bed_x, bed_y).await;
        let bed_id = placed["structure"]["id"].as_str().unwrap().to_string();

        ws.send(Message::Text(json!({"type": "home.set_respawn", "bed_id": bed_id}).to_string()))
            .await
            .unwrap();
        let ack = recv_until(&mut ws, "home.respawn_set").await;
        assert_eq!(ack["bed_id"], bed_id);

        // Die again: this time respawn lands exactly at the bed.
        zone.to_proxy.send(Message::Text(
            json!({"type": "player_died", "player_id": pid, "hp": 100}).to_string(),
        )).unwrap();
        let spawn2 = recv_value(&mut zone.from_proxy).await;
        assert_eq!(spawn2["type"], "spawn_entity");
        assert_eq!(spawn2["x"], bed_x as i64);
        assert_eq!(spawn2["y"], bed_y as i64);

        drop(ws);
    }

    /// #14 acceptance, end to end: a plot's rent warns, lapses, then reclaims —
    /// the plot returns to the pool (another character can claim it), the
    /// former owner's flair survives (unattached, not deleted), and their
    /// character-global storage is untouched throughout (it was never
    /// plot-scoped to begin with, #12/#13).
    #[tokio::test]
    async fn rent_warns_lapses_and_reclaims_returning_the_plot_to_the_pool() {
        let (proxy, dbf, mut zone) = proxy_with_db().await;
        let db = Db::connect(dbf.url()).await.unwrap();
        db.seed_capital(&mmo::world::capital(), 0).await.unwrap();

        let mut ws = dial(&proxy).await;
        let (pid, bounds) = registered_with_plot(&proxy, &mut zone, &mut ws, "Tenant").await;
        let (bx, by) = (bounds["x"].as_i64().unwrap() as i32, bounds["y"].as_i64().unwrap() as i32);
        let plot = db.plot_for_character(&pid).await.unwrap().unwrap();
        let due_at = plot.rent_due_at.unwrap();

        // A bed (so we can prove it gets demolished) and some flair + storage
        // (so we can prove those *don't*).
        let placed = place_home_structure(&mut zone, &mut ws, &pid, "bed", bx + 5, by + 5).await;
        let bed_id = placed["structure"]["id"].as_str().unwrap().to_string();
        let flair_id = db.add_flair(&pid, Some(&plot.id), "rug", 1, 1, 0).await.unwrap();
        db.deposit_to_storage(&pid, "wood", 10).await.unwrap();

        // Tick 1: just inside the warning window — one rent.warning, nothing else.
        proxy.tick_rent(due_at - RENT_WARNING_LEAD_SECS + 10).await;
        let warning = recv_until(&mut ws, "rent.warning").await;
        assert_eq!(warning["plot_id"], plot.id);
        assert!(db.load_plot(&plot.id).await.unwrap().unwrap().warned);

        // Tick 2: past due (no auto-pay set) — lapses; rent.status reflects it.
        proxy.tick_rent(due_at + 1).await;
        let status = recv_until(&mut ws, "rent.status").await;
        assert_eq!(status["plot_id"], plot.id);
        assert_eq!(status["state"], "lapsed");

        // Tick 3: past the grace window — reclaimed. The bed despawns, the zone
        // drops it from its proximity cache, and the former owner is notified.
        proxy.tick_rent(due_at + RENT_GRACE_SECS + 1).await;
        let mut saw_despawn = false;
        let mut reclaimed = None;
        while reclaimed.is_none() {
            let v = recv_frame(&mut ws).await.expect("expected despawn/rent.reclaimed");
            match v["type"].as_str() {
                Some("despawn") if v["player_id"] == json!(bed_id) => saw_despawn = true,
                Some("rent.reclaimed") => reclaimed = Some(v),
                _ => {}
            }
        }
        assert!(saw_despawn, "the demolished bed should despawn for onlookers (including the owner)");
        let reclaimed = reclaimed.unwrap();
        assert_eq!(reclaimed["plot_id"], plot.id);
        assert_eq!(reclaimed["moved_to_storage"], json!([]));

        let removed = recv_value(&mut zone.from_proxy).await;
        assert_eq!(removed["type"], "home_structure_removed");
        assert_eq!(removed["id"], bed_id);

        // The plot is back in the pool: no owner, and durably reclaimed.
        assert!(db.plot_for_character(&pid).await.unwrap().is_none());
        assert_eq!(db.load_plot(&plot.id).await.unwrap().unwrap().state, "reclaimed");

        // Another character can claim the very same plot.
        let mut ws2 = dial(&proxy).await;
        let (_pid2, bounds2) = registered_with_plot(&proxy, &mut zone, &mut ws2, "NextTenant").await;
        assert_eq!(bounds2, bounds, "the reclaimed plot is claimable again, at the same spot");

        // The original owner keeps everything they *owned*: flair survives
        // (unattached, not deleted), and storage was never at risk.
        let flair = db.flair_for_character(&pid).await.unwrap();
        assert_eq!(flair.len(), 1);
        assert_eq!(flair[0].id, flair_id);
        assert_eq!(flair[0].plot_id, None);
        let stash = db.storage_for_character(&pid).await.unwrap();
        assert_eq!(stash.iter().find(|i| i.item_id == "wood").unwrap().qty, 10);

        drop(ws);
        drop(ws2);
    }

    /// #15 acceptance: the actual position/zone handoff already happens via the
    /// ordinary migrate-request path (unchanged) — `district.enter` is purely the
    /// client-facing load/ready handshake for the transition curtain: it refreshes
    /// district-scoped content (the build board, for wherever the player actually
    /// is) and acks so the client knows it can drop the curtain.
    #[tokio::test]
    async fn district_enter_refreshes_the_build_board_and_acks_ready() {
        let (proxy, dbf, _zone) = proxy_with_db().await;
        let db = Db::connect(dbf.url()).await.unwrap();
        db.seed_capital(&mmo::world::capital(), 0).await.unwrap();

        let email = format!("d_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Traveler"}).to_string(),
        ))
        .await
        .unwrap();
        recv_until(&mut ws, "welcome").await;

        ws.send(Message::Text(
            json!({"type": "district.enter", "from": "suburbs", "to": "civic"}).to_string(),
        ))
        .await
        .unwrap();

        // build.list and district.ready interleave in either order — scan both.
        let (mut saw_orders, mut saw_ready) = (false, false);
        while !(saw_orders && saw_ready) {
            let v = recv_frame(&mut ws).await.expect("expected build.list/district.ready");
            match v["type"].as_str() {
                Some("build.list") => {
                    let orders = v["orders"].as_array().cloned().unwrap_or_default();
                    assert!(orders.iter().any(|o| o["kind"] == "town_well"), "the civic board's content");
                    saw_orders = true;
                }
                Some("district.ready") => saw_ready = true,
                _ => {}
            }
        }

        drop(ws);
    }

    /// #18: `plot.district` reports every plot in the requester's current
    /// district (owned or not, with the owner's name resolved), and a new
    /// claim broadcasts a refreshed roster to everyone else already standing
    /// in that district — not just on their next login/district-crossing.
    #[tokio::test]
    async fn plot_district_roster_shows_every_plot_and_broadcasts_on_claim() {
        let (proxy, dbf, _zone) = proxy_with_db().await;
        let db = Db::connect(dbf.url()).await.unwrap();
        db.seed_capital(&mmo::world::capital(), 0).await.unwrap();

        // The harness's default zone spans the whole world, whose region
        // *centre* resolves to Civic (no plot grid there). Add a second zone
        // that actually covers the Suburbs, so a client tracked there sees
        // the real roster.
        let _suburbs_zone = add_zone_region(&proxy, "z_suburbs", Region { x0: 4800, y0: 0, x1: 6400, y1: 6400 });

        let email1 = format!("landowner1_{}@t.test", Uuid::new_v4().simple());
        let mut ws1 = dial(&proxy).await;
        ws1.send(Message::Text(
            json!({"type": "register", "email": email1, "password": "pw12", "name": "Homesteader"}).to_string(),
        ))
        .await
        .unwrap();
        let welcome1 = recv_until(&mut ws1, "welcome").await;
        let pid1 = welcome1["player_id"].as_str().unwrap().to_string();
        recv_until(&mut ws1, "plot.assigned").await; // their own starter-plot grant
        // Claiming that plot also broadcasts a `plot.district` (their own
        // registration counts as "a plot changed hands" too) — drain it
        // before requesting a fresh one, so later reads can't mistake it for
        // the second player's later live update.
        recv_until(&mut ws1, "plot.district").await;

        // `current_zone` drives broadcast *reachability* (is this client in a
        // zone touching the district), but `plot.district`'s own content is
        // resolved from the player's actual cached position — move both into
        // the Suburbs so the roster request below reflects it.
        proxy.clients.lock().unwrap().get_mut(&pid1).unwrap().current_zone = "z_suburbs".to_string();
        proxy.entity_state.lock().unwrap().insert(
            pid1.clone(),
            EntityCache { x: 5000, y: 3000, hp: 100, gather: None },
        );

        ws1.send(Message::Text(json!({"type": "plot.district"}).to_string())).await.unwrap();
        let roster1 = recv_until(&mut ws1, "plot.district").await;
        let plots1 = roster1["plots"].as_array().unwrap();
        assert!(plots1.len() >= 2, "this player's plot plus at least one still-free one");
        let mine = plots1.iter().find(|p| p["owner_id"] == pid1).expect("my own claimed plot appears");
        assert_eq!(mine["owner_name"], "Homesteader");
        assert!(plots1.iter().any(|p| p["owner_name"].is_null()), "at least one free plot, no owner");

        // A second character logging in claims another suburbs plot — the
        // first client (still in the suburbs shard) should see it live.
        let email2 = format!("landowner2_{}@t.test", Uuid::new_v4().simple());
        let mut ws2 = dial(&proxy).await;
        ws2.send(Message::Text(
            json!({"type": "register", "email": email2, "password": "pw12", "name": "Newcomer"}).to_string(),
        ))
        .await
        .unwrap();
        recv_until(&mut ws2, "welcome").await;

        let roster2 = recv_until(&mut ws1, "plot.district").await;
        let plots2 = roster2["plots"].as_array().unwrap();
        assert!(
            plots2.iter().any(|p| p["owner_name"] == "Newcomer"),
            "the first client sees the second player's new plot live, without re-requesting"
        );

        drop(ws1);
        drop(ws2);
    }

    /// #35 regression: in a *single* zone spanning the whole world (the
    /// common small/dev deployment — no auto-scaling split has happened),
    /// `district_for_zone`'s region-*centre* resolution always reports Civic,
    /// no matter where the player actually is (there's only one zone, so
    /// `current_zone` never changes as they walk around). Left as the roster
    /// resolution strategy, this silently overwrites the correct Suburbs
    /// roster a player already has (from `send_plot`'s claim broadcast) with
    /// an empty one the moment anything re-requests it (`district.enter`, an
    /// explicit refresh) — the exact "I can't see my own plot" bug. The fix:
    /// resolve from the player's actual cached position instead.
    #[tokio::test]
    async fn plot_district_resolves_by_actual_position_not_zone_centre() {
        let (proxy, dbf, _zone) = proxy_with_db().await;
        let db = Db::connect(dbf.url()).await.unwrap();
        db.seed_capital(&mmo::world::capital(), 0).await.unwrap();

        let email = format!("wanderer_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Wanderer"}).to_string(),
        ))
        .await
        .unwrap();
        let welcome = recv_until(&mut ws, "welcome").await;
        let pid = welcome["player_id"].as_str().unwrap().to_string();
        recv_until(&mut ws, "plot.assigned").await;
        recv_until(&mut ws, "plot.district").await; // their own claim's broadcast

        // Still tracked by the one default zone (whole-world region, centre
        // in Civic) — only their cached position says otherwise.
        assert_eq!(
            proxy.clients.lock().unwrap().get(&pid).unwrap().current_zone,
            "zone_a"
        );
        proxy.entity_state.lock().unwrap().insert(
            pid.clone(),
            EntityCache { x: 5200, y: 3000, hp: 100, gather: None },
        );

        ws.send(Message::Text(json!({"type": "plot.district"}).to_string())).await.unwrap();
        let roster = recv_until(&mut ws, "plot.district").await;
        let plots = roster["plots"].as_array().unwrap();
        assert!(
            plots.iter().any(|p| p["owner_id"] == pid),
            "the Suburbs roster (240 plots incl. their own), not Civic's empty one, \
             even though the zone's region-centre resolves to Civic"
        );

        drop(ws);
    }
}
