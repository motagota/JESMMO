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

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
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
use mmo::persistence::{self, Db};
use mmo::protocol::{self, PROTOCOL_VERSION};
use mmo::util::{dist2, now_secs, random_heading};

/// Spawn point for a brand-new character: the capital's town centre (the spawn
/// anchor authored in `mmo::world`). Kept in sync via `spawn_matches_town_centre`.
const SPAWN_X: i32 = WORLD_SIZE / 2;
const SPAWN_Y: i32 = WORLD_SIZE / 2;
const SPAWN_HP: i32 = 100;

/// The one seeded mayor login — a normal account, just with `role = 'mayor'`, so
/// there's always a known way to commission city build orders.
const MAYOR_EMAIL: &str = "mayor@capital.town";
const MAYOR_PASSWORD: &str = "mayor12345";

/// The one seeded editor login (terrain editing, epic #72) — same pattern as
/// the mayor: a normal account with `role = 'editor'`, gating `terrain.edit_op`.
const EDITOR_EMAIL: &str = "editor@capital.town";
const EDITOR_PASSWORD: &str = "editor12345";

/// Hard cap on a corner's TOTAL accumulated height offset (base ± 50m) — the
/// server-side safety envelope for `terrain.edit_op`, checked against the
/// stored delta plus the op's increment. A per-op increment beyond this is
/// rejected outright too. Lives here as a const rather than a runtime TOML
/// because the server loads no runtime TOML today (brush *feel* params are
/// client-side, `config/editor/brushes.toml`); promote to config if Phase 2
/// player terraforming ever needs per-scope caps.
const EDIT_MAX_OFFSET_CM: i32 = 5_000;
/// Cells per op cap: a 64m-radius brush at 5m cells touches ~660 corners, and
/// even a stroke dragged across a whole 129-corner chunk is ~16k — anything
/// bigger is malformed or abusive, not a real stroke.
const EDIT_MAX_CELLS_PER_OP: usize = 16_384;

/// Object kinds an editor may place (#85). The gateway validates placement
/// against this registry; gameplay semantics attach elsewhere (the poison
/// tick, #88, reads `poison_tree` positions from the object cache).
const OBJECT_KINDS: &[&str] = &["poison_tree"];

/// Road plans (#94): stone cost per metre of laid path — 1 stone per 4m,
/// with a floor so even a stub road costs something. Consts like every
/// other tuning knob; the client mirrors them for display only.
const ROAD_STONE_PER_M_NUM: i64 = 1;
const ROAD_STONE_PER_M_DEN: i64 = 4;
const ROAD_MIN_STONE: i64 = 5;
/// Total path length cap — a single plan longer than this is a mis-drag or
/// abuse, not a road (lay long routes as multiple plans).
const ROAD_MAX_LENGTH_M: i64 = 4_000;
/// Points cap: each point past the first is a
/// corner; a real road plan has a handful, not hundreds.
const ROAD_MAX_POINTS: usize = 64;

/// Environmental tick cadence (#87): how often every connected player's
/// environment flags (submerged; poison sources, #88) are recomputed and
/// pushed to their owning zone. The push is unconditional each tick, not
/// on-change: at human player counts the traffic is trivial, and it makes
/// entity recreation (split/merge/respawn/migrate resets zone-side flags to
/// their defaults) self-heal within a second with zero bookkeeping.
const ENV_TICK_INTERVAL: Duration = Duration::from_secs(1);
/// A poison tree poisons within this many metres (#88). Matches the object
/// tool's world scale (1 unit = 1m); the zone turns the resulting source
/// count into buildup/proc/DoT.
const POISON_RADIUS_M: i64 = 15;
/// Depth clause of the submerged test: composited ground more than this
/// below sea level counts as underwater even OUTSIDE the baked water mask —
/// it's what makes an editor-dug pond drown. Inside the mask the depth is
/// irrelevant (see `env_tick_once`: the river/bay bed is mostly the LiDAR
/// NoData fill at exactly 0m, so a depth-only rule would make most of the
/// river non-drowning — being in the water is the signal, per the original
/// design: "goes in water → hold breath").
const SUBMERGED_DEPTH_M: f32 = 1.5;

/// Must be within this of a build board — or a build order's own placement — to
/// contribute to it.
const BOARD_RANGE: i32 = 60;

/// Squared distance from `(px,py)` to the segment `(x0,y0)-(x1,y1)` (clamped
/// projection), for gating proximity to a segment-shaped structure like a road.
fn point_segment_dist2(px: i32, py: i32, x0: i32, y0: i32, x1: i32, y1: i32) -> i64 {
    let (dx, dy) = (x1 - x0, y1 - y0);
    let len2 = (dx as i64).pow(2) + (dy as i64).pow(2);
    if len2 == 0 {
        return dist2(px, py, x0, y0);
    }
    let t = (((px - x0) as i64 * dx as i64 + (py - y0) as i64 * dy as i64) as f64 / len2 as f64)
        .clamp(0.0, 1.0);
    let cx = x0 as f64 + t * dx as f64;
    let cy = y0 as f64 + t * dy as f64;
    let (ddx, ddy) = (px as f64 - cx, py as f64 - cy);
    (ddx * ddx + ddy * ddy) as i64
}

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
use mmo::world::WORLD_SIZE;

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
    /// Cached from the account at login (`"player"`, `"mayor"`, or
    /// `"editor"`) so gating `mayor.build_create` / `terrain.edit_op`
    /// doesn't need a DB round trip per message.
    role: String,
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
    /// `"player"` (default), `"mayor"` (gates `mayor.build_create`), or
    /// `"editor"` (gates `terrain.edit_op`).
    role: String,
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
    /// Live placed world props (#85), keyed by object id — the source every
    /// `object.list` answer and (#88) poison-proximity check reads, so neither
    /// touches the DB. Lazily hydrated from `world_object` on first use (tests
    /// drive handlers without `start()`, so boot-time-only loading would miss
    /// them) and write-through on every accepted place/delete.
    world_objects: tokio::sync::OnceCell<Mutex<HashMap<String, persistence::WorldObject>>>,
    /// Serializes `terrain.edit_op` application (terrain editing #72): each op
    /// is a read-modify-write of its chunks' delta rows, and two concurrent
    /// editors interleaving load/save would silently drop one's cells. Edits
    /// are human-rate, so one async lock across the whole apply is plenty.
    terrain_edit_lock: tokio::sync::Mutex<()>,
}

/// Cap on the rolling DB-latency sample window (#16) — recent-enough to be a
/// useful health signal without growing unbounded.
const DB_LATENCY_SAMPLES: usize = 50;
/// Window for the "rent reclaims" ops counter.
const RECLAIM_LOG_WINDOW_SECS: i64 = 24 * 3600;

/// Render a build order as the client-facing board entry used by `build.list`.
fn build_order_json(o: &mmo::persistence::BuildOrder) -> Value {
    let mut v = json!({
        "order_id": o.id,
        "kind": o.kind,
        "required": serde_json::from_str::<Value>(&o.required_json).unwrap_or_else(|_| json!({})),
        "progress": serde_json::from_str::<Value>(&o.progress_json).unwrap_or_else(|_| json!({})),
        "state": o.state,
        // Skill gate (0 = ungated). The client greys the order and shows
        // "requires <skill> <level>" for players below the threshold.
        "required_skill": o.required_skill,
        "required_level": o.required_level,
    });
    // Road orders (#94/#95): the full grid path, so every client can render
    // the staked (accepted-but-unbuilt) plan and know where to haul stone.
    if let Some(path) = o.path_json.as_deref().and_then(|p| serde_json::from_str::<Value>(p).ok()) {
        v["path"] = path;
    }
    v
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
/// A live-render `status_update` for a completed build order's own placement.
/// `path_json` (road orders, #96) carries the full multi-run grid path so the
/// client renders the whole road, not just the first-run segment.
fn structure_status_json(kind: &str, p: &mmo::persistence::BuildPlacement, path_json: Option<&str>) -> Value {
    let mut v = json!({
        "type": "status_update",
        "player_id": format!("structure_{}", kind),
        "state": {
            "x": p.x, "y": p.y, "x1": p.x1, "y1": p.y1,
            "type": "structure", "kind": p.structure_kind, "facing": [0, 0],
        },
    });
    if let Some(path) = path_json.and_then(|s| serde_json::from_str::<Value>(s).ok()) {
        v["state"]["path"] = path;
    }
    v
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
            terrain_edit_lock: tokio::sync::Mutex::new(()),
            world_objects: tokio::sync::OnceCell::new(),
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
                        let ch = if kind == protocol::C_REGISTER {
                            let name = data.get("name").and_then(|v| v.as_str()).unwrap_or("");
                            auth::register(
                                db, email, password, name,
                                SPAWN_X as i64, SPAWN_Y as i64, SPAWN_HP as i64,
                            )
                            .await
                        } else {
                            auth::login(db, email, password).await
                        };
                        match ch {
                            Ok(ch) => {
                                let role = db
                                    .role_for_account(&ch.account_id)
                                    .await
                                    .unwrap_or_else(|_| "player".to_string());
                                Ok(persistent_identity(ch, role))
                            }
                            Err(e) => Err(e),
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
        let role = db
            .role_for_account(&ch.account_id)
            .await
            .unwrap_or_else(|_| "player".to_string());
        Some(persistent_identity(ch, role))
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
        // District from the player's cached POSITION, not the zone's region
        // centre — `district_for_zone` only tells districts apart when each
        // has its own shard (see `send_plot_roster`'s doc for the full
        // reasoning; found again in #94 when a post-split town-centre player
        // was handed a neighbouring district's board, hiding staked road
        // plans). Zone fallback only for a client with no cached position yet.
        let by_position = self
            .entity_state
            .lock()
            .unwrap()
            .get(pid)
            .map(|c| (c.x, c.y))
            .and_then(|(x, y)| self.capital.district_at(x, y).map(|d| d.id.to_string()));
        let district = match by_position {
            Some(d) => d,
            None => {
                let zone_id = match self.clients.lock().unwrap().get(pid).map(|i| i.current_zone.clone()) {
                    Some(z) => z,
                    None => return,
                };
                match self.district_for_zone(&zone_id) {
                    Some(d) => d,
                    None => return,
                }
            }
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
        for d in &self.capital.districts {
            let Ok(orders) = db.build_orders_for_district(d.id).await else { continue };
            for o in orders.iter().filter(|o| o.state == "completed") {
                if let Some(p) = o.placement() {
                    self.push_to_player(pid, structure_status_json(&o.kind, &p, o.path_json.as_deref()));
                }
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
        // Proximity gate: near a build board in the order's district, or near the
        // order's own placement (e.g. a mayor-commissioned dirt path built well away
        // from the civic board). The gateway's live position cache (updated on every
        // status_update, same as the zone's tick) is fresh enough for this check.
        let Ok(Some(order)) = db.build_order_by_id(order_id).await else { return };
        let Some((px, py)) = self.entity_state.lock().unwrap().get(pid).map(|c| (c.x, c.y)) else { return };
        let near_board = self
            .capital
            .districts
            .iter()
            .find(|d| d.id == order.district)
            .map(|d| self.capital.build_boards_in(d.region))
            .unwrap_or_default()
            .iter()
            .any(|b| dist2(px, py, b.x, b.y) <= (BOARD_RANGE as i64).pow(2));
        let mut near_order = match (order.x, order.y) {
            (Some(ox), Some(oy)) => {
                let d2 = match (order.x1, order.y1) {
                    (Some(ox1), Some(oy1)) => point_segment_dist2(
                        px, py, ox as i32, oy as i32, ox1 as i32, oy1 as i32,
                    ),
                    _ => dist2(px, py, ox as i32, oy as i32),
                };
                d2 <= (BOARD_RANGE as i64).pow(2)
            }
            _ => false,
        };
        // Road orders (#96): near ANY run of the stored path counts — a
        // multi-run road is buildable from its far end, not just from the
        // first run the placement columns carry.
        if !near_order {
            if let Some(runs) = order
                .path_json
                .as_deref()
                .and_then(|p| serde_json::from_str::<Vec<(i64, i64)>>(p).ok())
            {
                near_order = runs.windows(2).any(|w| {
                    point_segment_dist2(px, py, w[0].0 as i32, w[0].1 as i32, w[1].0 as i32, w[1].1 as i32)
                        <= (BOARD_RANGE as i64).pow(2)
                });
            }
        }
        if !near_board && !near_order {
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
        // A completed DEMOLITION order (#106) tears its target road out and
        // pays the salvage before the ordinary completion announcements (so
        // the board refresh at the end already shows the road gone). Demo
        // orders carry no placement, so no structure spawns from them.
        if res.kind.starts_with("demo_") {
            self.finish_demolition(&db, &res.kind, &res.contributors).await;
        }
        self.announce_order_completion(
            &db, order_id, &res.kind, &res.district,
            &res.contributors, res.placement.as_ref(), order.path_json.as_deref(),
        )
        .await;
    }

    /// Everything that happens when an order completes, AFTER the durable
    /// state flip: contributor XP, the `build.completed` broadcast, the
    /// structure render push, dependent unlocks, and the board refresh.
    /// Shared by the ordinary contribute path and `road.replan`'s
    /// covered-by-kept-progress edge (#104) so a completion is a completion,
    /// whichever door it came through.
    async fn announce_order_completion(
        &self,
        db: &Db,
        order_id: &str,
        kind: &str,
        district: &str,
        contributors: &[(String, i64)],
        placement: Option<&mmo::persistence::BuildPlacement>,
        path_json: Option<&str>,
    ) {
        // Lump-sum building XP to each contributor, split by units contributed.
        for (cid, units) in contributors {
            let amount = units * mmo::persistence::BUILD_XP_PER_UNIT;
            if let Ok(gain) = db.grant_skill_xp(cid, "building", amount).await {
                self.push_skill_gain(cid, &gain);
            }
        }

        // This order's own placement (set at creation — mayor-commissioned or authored).
        let structures: Vec<Value> = placement
            .iter()
            .map(|p| json!({"kind": p.structure_kind, "x": p.x, "y": p.y, "x1": p.x1, "y1": p.y1}))
            .collect();
        self.broadcast_to_district(district, json!({
            "type": "build.completed", "order_id": order_id, "structures": structures,
        }));
        // Render the new structure for every connected client (path_json:
        // roads render their full multi-run path, #96).
        if let Some(p) = placement {
            let entity = structure_status_json(kind, p, path_json).to_string();
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
            .filter(|o| o.prereq == Some(kind))
            .map(|o| (o.district, o.kind))
            .collect();
        let mut unlocked_ids: Vec<String> = Vec::new();
        for (d, k) in dependents {
            if let Ok(Some(o)) = db.open_build_order(d, k).await {
                unlocked_ids.push(o.id);
            }
        }
        if !unlocked_ids.is_empty() {
            self.broadcast_to_district(district, json!({
                "type": "build.unlocked", "order_ids": unlocked_ids,
            }));
        }
        // Refresh the board for the district (the newly opened orders now appear).
        self.broadcast_build_list(district).await;
    }

    /// Whether `(x,y)` falls inside a currently-owned plot — i.e. is *not*
    /// city-owned land. Districts without an authored plot grid (everywhere but
    /// the suburbs today) have no ownable plots at all, so every point in them is
    /// city land.
    async fn on_owned_plot(&self, x: i32, y: i32, db: &Db) -> bool {
        let Some(district) = self.capital.district_at(x, y) else { return false };
        if district.plot_grid.is_none() {
            return false;
        }
        let cells = district.plots();
        let Ok(rows) = db.plots_for_district(district.id).await else { return false };
        rows.iter().filter(|p| p.owner_character_id.is_some()).any(|p| {
            cells.iter().any(|c| {
                c.grid_x as i64 == p.grid_x && c.grid_y as i64 == p.grid_y && c.rect().contains(x, y)
            })
        })
    }

    /// Handle `mayor.build_create`: only the seeded mayor account may commission
    /// city work, and only on city-owned land (not inside anyone's claimed plot).
    /// Otherwise this mirrors authored seeding — an open build order any player
    /// can then contribute to via the ordinary `build.contribute` path.
    async fn apply_mayor_build_create(&self, pid: &str, data: Value) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        let role = self.clients.lock().unwrap().get(pid).map(|c| c.role.clone()).unwrap_or_default();
        if role != "mayor" {
            self.push_to_player(pid, json!({
                "type": protocol::S_MAYOR_BUILD_ERROR,
                "message": "only the mayor may commission city work",
            }));
            return;
        }

        let district = data.get("district").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let kind = data.get("kind").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let structure_kind = data.get("structure_kind").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let required_json = data.get("required_json").and_then(|v| v.as_str()).unwrap_or("{}").to_string();
        let (Some(x), Some(y)) = (
            data.get("x").and_then(|v| v.as_i64()),
            data.get("y").and_then(|v| v.as_i64()),
        ) else {
            self.push_to_player(pid, json!({
                "type": protocol::S_MAYOR_BUILD_ERROR, "message": "x/y are required",
            }));
            return;
        };
        let x1 = data.get("x1").and_then(|v| v.as_i64());
        let y1 = data.get("y1").and_then(|v| v.as_i64());
        if district.is_empty() || kind.is_empty() || structure_kind.is_empty() {
            self.push_to_player(pid, json!({
                "type": protocol::S_MAYOR_BUILD_ERROR,
                "message": "district, kind, and structure_kind are required",
            }));
            return;
        }

        // City land only: check the start point, the end point (for a segment),
        // and its midpoint against every currently-owned plot.
        let mut check_points = vec![(x as i32, y as i32)];
        if let (Some(x1), Some(y1)) = (x1, y1) {
            check_points.push((x1 as i32, y1 as i32));
            check_points.push((((x + x1) / 2) as i32, ((y + y1) / 2) as i32));
        }
        for (px, py) in check_points {
            if self.on_owned_plot(px, py, &db).await {
                self.push_to_player(pid, json!({
                    "type": protocol::S_MAYOR_BUILD_ERROR,
                    "message": "that land is privately owned",
                }));
                return;
            }
        }

        let placement = Some(mmo::persistence::BuildPlacement { structure_kind, x, y, x1, y1 });
        match db.insert_build_order(&district, &kind, &required_json, "open", now_secs(), None, 0, placement, None).await {
            Ok(_) => self.broadcast_build_list(&district).await,
            Err(_) => self.push_to_player(pid, json!({
                "type": protocol::S_MAYOR_BUILD_ERROR, "message": "failed to create the order",
            })),
        }
    }

    /// Parse + validate a road path payload (`points`, shared by `road.plan`
    /// and `road.replan` #104): a lattice polyline whose consecutive pairs
    /// are free-angle segments (#111 — the client splines through them),
    /// in-world, under the point/length caps, and not
    /// crossing privately owned land. Returns `(points, length_m)` or the
    /// `road.plan_error` message.
    async fn parse_road_path(&self, db: &Db, data: &Value) -> Result<(Vec<(i64, i64)>, i64), &'static str> {
        let Some(raw) = data.get("points").and_then(|v| v.as_array()) else {
            return Err("malformed road plan (points required)");
        };
        if raw.len() < 2 {
            return Err("a road needs at least two points");
        }
        if raw.len() > ROAD_MAX_POINTS {
            return Err("too many corners in one plan");
        }
        let world = WORLD_SIZE as i64;
        let mut points: Vec<(i64, i64)> = Vec::with_capacity(raw.len());
        for p in raw {
            let (Some(x), Some(y)) = (
                p.get(0).and_then(|v| v.as_i64()),
                p.get(1).and_then(|v| v.as_i64()),
            ) else {
                return Err("malformed point (want [x, y] integers)");
            };
            if !(0..world).contains(&x) || !(0..world).contains(&y) {
                return Err("road point is outside the world");
            }
            points.push((x, y));
        }
        // Segments run at ANY angle (#111 — the client renders a smooth
        // spline through these waypoints); length is the Euclidean sum of
        // the chords, which is identical to the old Manhattan sum for the
        // axis-aligned roads that already exist, so nothing reprices.
        let mut length_f = 0.0f64;
        for w in points.windows(2) {
            let (dx, dy) = (w[1].0 - w[0].0, w[1].1 - w[0].1);
            if dx == 0 && dy == 0 {
                return Err("degenerate run (repeated point)");
            }
            length_f += ((dx * dx + dy * dy) as f64).sqrt();
        }
        let length = length_f.round() as i64;
        if length > ROAD_MAX_LENGTH_M {
            return Err("plan exceeds the single-road length cap (lay long routes as multiple plans)");
        }
        // City land only, mirroring `apply_mayor_build_create`: check each
        // run's start, end, and midpoint against every owned plot.
        for w in points.windows(2) {
            let mid = ((w[0].0 + w[1].0) / 2, (w[0].1 + w[1].1) / 2);
            for (px, py) in [w[0], w[1], mid] {
                if self.on_owned_plot(px as i32, py as i32, db).await {
                    return Err("the road would cross privately owned land");
                }
            }
        }
        Ok((points, length))
    }

    /// Apply an editor's `road.plan` (#94): validate a lattice polyline of
    /// free-angle waypoints on the world's native 1m grid and turn it into ONE
    /// ordinary build order (structure_kind `dirt_road`, stone cost scaled by
    /// total length) that players fulfil through the normal `build.contribute`
    /// flow — the contribution IS the labour. Explicit-error posture like the
    /// other editor ops (`road.plan_error {message}`).
    ///
    /// The placement columns carry the FIRST run (so every existing
    /// segment-based proximity/completion consumer keeps working); the full
    /// path rides `build_order.path_json` for the multi-run consumers (#96).
    async fn apply_road_plan(&self, pid: &str, data: Value) {
        let reject = |message: &str| {
            self.push_to_player(pid, json!({"type": "road.plan_error", "message": message}));
        };
        let role = self.clients.lock().unwrap().get(pid).map(|c| c.role.clone()).unwrap_or_default();
        if role != "editor" {
            reject("only an editor may lay road plans");
            return;
        }
        let Some(db) = self.db.clone() else {
            reject("road planning requires persistence (no database)");
            return;
        };
        let (points, length) = match self.parse_road_path(&db, &data).await {
            Ok(v) => v,
            Err(msg) => {
                reject(msg);
                return;
            }
        };
        // District resolved server-side from the path start (the mayor tool
        // sends its district; the editor shouldn't have to know it).
        let Some(district) = self
            .capital
            .district_at(points[0].0 as i32, points[0].1 as i32)
            .map(|d| d.id.to_string())
        else {
            reject("the road must start inside the capital");
            return;
        };
        let stone = (length * ROAD_STONE_PER_M_NUM / ROAD_STONE_PER_M_DEN).max(ROAD_MIN_STONE);
        let required_json = json!({ "stone": stone }).to_string();
        let path_json = match serde_json::to_string(&points) {
            Ok(s) => s,
            Err(_) => {
                reject("failed to encode the path");
                return;
            }
        };
        let kind = format!("road_{}", Uuid::new_v4().simple());
        let placement = Some(mmo::persistence::BuildPlacement {
            structure_kind: "dirt_road".to_string(),
            x: points[0].0,
            y: points[0].1,
            x1: Some(points[1].0),
            y1: Some(points[1].1),
        });
        match db
            .insert_build_order(&district, &kind, &required_json, "open", now_secs(), None, 0, placement, Some(&path_json))
            .await
        {
            Ok(order) => {
                self.push_to_player(pid, json!({"type": "road.planned", "order_id": order.id}));
                self.broadcast_build_list(&district).await;
            }
            Err(e) => {
                eprintln!("[Proxy] road.plan: creating the order failed: {e}");
                reject("failed to create the road order");
            }
        }
    }

    /// Apply an editor's `road.replan` (#104): re-route an OPEN road plan.
    /// Full `road.plan` path validation, stone cost recomputed from the new
    /// length, contributed progress kept — and if the kept progress already
    /// covers the recomputed cost, the order completes on the spot through
    /// the ordinary completion announcements (never a zombie order no
    /// contribution can finish). Built roads deliberately don't move: that's
    /// demolish + re-lay (#106), which is the economy working.
    async fn apply_road_replan(&self, pid: &str, data: Value) {
        let reject = |message: &str| {
            self.push_to_player(pid, json!({"type": "road.plan_error", "message": message}));
        };
        let role = self.clients.lock().unwrap().get(pid).map(|c| c.role.clone()).unwrap_or_default();
        if role != "editor" {
            reject("only an editor may move road plans");
            return;
        }
        let Some(db) = self.db.clone() else {
            reject("road planning requires persistence (no database)");
            return;
        };
        let order_id = data.get("order_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if order_id.is_empty() {
            reject("malformed road.replan (order_id required)");
            return;
        }
        let Ok(Some(order)) = db.build_order_by_id(&order_id).await else {
            reject("no such order");
            return;
        };
        if order.path_json.is_none() {
            reject("that order is not a road plan");
            return;
        }
        if order.state != "open" {
            reject("only open plans can be moved — demolish a built road instead");
            return;
        }
        let (points, length) = match self.parse_road_path(&db, &data).await {
            Ok(v) => v,
            Err(msg) => {
                reject(msg);
                return;
            }
        };
        let Some(district) = self
            .capital
            .district_at(points[0].0 as i32, points[0].1 as i32)
            .map(|d| d.id.to_string())
        else {
            reject("the road must start inside the capital");
            return;
        };
        let stone = (length * ROAD_STONE_PER_M_NUM / ROAD_STONE_PER_M_DEN).max(ROAD_MIN_STONE);
        let required_json = json!({ "stone": stone }).to_string();
        let path_json = match serde_json::to_string(&points) {
            Ok(s) => s,
            Err(_) => {
                reject("failed to encode the path");
                return;
            }
        };
        let placement = mmo::persistence::BuildPlacement {
            structure_kind: "dirt_road".to_string(),
            x: points[0].0,
            y: points[0].1,
            x1: Some(points[1].0),
            y1: Some(points[1].1),
        };
        match db
            .replan_road_order(&order_id, &district, &required_json, &path_json, &placement, now_secs())
            .await
        {
            Ok(outcome) if outcome.applied => {
                self.push_to_player(pid, json!({"type": "road.planned", "order_id": order_id}));
                if outcome.completed {
                    self.announce_order_completion(
                        &db, &order_id, &order.kind, &district,
                        &outcome.contributors, Some(&placement), Some(&path_json),
                    )
                    .await;
                } else {
                    self.broadcast_build_list(&district).await;
                }
                // A replan can carry the plan into a different district's
                // board — the old board must drop it too.
                if district != order.district {
                    self.broadcast_build_list(&order.district).await;
                }
            }
            Ok(_) => reject("the order changed while you were editing — try again"),
            Err(e) => {
                eprintln!("[Proxy] road.replan: updating the order failed: {e}");
                reject("failed to update the road order");
            }
        }
    }

    /// Apply an editor's `road.cancel` (#106): remove a pristine (open,
    /// zero-progress) road plan outright. Anything with contributed stone is
    /// refused toward the demolition route — no silent vaporising of
    /// players' hauling.
    async fn apply_road_cancel(&self, pid: &str, data: Value) {
        let reject = |message: &str| {
            self.push_to_player(pid, json!({"type": "road.plan_error", "message": message}));
        };
        let role = self.clients.lock().unwrap().get(pid).map(|c| c.role.clone()).unwrap_or_default();
        if role != "editor" {
            reject("only an editor may cancel road plans");
            return;
        }
        let Some(db) = self.db.clone() else {
            reject("road planning requires persistence (no database)");
            return;
        };
        let order_id = data.get("order_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if order_id.is_empty() {
            reject("malformed road.cancel (order_id required)");
            return;
        }
        // District for the board refresh, read before the row goes.
        let district = match db.build_order_by_id(&order_id).await {
            Ok(Some(o)) => o.district,
            _ => {
                reject("no such order");
                return;
            }
        };
        match db.cancel_road_order(&order_id).await {
            Ok(true) => {
                self.push_to_player(pid, json!({"type": "road.cancelled", "order_id": order_id}));
                self.broadcast_build_list(&district).await;
            }
            Ok(false) => reject("that plan has contributed stone (or is built) — demolish it instead"),
            Err(e) => {
                eprintln!("[Proxy] road.cancel: {e}");
                reject("failed to cancel the plan");
            }
        }
    }

    /// Apply an editor's `road.demolish` (#106): post a demolition order for
    /// a built road or a part-built plan. The job requires one tool_kit,
    /// contributed on site (the demo order carries the road's path for the
    /// proximity gate); completing it removes the road and refunds its
    /// banked stone — see the demo branch in `apply_build_contribute`.
    async fn apply_road_demolish(&self, pid: &str, data: Value) {
        let reject = |message: &str| {
            self.push_to_player(pid, json!({"type": "road.plan_error", "message": message}));
        };
        let role = self.clients.lock().unwrap().get(pid).map(|c| c.role.clone()).unwrap_or_default();
        if role != "editor" {
            reject("only an editor may post demolitions");
            return;
        }
        let Some(db) = self.db.clone() else {
            reject("road planning requires persistence (no database)");
            return;
        };
        let order_id = data.get("order_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if order_id.is_empty() {
            reject("malformed road.demolish (order_id required)");
            return;
        }
        match db.create_demolition(&order_id, now_secs()).await {
            Ok(Ok(demo)) => {
                self.push_to_player(pid, json!({
                    "type": "road.demolition_planned",
                    "order_id": order_id,
                    "demo_order_id": demo.id,
                }));
                self.broadcast_build_list(&demo.district).await;
            }
            Ok(Err(msg)) => reject(msg),
            Err(e) => {
                eprintln!("[Proxy] road.demolish: {e}");
                reject("failed to post the demolition");
            }
        }
    }

    /// A demolition order completed (#106): tear the road out and pay the
    /// salvage. The refund basis comes from the TARGET (a built road refunds
    /// its full required cost, a part-built plan its contributed progress);
    /// the recipients are the DEMOLITION order's contributors — they did the
    /// salvage work — pro-rata by contributed units, paid into town storage
    /// (the carry cap would strand a big refund). The target order row goes,
    /// the built road's render entity is despawned everywhere, and the
    /// board refresh (from the caller's ordinary completion flow) shows the
    /// road gone.
    async fn finish_demolition(&self, db: &Db, demo_kind: &str, contributors: &[(String, i64)]) {
        let Some(target_id) = demo_kind.strip_prefix("demo_") else { return };
        let Ok(Some((target, refund))) = db.settle_demolition(target_id).await else {
            eprintln!("[Proxy] demolition {demo_kind}: target already gone");
            return;
        };
        // Pay the salvage pro-rata by demo-order units (integer split,
        // remainder to the largest contributor first by ordering).
        let total_units: i64 = contributors.iter().map(|(_, u)| u).sum();
        if total_units > 0 {
            for (item, qty) in &refund {
                let mut remaining = *qty;
                for (i, (cid, units)) in contributors.iter().enumerate() {
                    let share = if i + 1 == contributors.len() {
                        remaining // last takes the remainder — nothing lost
                    } else {
                        qty * units / total_units
                    };
                    if share > 0 {
                        if let Err(e) = db.grant_storage(cid, item, share).await {
                            eprintln!("[Proxy] demolition refund to {cid} failed: {e}");
                        } else {
                            self.send_storage(cid).await; // online recipients see it land
                        }
                        remaining -= share;
                    }
                }
            }
        }
        // The built road's render entity disappears for everyone. (A
        // part-built plan had no structure; the despawn is a no-op there.)
        let entity_id = format!("structure_{}", target.kind);
        let msg = json!({"type": "despawn", "player_id": entity_id}).to_string();
        let clients = self.clients.lock().unwrap();
        for info in clients.values() {
            self.push_to_client(info, Message::Text(msg.clone()));
        }
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
    /// Push every plot in `district_id` (owned or not, with owner names
    /// resolved) to `pid` as `plot.district`. Takes the district directly
    /// rather than deriving it, so callers that already know it (`send_plot_roster`
    /// below; `district.enter`'s handler, which trusts the client's own
    /// self-reported crossing) can't race a lagging position cache (#48).
    async fn send_plot_roster_for(&self, pid: &str, district_id: &str) {
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
            self.push_to_player(pid, json!({"type": "plot.district", "plots": arr}));
        }
    }

    /// `send_plot_roster_for`, with the district resolved from the player's
    /// own cached position — for callers with no better context (an explicit
    /// `plot.district` request). Resolved by position, not the zone's region
    /// *centre* (`district_for_zone`) — the latter only tells apart districts
    /// when each is backed by its own zone shard (the real auto-scaled
    /// deployment model). A single zone spanning every district (the common
    /// small/dev deployment) has one fixed centre, so `district_for_zone`
    /// would report the same district regardless of where the player actually
    /// walks — invisible for `build.list` (every Phase 1 build order is in
    /// Civic anyway) but very visible here, since plots exist only in the
    /// Suburbs.
    ///
    /// **Not** used by `district.enter` (see its handler): the player's
    /// position cache (`entity_state`) is updated asynchronously from the
    /// zone's own status broadcasts, and can still read the *previous*
    /// district for a moment right when the client's own (instant,
    /// self-detected) crossing message arrives — #48, reproduced by sending
    /// `district.enter` immediately after movement with no settling delay.
    async fn send_plot_roster(&self, pid: &str) {
        let Some((x, y)) = self.entity_state.lock().unwrap().get(pid).map(|c| (c.x, c.y)) else { return };
        let Some(district_id) = self.capital.district_at(x, y).map(|d| d.id) else { return };
        self.send_plot_roster_for(pid, district_id).await;
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

    /// Answer `terrain.list` with the heightmap grid (#54), now sampled from
    /// the baked terrain artifact (#63) rather than an in-process generator.
    /// Stateless and static (same every boot) — sent once per session rather
    /// than folded into `partition` (which gets rebroadcast on every zone
    /// split/merge/capture; the terrain payload is too large to wastefully
    /// resend on every one of those).
    ///
    /// The coarse-grid wire shape is unchanged from the old synthetic
    /// generator — a flat `(TERRAIN_RESOLUTION+1)^2` grid — deliberately
    /// decoupled from the artifact's own internal tile/cell resolution (see
    /// `mmo::world::loaded_terrain`'s doc comment for why): it's the
    /// permanent, always-present backdrop. Also includes the artifact's own
    /// manifest shape (tile_size/tiles/cell_size_m/height_min_m/
    /// height_max_m) so the client knows what it can additionally stream in
    /// at native resolution via `terrain.tile_request`/`send_terrain_tile`.
    fn send_terrain(&self, pid: &str) {
        let terrain = &self.capital.terrain;
        let resolution = mmo::world::TERRAIN_RESOLUTION;
        let fine_n = (resolution + 1) as usize;
        let step = WORLD_SIZE as f32 / resolution as f32;
        let mut heights = Vec::with_capacity(fine_n * fine_n);
        for gy in 0..fine_n {
            for gx in 0..fine_n {
                heights.push(terrain.sample_height(gx as f32 * step, gy as f32 * step));
            }
        }
        let manifest = terrain.manifest();
        self.push_to_player(pid, json!({
            "type": "terrain.data",
            "resolution": resolution,
            "world_size": WORLD_SIZE,
            "heights": heights,
            "tile_size": manifest.tile_size,
            "tiles": [manifest.tiles.0, manifest.tiles.1],
            "cell_size_m": manifest.cell_size_m,
            "height_min_m": manifest.height_min_m,
            "height_max_m": manifest.height_max_m,
        }));
    }

    /// Answer `terrain.tile_request` with the requested tile's raw bytes —
    /// terrain streaming's on-demand native-resolution path. Reuses
    /// `terrain_common::HeightTile::encode`'s exact on-disk wire format,
    /// base64-wrapped so it still rides the existing all-JSON/text-frame
    /// transport (see `docs/protocol.md`'s `terrain.*` section). Stateless
    /// and silent on a miss: an out-of-range or not-yet-loaded `(tx, ty)`
    /// just gets nothing back, the same posture as every other
    /// directly-answered message in this dispatch loop when asked for
    /// something that doesn't exist.
    fn send_terrain_tile(&self, pid: &str, tx: i32, ty: i32) {
        let Some(tile) = self.capital.terrain.height_tile(tx, ty) else { return };
        let bytes = tile.encode(1);
        self.push_to_player(pid, json!({
            "type": "terrain.tile_data",
            "tx": tx,
            "ty": ty,
            "side": tile.side,
            "encoding": "tile_v1",
            "data_b64": base64::engine::general_purpose::STANDARD.encode(&bytes),
        }));
    }

    /// Answer `terrain.delta_request` with the chunk's hand-authored edit
    /// layer (terrain-editing epic #72). Same client-pull, stateless posture
    /// as `send_terrain_tile`, with one deliberate difference: an in-range
    /// chunk **always** answers — `has_delta: false` when unedited — so the
    /// client never has to distinguish "not answered yet" from "answered,
    /// nothing here". Out-of-range requests stay silently ignored, exactly
    /// like the tile path. A DB read failure (or db-less mode) answers
    /// `has_delta: false` too: the client renders base terrain, which is
    /// also what a corrupt-row chunk should degrade to.
    async fn send_terrain_delta(&self, pid: &str, tx: i32, ty: i32) {
        let manifest = self.capital.terrain.manifest();
        if tx < 0 || ty < 0 || tx >= manifest.tiles.0 as i32 || ty >= manifest.tiles.1 as i32 {
            return;
        }
        let side = manifest.tile_size as usize + 1;
        let delta = match &self.db {
            Some(db) => db.load_terrain_delta(tx, ty, side).await.ok().flatten(),
            None => None,
        };
        match delta.and_then(|d| d.height_delta.map(|hd| (d.revision, hd))) {
            Some((revision, height_delta)) => {
                let bytes = height_delta.encode(1);
                self.push_to_player(pid, json!({
                    "type": "terrain.delta_data",
                    "tx": tx,
                    "ty": ty,
                    "has_delta": true,
                    "revision": revision,
                    "encoding": "delta_v1",
                    "data_b64": base64::engine::general_purpose::STANDARD.encode(&bytes),
                }));
            }
            None => {
                self.push_to_player(pid, json!({
                    "type": "terrain.delta_data",
                    "tx": tx,
                    "ty": ty,
                    "has_delta": false,
                }));
            }
        }
    }

    /// The authoritative *effective* ground height at `(x, y)`: baked base
    /// plus any hand-authored height delta (terrain editing #72/#80) — the
    /// one blessed door for any future server-side gameplay consumer of
    /// elevation (fall damage, water simulation, 3D-aware movement
    /// validation, Phase 2 terraforming rules...).
    ///
    /// The #80 audit found no such consumer at the time; the **first real
    /// one is `env_tick_once` (#87)**, which reads it ~once per player per
    /// second to decide "submerged". Otherwise the audit's findings stand:
    /// movement validation is pure 2D clamping (`zone_server::clamp_world`/
    /// `clamp_region`), there is no server-side ground-snap (the client
    /// snaps visually via `Protocol.w2v`), and `is_walkable`/`nav_flags`
    /// have no production call sites. The only production `sample_height`
    /// caller is `send_terrain`'s coarse backdrop, which deliberately stays
    /// **base**: it's sent once per session as a static payload, so baking
    /// deltas in would leave it stale after the first live edit — and the
    /// client only renders deltas on streamed chunks anyway (the backdrop
    /// is only visible outside the streamed ring, where an edit is beneath
    /// LOD relevance).
    ///
    /// Composition happens live (a per-call delta load), so there is no
    /// cache and therefore nothing to invalidate or debounce — the question
    /// #80 told us to check before building machinery. If a per-tick
    /// consumer ever appears, add an in-memory delta cache maintained by
    /// `apply_terrain_edit_op`/`apply_terrain_revert_op` (both already
    /// serialize under `terrain_edit_lock`) and invalidate there.
    async fn composited_ground_height(&self, x: f32, y: f32) -> f32 {
        let terrain = &self.capital.terrain;
        let Some(db) = &self.db else {
            return terrain.sample_height(x, y); // db-less mode: base only
        };
        let (tx, ty) = terrain.tile_at(x, y);
        let side = terrain.manifest().tile_size as usize + 1;
        let delta = db
            .load_terrain_delta(tx, ty, side)
            .await
            .ok()
            .flatten()
            .and_then(|d| d.height_delta);
        terrain.sample_height_with_delta(x, y, delta.as_ref())
    }

    /// One environmental pass (#87): compute every connected player's
    /// environment flags from the gateway's live position cache and push them
    /// to the player's owning zone as an `env_state` command (the same channel
    /// `spawn_entity` uses). The zone applies drain/damage authoritatively in
    /// its own tick — the split brain stays split: the gateway knows terrain
    /// and object positions but doesn't own hp; the zone owns hp but knows no
    /// terrain.
    ///
    /// Submerged = **in the baked water mask** (the bake's own per-cell
    /// verdict of where the river/bay is — matching the design's "goes in
    /// water → hold breath"; a mask cell's bed is usually the flat 0m NoData
    /// fill, so depth carries no signal there) **or** composited ground (the
    /// #80 door, making this that audit's first real gameplay consumer) more
    /// than `SUBMERGED_DEPTH_M` below sea level — the clause that makes an
    /// editor-dug pond drown. The bank fringe the client's water plane
    /// visually floods stays land in the mask (the bake clamps land at/below
    /// sea UP to +0.2m), so the wet-looking shoreline rim is harmless.
    /// Known limit: the mask is base-static — an editor *raising* a mask
    /// cell above the waterline still reads as water until a rebake.
    ///
    /// The per-call delta load composited_ground_height documents is ~one DB
    /// point-read per player per second here — nowhere near needing the
    /// cache that doc reserves as the escalation path.
    ///
    /// Factored out of `env_monitor`'s loop so tests can drive single passes.
    async fn env_tick_once(&self) {
        let sea_level = self.capital.terrain.manifest().sea_level_m;
        // Connected players joined with their cached positions — entity_state
        // follows the zones' status_updates, the same view every other
        // gateway consumer trusts. Snapshot under the locks, compute after.
        let players: Vec<(String, i32, i32, String)> = {
            let clients = self.clients.lock().unwrap();
            let cache = self.entity_state.lock().unwrap();
            clients
                .values()
                .filter_map(|c| {
                    cache
                        .get(&c.player_id)
                        .map(|e| (c.player_id.clone(), e.x, e.y, c.current_zone.clone()))
                })
                .collect()
        };
        // Poison-tree positions, snapshotted once per pass from the object
        // cache (#85 — never the DB). A linear scan per player is fine at
        // authored-forest counts; bucket spatially only if profiling ever
        // says otherwise.
        let trees: Vec<(i64, i64)> = self
            .world_object_cache()
            .await
            .lock()
            .unwrap()
            .values()
            .filter(|o| o.kind == "poison_tree")
            .map(|o| (o.x as i64, o.y as i64))
            .collect();
        let radius2 = POISON_RADIUS_M * POISON_RADIUS_M;
        for (pid, x, y, zone_id) in players {
            let in_mask = self.capital.terrain.is_water(x as f32, y as f32);
            let submerged = in_mask || {
                let ground = self.composited_ground_height(x as f32, y as f32).await;
                sea_level - ground > SUBMERGED_DEPTH_M
            };
            let poison_sources = trees
                .iter()
                .filter(|(tx_, ty_)| {
                    let (dx, dy) = (tx_ - x as i64, ty_ - y as i64);
                    dx * dx + dy * dy <= radius2
                })
                .count() as i64;
            let tx = self.zones.lock().unwrap().get(&zone_id).map(|z| z.tx.clone());
            if let Some(tx) = tx {
                let _ = tx.send(Message::Text(
                    json!({
                        "type": "env_state",
                        "player_id": pid,
                        "submerged": submerged,
                        "poison_sources": poison_sources,
                    })
                    .to_string(),
                ));
            }
        }
    }

    /// The environment ticker (#87): `env_tick_once` every ENV_TICK_INTERVAL.
    async fn env_monitor(self: Arc<Self>) {
        let mut interval = tokio::time::interval(ENV_TICK_INTERVAL);
        loop {
            interval.tick().await;
            self.env_tick_once().await;
        }
    }

    /// Push one message to every connected client — the fanout for
    /// `terrain.delta_patch`, which any client with the chunk streamed in
    /// cares about regardless of zone/district (terrain is world-scoped;
    /// clients that don't hold the chunk ignore the patch).
    fn broadcast_to_all(&self, msg: Value) {
        let text = msg.to_string();
        let clients = self.clients.lock().unwrap();
        for info in clients.values() {
            self.push_to_client(info, Message::Text(text.clone()));
        }
    }

    /// Apply `terrain.edit_op` (terrain editing #72): one editor brush
    /// stroke's worth of corner-height increments, validated and written to
    /// the authoritative delta store, then patched out to every client.
    ///
    /// Cells arrive in **world corner coordinates** (`[[cx, cy, d_cm], ..]`)
    /// rather than per-chunk ones: a chunk's last corner row/column is the
    /// same world data as its neighbor's first (the tile edge-duplication
    /// convention), and making the *server* own that fanout means a stroke
    /// across a seam can never leave the two chunks disagreeing — the exact
    /// hazard `terrain-common`'s module doc flags for the write path. One op
    /// therefore touches 1–4 chunks (4 only at a chunk corner).
    ///
    /// Validation is all-or-nothing: any out-of-bounds corner, over-cap
    /// increment, or over-cap accumulated offset rejects the whole op with
    /// `terrain.edit_error` before anything is saved (mirrors
    /// `apply_mayor_build_create`'s explicit-error posture — an editor needs
    /// to see *why*, unlike the silent gameplay no-ops).
    async fn apply_terrain_edit_op(&self, pid: &str, data: Value) {
        let reject = |message: &str| {
            self.push_to_player(pid, json!({"type": "terrain.edit_error", "message": message}));
        };
        let role = self.clients.lock().unwrap().get(pid).map(|c| c.role.clone()).unwrap_or_default();
        if role != "editor" {
            reject("only an editor may edit terrain");
            return;
        }
        let Some(db) = self.db.clone() else {
            reject("terrain editing requires persistence (no database)");
            return;
        };
        let Some(cells_json) = data.get("cells").and_then(|v| v.as_array()) else {
            reject("malformed op: missing cells");
            return;
        };
        if cells_json.is_empty() || cells_json.len() > EDIT_MAX_CELLS_PER_OP {
            reject("malformed op: empty or oversized cells array");
            return;
        }
        let manifest = self.capital.terrain.manifest();
        let ts = manifest.tile_size as i64;
        let (max_cx, max_cy) = (ts * manifest.tiles.0 as i64, ts * manifest.tiles.1 as i64);
        let mut cells: Vec<(i64, i64, i32)> = Vec::with_capacity(cells_json.len());
        for c in cells_json {
            let (Some(cx), Some(cy), Some(d_cm)) = (
                c.get(0).and_then(|v| v.as_i64()),
                c.get(1).and_then(|v| v.as_i64()),
                c.get(2).and_then(|v| v.as_i64()),
            ) else {
                reject("malformed op: each cell must be [cx, cy, d_cm]");
                return;
            };
            if cx < 0 || cx > max_cx || cy < 0 || cy > max_cy {
                reject("corner out of world bounds");
                return;
            }
            if d_cm.abs() > EDIT_MAX_OFFSET_CM as i64 {
                reject("increment exceeds the per-corner offset cap");
                return;
            }
            cells.push((cx, cy, d_cm as i32));
        }

        // Group per chunk, duplicating shared-edge corners into every chunk
        // that stores them: `cx/ts` owns the corner, and a corner exactly on
        // an interior seam (`cx % ts == 0`) is also its left/top neighbor's
        // last column/row.
        let mut per_chunk: BTreeMap<(i32, i32), Vec<(usize, usize, i32)>> = BTreeMap::new();
        for &(cx, cy, d) in &cells {
            let mut txs = vec![(cx / ts).min(manifest.tiles.0 as i64 - 1)];
            if cx % ts == 0 && cx > 0 && cx / ts <= manifest.tiles.0 as i64 - 1 {
                txs.push(cx / ts - 1);
            }
            let mut tys = vec![(cy / ts).min(manifest.tiles.1 as i64 - 1)];
            if cy % ts == 0 && cy > 0 && cy / ts <= manifest.tiles.1 as i64 - 1 {
                tys.push(cy / ts - 1);
            }
            for &tx in &txs {
                for &ty in &tys {
                    let (gx, gy) = ((cx - tx * ts) as usize, (cy - ty * ts) as usize);
                    per_chunk.entry((tx as i32, ty as i32)).or_default().push((gx, gy, d));
                }
            }
        }

        // Read-modify-write under the edit lock: build every chunk's updated
        // delta in memory first (so a cap violation rejects the whole op with
        // nothing saved), then persist and broadcast. Along the way, capture
        // each touched block's PRE-edit content for the undo log (whole
        // 512-byte blocks; `None` = the block didn't exist, revert deletes
        // it — the design doc's inverse-blocks tradeoff).
        let _guard = self.terrain_edit_lock.lock().await;
        let side = manifest.tile_size as usize + 1;
        let mut updated: Vec<((i32, i32), terrain_common::SparseHeightDelta)> = Vec::new();
        let mut prev_blocks: Vec<(i32, i32, i64, Option<Vec<u8>>)> = Vec::new();
        for (&(tx, ty), chunk_cells) in &per_chunk {
            let mut hd = match db.load_terrain_delta(tx, ty, side).await {
                Ok(existing) => existing
                    .and_then(|d| d.height_delta)
                    .unwrap_or_else(|| terrain_common::SparseHeightDelta::new(side)),
                Err(e) => {
                    eprintln!("[Proxy] terrain.edit_op: loading delta ({tx},{ty}) failed: {e}");
                    reject("storage error loading the chunk's delta");
                    return;
                }
            };
            let mut captured: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
            for &(gx, gy, _) in chunk_cells {
                let idx = hd.block_index_for(gx, gy);
                if captured.insert(idx) {
                    prev_blocks.push((tx, ty, idx as i64, hd.block_bytes(idx)));
                }
            }
            for &(gx, gy, d) in chunk_cells {
                let total = hd.offset_cm(gx, gy) as i32 + d;
                if total.abs() > EDIT_MAX_OFFSET_CM {
                    reject("accumulated offset would exceed the per-corner cap");
                    return;
                }
                hd.set_offset_cm(gx, gy, total as i16);
            }
            hd.prune_zero_blocks();
            updated.push(((tx, ty), hd));
        }
        let edited_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // Log the op + ack its id to the author BEFORE the patches, so the
        // client's history UI knows the stroke's id when they arrive.
        let op_id = Uuid::new_v4().to_string();
        let brush = data.get("brush").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
        let author = terrain_common::AuthorId::Editor(pid.to_string());
        if let Err(e) = db.log_terrain_edit_op(&op_id, &author.to_string(), &brush, edited_at, &prev_blocks).await {
            eprintln!("[Proxy] terrain.edit_op: logging op failed: {e}");
            reject("storage error logging the op");
            return;
        }
        self.push_to_player(pid, json!({"type": "terrain.edit_ack", "op_id": op_id, "brush": brush}));
        for ((tx, ty), hd) in updated {
            let blob = hd.encode(1);
            let delta = terrain_common::TerrainDelta {
                chunk_tx: tx,
                chunk_ty: ty,
                bake_hash: manifest.bake_hash.clone(),
                revision: 0, // assigned by the DB on save
                // A pruned-to-empty delta (an op that nets to zero) persists
                // as "no height layer" (NULL blob), per SparseHeightDelta::
                // is_empty's contract — otherwise the chunk answers
                // `has_delta: true` forever with all-zero offsets.
                height_delta: if hd.is_empty() { None } else { Some(hd) },
                provenance: terrain_common::Provenance {
                    // The durable character id — the identity the rest of the
                    // codebase uses for "who did this".
                    author: terrain_common::AuthorId::Editor(pid.to_string()),
                    edited_at,
                },
            };
            match db.save_terrain_delta(&delta).await {
                Ok(revision) => {
                    self.broadcast_to_all(json!({
                        "type": "terrain.delta_patch",
                        "tx": tx,
                        "ty": ty,
                        "revision": revision,
                        "encoding": "delta_v1",
                        "data_b64": base64::engine::general_purpose::STANDARD.encode(&blob),
                    }));
                }
                Err(e) => {
                    eprintln!("[Proxy] terrain.edit_op: saving delta ({tx},{ty}) failed: {e}");
                    reject("storage error saving the chunk's delta");
                    return;
                }
            }
        }
    }

    /// Apply `terrain.revert_op` (terrain-editing undo): restore every block
    /// the op touched to its logged pre-op content, wholesale. Editor-gated
    /// like `terrain.edit_op`; an unknown or already-reverted op id rejects
    /// with `terrain.edit_error` (the DB claim in `take_revertable_edit_op`
    /// is the double-revert guard, atomic even across racing reverts).
    /// Whole-block restore means an out-of-order revert can clobber a later
    /// overlapping op — the documented tradeoff; clients offer undo-last.
    async fn apply_terrain_revert_op(&self, pid: &str, data: Value) {
        let reject = |message: &str| {
            self.push_to_player(pid, json!({"type": "terrain.edit_error", "message": message}));
        };
        let role = self.clients.lock().unwrap().get(pid).map(|c| c.role.clone()).unwrap_or_default();
        if role != "editor" {
            reject("only an editor may edit terrain");
            return;
        }
        let Some(db) = self.db.clone() else {
            reject("terrain editing requires persistence (no database)");
            return;
        };
        let Some(op_id) = data.get("op_id").and_then(|v| v.as_str()).map(str::to_string) else {
            reject("malformed revert: missing op_id");
            return;
        };
        let _guard = self.terrain_edit_lock.lock().await;
        let rows = match db.take_revertable_edit_op(&op_id).await {
            Ok(Some(rows)) => rows,
            Ok(None) => {
                reject("unknown or already-reverted op");
                return;
            }
            Err(e) => {
                eprintln!("[Proxy] terrain.revert_op: claiming {op_id} failed: {e}");
                reject("storage error claiming the op");
                return;
            }
        };
        // Group the snapshots per chunk and write each chunk's blocks back.
        let mut per_chunk: BTreeMap<(i32, i32), Vec<(i64, Option<Vec<u8>>)>> = BTreeMap::new();
        for (tx, ty, idx, prev) in rows {
            per_chunk.entry((tx, ty)).or_default().push((idx, prev));
        }
        let manifest = self.capital.terrain.manifest();
        let side = manifest.tile_size as usize + 1;
        let edited_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        for ((tx, ty), blocks) in per_chunk {
            let mut hd = match db.load_terrain_delta(tx, ty, side).await {
                Ok(existing) => existing
                    .and_then(|d| d.height_delta)
                    .unwrap_or_else(|| terrain_common::SparseHeightDelta::new(side)),
                Err(e) => {
                    eprintln!("[Proxy] terrain.revert_op: loading delta ({tx},{ty}) failed: {e}");
                    reject("storage error loading the chunk's delta");
                    return;
                }
            };
            for (idx, prev) in blocks {
                match prev {
                    Some(bytes) => {
                        if hd.set_block_bytes(idx as usize, &bytes).is_err() {
                            eprintln!("[Proxy] terrain.revert_op: corrupt snapshot for op {op_id} block {idx}");
                            reject("corrupt pre-edit snapshot for this op");
                            return;
                        }
                    }
                    None => hd.remove_block(idx as usize),
                }
            }
            hd.prune_zero_blocks();
            let blob = hd.encode(1);
            let delta = terrain_common::TerrainDelta {
                chunk_tx: tx,
                chunk_ty: ty,
                bake_hash: manifest.bake_hash.clone(),
                revision: 0, // assigned by the DB on save
                // A fully-reverted chunk persists as "no height layer" (NULL
                // blob) so it round-trips as unedited (`has_delta: false`) —
                // same rule as the edit path above.
                height_delta: if hd.is_empty() { None } else { Some(hd) },
                provenance: terrain_common::Provenance {
                    author: terrain_common::AuthorId::Editor(pid.to_string()),
                    edited_at,
                },
            };
            match db.save_terrain_delta(&delta).await {
                Ok(revision) => {
                    self.broadcast_to_all(json!({
                        "type": "terrain.delta_patch",
                        "tx": tx,
                        "ty": ty,
                        "revision": revision,
                        "encoding": "delta_v1",
                        "data_b64": base64::engine::general_purpose::STANDARD.encode(&blob),
                    }));
                }
                Err(e) => {
                    eprintln!("[Proxy] terrain.revert_op: saving delta ({tx},{ty}) failed: {e}");
                    reject("storage error saving the chunk's delta");
                    return;
                }
            }
        }
        self.push_to_player(pid, json!({"type": "terrain.revert_ack", "op_id": op_id}));
    }

    // --- Placed world props (player-attributes epic #83, issue #85) ----------

    /// The live world-object cache (see the field doc): lazily hydrated from
    /// the `world_object` table on first touch, then kept write-through by
    /// `apply_object_place`/`apply_object_delete`. With no DB it stays an
    /// empty map — `object.list` still answers (an empty roster), only the
    /// write path needs persistence.
    async fn world_object_cache(&self) -> &Mutex<HashMap<String, persistence::WorldObject>> {
        self.world_objects
            .get_or_init(|| async {
                let mut map = HashMap::new();
                if let Some(db) = &self.db {
                    match db.list_world_objects().await {
                        Ok(objects) => {
                            for o in objects {
                                map.insert(o.id.clone(), o);
                            }
                        }
                        Err(e) => println!("[Proxy] WARNING: world_object cache load failed: {e}"),
                    }
                }
                Mutex::new(map)
            })
            .await
    }

    /// Answer `object.list`: the full current object roster from the cache.
    /// Explicit even when empty — the client must not have to distinguish
    /// "not answered yet" from "answered, nothing placed" (the
    /// `terrain.delta_data` lesson).
    async fn send_object_list(&self, pid: &str) {
        let objects: Vec<Value> = {
            let cache = self.world_object_cache().await.lock().unwrap();
            cache
                .values()
                .map(|o| json!({"id": o.id, "kind": o.kind, "x": o.x, "y": o.y}))
                .collect()
        };
        self.push_to_player(pid, json!({"type": "object.list", "objects": objects}));
    }

    /// Apply an editor's `object.place`. Validation is explicit-error
    /// (`object.edit_error`), mirroring `apply_terrain_edit_op`'s posture — an
    /// editor needs to see *why*, unlike the silent gameplay no-ops. On
    /// success the stored object is broadcast to every client as
    /// `object.placed` (the author included — clients render acks).
    async fn apply_object_place(&self, pid: &str, data: Value) {
        let reject = |message: &str| {
            self.push_to_player(pid, json!({"type": "object.edit_error", "message": message}));
        };
        let role = self.clients.lock().unwrap().get(pid).map(|c| c.role.clone()).unwrap_or_default();
        if role != "editor" {
            reject("only an editor may place objects");
            return;
        }
        let Some(db) = self.db.clone() else {
            reject("placing objects requires persistence (no database)");
            return;
        };
        let kind = data.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        if !OBJECT_KINDS.contains(&kind) {
            reject("unknown object kind");
            return;
        }
        let (Some(x), Some(y)) = (
            data.get("x").and_then(|v| v.as_i64()),
            data.get("y").and_then(|v| v.as_i64()),
        ) else {
            reject("malformed object.place (x/y required)");
            return;
        };
        let world = mmo::world::WORLD_SIZE as i64;
        if !(0..world).contains(&x) || !(0..world).contains(&y) {
            reject("object position is outside the world");
            return;
        }
        let author = terrain_common::AuthorId::Editor(pid.to_string()).to_string();
        match db.insert_world_object(kind, x as i32, y as i32, &author, now_secs()).await {
            Ok(obj) => {
                let placed = json!({"type": "object.placed", "id": obj.id, "kind": obj.kind, "x": obj.x, "y": obj.y});
                self.world_object_cache().await.lock().unwrap().insert(obj.id.clone(), obj);
                self.broadcast_to_all(placed);
            }
            Err(e) => {
                println!("[Proxy] object.place persist failed: {e}");
                reject("storage error saving the object");
            }
        }
    }

    /// Apply an editor's `object.delete`. The DB row is the claim (a losing
    /// racer's delete affects zero rows and errors instead of broadcasting a
    /// second removal); the cache entry follows the row.
    async fn apply_object_delete(&self, pid: &str, data: Value) {
        let reject = |message: &str| {
            self.push_to_player(pid, json!({"type": "object.edit_error", "message": message}));
        };
        let role = self.clients.lock().unwrap().get(pid).map(|c| c.role.clone()).unwrap_or_default();
        if role != "editor" {
            reject("only an editor may delete objects");
            return;
        }
        let Some(db) = self.db.clone() else {
            reject("deleting objects requires persistence (no database)");
            return;
        };
        let object_id = data.get("object_id").and_then(|v| v.as_str()).unwrap_or("");
        if object_id.is_empty() {
            reject("malformed object.delete (object_id required)");
            return;
        }
        match db.delete_world_object(object_id).await {
            Ok(true) => {
                self.world_object_cache().await.lock().unwrap().remove(object_id);
                self.broadcast_to_all(json!({"type": "object.removed", "id": object_id}));
            }
            Ok(false) => reject("no such object"),
            Err(e) => {
                println!("[Proxy] object.delete persist failed: {e}");
                reject("storage error deleting the object");
            }
        }
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
                role: identity.role.clone(),
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
                "role": identity.role.clone(),
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
                    // `terrain.list` is a stateless read of the static heightmap
                    // grid (#54) — same reasoning as `craft.list`.
                    if data.get("type").and_then(|v| v.as_str()) == Some("terrain.list") {
                        self.send_terrain(&player_id);
                        continue;
                    }
                    // `terrain.tile_request` (terrain streaming): a client-pull
                    // request for one native-resolution tile, keyed only on the
                    // requested (tx, ty) — stateless/idempotent, same reasoning
                    // as `terrain.list`.
                    if data.get("type").and_then(|v| v.as_str()) == Some("terrain.tile_request") {
                        let tx = data.get("tx").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                        let ty = data.get("ty").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                        self.send_terrain_tile(&player_id, tx, ty);
                        continue;
                    }
                    // `terrain.delta_request` (terrain editing #72): the chunk's
                    // hand-authored edit layer — client-pull and stateless like
                    // `terrain.tile_request`, but an in-range chunk always
                    // answers (`has_delta: false` when unedited).
                    if data.get("type").and_then(|v| v.as_str()) == Some("terrain.delta_request") {
                        let tx = data.get("tx").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                        let ty = data.get("ty").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                        self.send_terrain_delta(&player_id, tx, ty).await;
                        continue;
                    }
                    // `terrain.edit_op` (terrain editing #72) is role- and
                    // bounds-checked with no live-position dependency — same
                    // direct-answer reasoning as `mayor.build_create`.
                    if data.get("type").and_then(|v| v.as_str()) == Some("terrain.edit_op") {
                        self.apply_terrain_edit_op(&player_id, data).await;
                        continue;
                    }
                    // `terrain.revert_op` (terrain-editing undo): same
                    // role-gated, no-live-position reasoning as edit_op.
                    if data.get("type").and_then(|v| v.as_str()) == Some("terrain.revert_op") {
                        self.apply_terrain_revert_op(&player_id, data).await;
                        continue;
                    }
                    // `object.list` (world props #85) is a stateless read of the
                    // gateway's object cache — same reasoning as `terrain.list`.
                    if data.get("type").and_then(|v| v.as_str()) == Some("object.list") {
                        self.send_object_list(&player_id).await;
                        continue;
                    }
                    // `object.place`/`object.delete` (#85) are role- and
                    // bounds-checked with no live-position dependency — same
                    // direct-answer reasoning as `terrain.edit_op`.
                    if data.get("type").and_then(|v| v.as_str()) == Some("object.place") {
                        self.apply_object_place(&player_id, data).await;
                        continue;
                    }
                    if data.get("type").and_then(|v| v.as_str()) == Some("object.delete") {
                        self.apply_object_delete(&player_id, data).await;
                        continue;
                    }
                    // `road.replan` (#104): role/DB-checked like road.plan.
                    if data.get("type").and_then(|v| v.as_str()) == Some("road.replan") {
                        self.apply_road_replan(&player_id, data).await;
                        continue;
                    }
                    // `road.cancel` / `road.demolish` (#106): same reasoning.
                    if data.get("type").and_then(|v| v.as_str()) == Some("road.cancel") {
                        self.apply_road_cancel(&player_id, data).await;
                        continue;
                    }
                    if data.get("type").and_then(|v| v.as_str()) == Some("road.demolish") {
                        self.apply_road_demolish(&player_id, data).await;
                        continue;
                    }
                    // `road.plan` (#94) is role/geometry/db-checked with no
                    // live-position dependency — same direct-answer reasoning
                    // as `terrain.edit_op` and `mayor.build_create`.
                    if data.get("type").and_then(|v| v.as_str()) == Some("road.plan") {
                        self.apply_road_plan(&player_id, data).await;
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
                    // `mayor.build_create` is role- and DB-ownership-checked (is this
                    // caller the mayor? is the target city land?), no live-position
                    // dependency, so it's answered directly too.
                    if data.get("type").and_then(|v| v.as_str()) == Some("mayor.build_create") {
                        self.apply_mayor_build_create(&player_id, data).await;
                        continue;
                    }
                    // `district.enter` is the client announcing (self-detected from the
                    // `partition` it already has) that it crossed a district gate and is
                    // showing a transition curtain. The actual position/zone handoff
                    // already happened via the ordinary migrate-request path — this is
                    // purely the client-facing load/ready handshake (#15): refresh the
                    // district-scoped content (the build board, the plot roster) for
                    // wherever the player actually now is, then ack so the client can drop
                    // the curtain. The plot roster trusts the client's self-reported `to`
                    // directly (#48) rather than re-deriving it from the position cache,
                    // which updates asynchronously and can still read the *previous*
                    // district for a moment right as this message arrives — a read-only,
                    // non-authoritative query, so there's nothing to gain from re-deriving
                    // it server-side, only a race to lose.
                    if data.get("type").and_then(|v| v.as_str()) == Some("district.enter") {
                        self.send_build_orders(&player_id).await;
                        match data.get("to").and_then(|v| v.as_str()) {
                            Some(to) => self.send_plot_roster_for(&player_id, to).await,
                            None => self.send_plot_roster(&player_id).await,
                        }
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

        // Environmental tick (#87): per-player submerged/poison flags pushed
        // to owning zones every second.
        let me = self.clone();
        tokio::spawn(async move { me.env_monitor().await });

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
        role: "player".to_string(),
        pending,
    }
}

/// Build a durable identity from a loaded/created character row and its account's role.
fn persistent_identity(ch: mmo::persistence::Character, role: String) -> Identity {
    Identity {
        character_id: ch.id,
        name: ch.name,
        x: ch.x as i32,
        y: ch.y as i32,
        hp: ch.hp as i32,
        persistent: true,
        role,
        pending: None,
    }
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
            // Seed the one mayor login (idempotent — a no-op once the account exists).
            let mayor_hash = auth::hash_password(MAYOR_PASSWORD).unwrap_or_default();
            let (tcx, tcy) = mmo::world::capital().town_centre;
            match db.seed_mayor_account(MAYOR_EMAIL, &mayor_hash, "The Mayor", tcx as i64, tcy as i64, SPAWN_HP as i64, now).await {
                Ok(()) => println!("[Proxy] Mayor account ready ({MAYOR_EMAIL})"),
                Err(e) => println!("[Proxy] WARNING: mayor seeding failed: {e}"),
            }
            // Seed the one editor login (terrain editing #72) the same way.
            let editor_hash = auth::hash_password(EDITOR_PASSWORD).unwrap_or_default();
            match db.seed_account_with_role(EDITOR_EMAIL, &editor_hash, "The Editor", tcx as i64, tcy as i64, SPAWN_HP as i64, now, "editor").await {
                Ok(()) => println!("[Proxy] Editor account ready ({EDITOR_EMAIL})"),
                Err(e) => println!("[Proxy] WARNING: editor seeding failed: {e}"),
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
            terrain_edit_lock: tokio::sync::Mutex::new(()),
            world_objects: tokio::sync::OnceCell::new(),
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
                role: "player".to_string(),
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
                role: "player".to_string(),
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
                role: "player".to_string(),
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
                role: "player".to_string(),
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
        add_zone_region(&proxy, "z_suburbs", Region { x0: 0, y0: 0, x1: 6400, y1: 25600 });
        add_zone_region(&proxy, "z_civic", Region { x0: 6400, y0: 6400, x1: 19200, y1: 19200 });
        add_zone_region(&proxy, "z_market", Region { x0: 19200, y0: 0, x1: 25600, y1: 25600 });

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

    /// #9 headline: pooling gathered items into a build order fills it, then
    /// completion pays building XP and spawns the structure — the full gateway
    /// path a zone's `build_contribute` drives. Build orders are commissioned at
    /// runtime now (by the mayor in practice; inserted directly here to isolate
    /// this from `mayor.build_create`'s own gating, covered separately).
    #[tokio::test]
    async fn build_contribute_completes_order_pays_xp() {
        let (proxy, dbf, zone) = proxy_with_db().await;
        let db = Db::connect(dbf.url()).await.unwrap();
        let (tcx, tcy) = mmo::world::capital().town_centre;
        let order = db
            .insert_build_order(
                "civic", "test_well", r#"{"wood":20,"stone":10}"#, "open", 0, None, 0,
                Some(mmo::persistence::BuildPlacement {
                    structure_kind: "well".to_string(),
                    x: tcx as i64, y: (tcy - 40) as i64, x1: None, y1: None,
                }),
                None,
            )
            .await
            .unwrap();

        let email = format!("b_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Builder"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();

        // Stand at the order's own location so the gateway's proximity gate passes.
        proxy.entity_state.lock().unwrap().insert(
            pid.clone(),
            EntityCache { x: tcx, y: tcy - 40, hp: 100, gather: None },
        );

        // Stock exactly the well's cost (wood 20 + stone 10), as gathering would.
        for (item, qty) in [("wood", 20), ("stone", 10)] {
            zone.to_proxy
                .send(Message::Text(json!({
                    "type": "gather_yield", "player_id": pid,
                    "item_id": item, "qty": qty, "skill": "gathering", "xp": 1,
                }).to_string()))
                .unwrap();
        }
        // Contribute both items to the well.
        for (item, qty) in [("wood", 20), ("stone", 10)] {
            zone.to_proxy
                .send(Message::Text(json!({
                    "type": "build_contribute", "player_id": pid,
                    "order_id": order.id, "item_id": item, "qty": qty,
                }).to_string()))
                .unwrap();
        }

        // Expect: progress, completion (with the well structure), a building skill
        // gain, and a building level-up (30 units → 150 XP → Building 1). Frames
        // interleave — scan once, check all.
        let (mut progressed, mut completed, mut built_xp, mut leveled) = (false, false, false, false);
        for _ in 0..80 {
            let Some(v) = recv_frame(&mut ws).await else { break };
            match v["type"].as_str() {
                Some("build.progress") if v["order_id"] == json!(order.id) => progressed = true,
                Some("build.completed") if v["order_id"] == json!(order.id) => {
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
                _ => {}
            }
            if progressed && completed && built_xp && leveled {
                break;
            }
        }
        assert!(progressed, "never saw build.progress");
        assert!(completed, "the order never completed");
        assert!(built_xp, "contributor never gained building XP");
        assert!(leveled, "contributor never got a building level-up");

        // Durable: the order is completed.
        let orders = db.build_orders_for_district("civic").await.unwrap();
        assert_eq!(orders.iter().find(|o| o.id == order.id).unwrap().state, "completed");

        drop(ws);
    }

    /// A regular player has no city-building authority: `mayor.build_create` is
    /// rejected outright, and no order is created.
    #[tokio::test]
    async fn mayor_build_create_rejects_non_mayor() {
        let (proxy, dbf, _zone) = proxy_with_db().await;
        let db = Db::connect(dbf.url()).await.unwrap();
        let email = format!("p_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Regular"}).to_string(),
        ))
        .await
        .unwrap();
        recv_until(&mut ws, "welcome").await;

        ws.send(Message::Text(json!({
            "type": "mayor.build_create", "district": "civic", "kind": "dirt_path",
            "structure_kind": "dirt_road", "required_json": "{\"stone\":5}",
            "x": 12800, "y": 12800, "x1": 13200, "y1": 12800,
        }).to_string()))
        .await
        .unwrap();

        let err = recv_until(&mut ws, "mayor.build_error").await;
        assert!(err["message"].as_str().unwrap().contains("mayor"));
        assert!(db.build_orders_for_district("civic").await.unwrap().is_empty());

        drop(ws);
    }

    /// The mayor may not commission work on land someone already owns — only on
    /// city-owned land (#55/dirt paths: this is the "city owned" gate).
    #[tokio::test]
    async fn mayor_build_create_rejects_privately_owned_land() {
        let (proxy, dbf, mut zone) = proxy_with_db().await;
        let db = Db::connect(dbf.url()).await.unwrap();
        db.seed_capital(&mmo::world::capital(), 0).await.unwrap();
        let mayor_hash = auth::hash_password("h").unwrap();
        db.seed_mayor_account(MAYOR_EMAIL, &mayor_hash, "The Mayor", 12800, 12800, 100, 0)
            .await
            .unwrap();

        // A regular player claims their starter suburbs plot.
        let mut owner_ws = dial(&proxy).await;
        let (_pid, bounds) = registered_with_plot(&proxy, &mut zone, &mut owner_ws, "Tenant").await;
        let (px, py) = (
            bounds["x"].as_i64().unwrap() + bounds["w"].as_i64().unwrap() / 2,
            bounds["y"].as_i64().unwrap() + bounds["h"].as_i64().unwrap() / 2,
        );

        let mut mayor_ws = dial(&proxy).await;
        mayor_ws.send(Message::Text(
            json!({"type": "login", "email": MAYOR_EMAIL, "password": "h"}).to_string(),
        ))
        .await
        .unwrap();
        recv_until(&mut mayor_ws, "welcome").await;

        mayor_ws.send(Message::Text(json!({
            "type": "mayor.build_create", "district": "suburbs", "kind": "dirt_path",
            "structure_kind": "dirt_road", "required_json": "{\"stone\":5}",
            "x": px, "y": py,
        }).to_string()))
        .await
        .unwrap();

        let err = recv_until(&mut mayor_ws, "mayor.build_error").await;
        assert!(err["message"].as_str().unwrap().contains("owned"));
        assert!(db.build_orders_for_district("suburbs").await.unwrap().is_empty());

        drop(owner_ws);
        drop(mayor_ws);
    }

    /// The headline path: the mayor commissions a dirt path on city land, and any
    /// player standing near it (not the civic board) can fill it, spawning a
    /// segment-shaped `dirt_road` structure.
    #[tokio::test]
    async fn mayor_build_create_dirt_path_then_contribute_completes() {
        let (proxy, dbf, zone) = proxy_with_db().await;
        let db = Db::connect(dbf.url()).await.unwrap();
        let mayor_hash = auth::hash_password("h").unwrap();
        db.seed_mayor_account(MAYOR_EMAIL, &mayor_hash, "The Mayor", 12800, 12800, 100, 0)
            .await
            .unwrap();

        let mut mayor_ws = dial(&proxy).await;
        mayor_ws.send(Message::Text(
            json!({"type": "login", "email": MAYOR_EMAIL, "password": "h"}).to_string(),
        ))
        .await
        .unwrap();
        recv_until(&mut mayor_ws, "welcome").await;

        // Well clear of the civic build board (town_centre - 30, +10) and of any
        // plot grid (only the suburbs has one) — plainly city land.
        let (x0, y0, x1, y1) = (12800, 4000, 13200, 4000);
        mayor_ws.send(Message::Text(json!({
            "type": "mayor.build_create", "district": "civic", "kind": "dirt_path",
            "structure_kind": "dirt_road", "required_json": "{\"stone\":5}",
            "x": x0, "y": y0, "x1": x1, "y1": y1,
        }).to_string()))
        .await
        .unwrap();
        // Login hydration already sent one (empty) `build.list` before the create
        // was processed — keep waiting until one actually lists the new order.
        let order_id = loop {
            let listed = recv_until(&mut mayor_ws, "build.list").await;
            let found = listed["orders"].as_array().unwrap().iter()
                .find(|o| o["kind"] == "dirt_path")
                .map(|o| o["order_id"].as_str().unwrap().to_string());
            if let Some(id) = found {
                break id;
            }
        };

        let email = format!("w_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Worker"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();

        // Stand at the path's start point, nowhere near the civic board.
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: x0, y: y0, hp: 100, gather: None });

        zone.to_proxy.send(Message::Text(json!({
            "type": "gather_yield", "player_id": pid,
            "item_id": "stone", "qty": 5, "skill": "gathering", "xp": 1,
        }).to_string())).unwrap();
        zone.to_proxy.send(Message::Text(json!({
            "type": "build_contribute", "player_id": pid,
            "order_id": order_id, "item_id": "stone", "qty": 5,
        }).to_string())).unwrap();

        let mut completed = false;
        for _ in 0..40 {
            let Some(v) = recv_frame(&mut ws).await else { break };
            if v["type"] == "build.completed" && v["order_id"] == json!(order_id) {
                let structs = v["structures"].as_array().cloned().unwrap_or_default();
                assert!(structs.iter().any(|s| {
                    s["kind"] == "dirt_road" && s["x"] == json!(x0) && s["x1"] == json!(x1)
                }), "dirt_road segment missing from build.completed: {structs:?}");
                completed = true;
                break;
            }
        }
        assert!(completed, "the dirt path never completed");

        drop(mayor_ws);
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
        // A runtime-commissioned order (as `mayor.build_create` would insert) so
        // there's civic content for `build.list` to report.
        db.insert_build_order("civic", "test_well", r#"{"wood":20}"#, "open", 0, None, 0, None, None)
            .await
            .unwrap();

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
                    assert!(orders.iter().any(|o| o["kind"] == "test_well"), "the civic board's content");
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
        let _suburbs_zone = add_zone_region(&proxy, "z_suburbs", Region { x0: 0, y0: 0, x1: 6400, y1: 25600 });

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

    /// #48: `district.enter` fires the instant the *client* detects it crossed
    /// a district gate — before the gateway's own position cache (updated
    /// asynchronously from the zone's status broadcasts) necessarily reflects
    /// it. If `district.enter`'s roster push re-derived the district from that
    /// cache (like the plain `plot.district` request does), it could read the
    /// *previous* district for a moment and hand back an empty/wrong roster —
    /// reproduced against a real client by sending `district.enter` immediately
    /// after movement with no settling delay. The fix: trust the client's own
    /// self-reported `to` directly for this read-only query.
    #[tokio::test]
    async fn district_enter_plot_roster_is_correct_even_with_a_stale_position_cache() {
        let (proxy, dbf, _zone) = proxy_with_db().await;
        let db = Db::connect(dbf.url()).await.unwrap();
        db.seed_capital(&mmo::world::capital(), 0).await.unwrap();

        let email = format!("racer_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Racer"}).to_string(),
        ))
        .await
        .unwrap();
        let welcome = recv_until(&mut ws, "welcome").await;
        let pid = welcome["player_id"].as_str().unwrap().to_string();
        recv_until(&mut ws, "plot.assigned").await;
        recv_until(&mut ws, "plot.district").await; // their own claim's broadcast

        // Simulate the race directly: the cache still says Civic (the town
        // centre spawn point) even though the client has already announced it
        // crossed into the Suburbs.
        proxy.entity_state.lock().unwrap().insert(
            pid.clone(),
            EntityCache { x: 12800, y: 12800, hp: 100, gather: None },
        );

        ws.send(Message::Text(
            json!({"type": "district.enter", "from": "civic", "to": "suburbs"}).to_string(),
        ))
        .await
        .unwrap();
        let roster = recv_until(&mut ws, "plot.district").await;
        let plots = roster["plots"].as_array().unwrap();
        assert_eq!(plots.len(), 240, "the Suburbs roster, trusting `to` directly, not the stale Civic-reading cache");

        drop(ws);
    }

    /// #54: `terrain.list` answers with the same authored heightmap grid
    /// `capital()` holds server-side — stateless, no DB/position involved,
    /// same shape as `craft.list`/`craft.recipes`.
    #[tokio::test]
    async fn terrain_list_answers_with_the_authored_heightmap() {
        let (proxy, _dbf, _zone) = proxy_with_db().await;

        let email = format!("surveyor_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Surveyor"}).to_string(),
        ))
        .await
        .unwrap();
        recv_until(&mut ws, "welcome").await;

        ws.send(Message::Text(json!({"type": "terrain.list"}).to_string())).await.unwrap();
        let msg = recv_until(&mut ws, "terrain.data").await;

        let expected = mmo::world::capital().terrain;
        let resolution = mmo::world::TERRAIN_RESOLUTION;
        assert_eq!(msg["resolution"].as_i64().unwrap(), resolution as i64);
        let heights: Vec<f64> = msg["heights"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
        let fine_n = (resolution + 1) as usize;
        assert_eq!(heights.len(), fine_n * fine_n);

        // Every sent corner must match an independent `sample_height` call
        // against the same loaded artifact — the wire message and
        // `capital()` must never disagree (that mismatch is exactly the bug
        // class #54 fixed).
        let step = WORLD_SIZE as f32 / resolution as f32;
        for gy in 0..fine_n {
            for gx in 0..fine_n {
                let want = expected.sample_height(gx as f32 * step, gy as f32 * step);
                let got = heights[gy * fine_n + gx];
                assert!(
                    (got - want as f64).abs() < 0.0001,
                    "heightmap sent over the wire must match capital()'s exactly at ({gx},{gy})"
                );
            }
        }

        // Terrain streaming: the same message must also carry the baked
        // artifact's own manifest shape, so the client knows what it can
        // additionally request at native resolution.
        let manifest = expected.manifest();
        assert_eq!(msg["tile_size"].as_u64().unwrap(), manifest.tile_size as u64);
        assert_eq!(msg["tiles"][0].as_u64().unwrap(), manifest.tiles.0 as u64);
        assert_eq!(msg["tiles"][1].as_u64().unwrap(), manifest.tiles.1 as u64);
        assert!((msg["cell_size_m"].as_f64().unwrap() - manifest.cell_size_m as f64).abs() < 0.0001);
        assert!((msg["height_min_m"].as_f64().unwrap() - manifest.height_min_m as f64).abs() < 0.0001);
        assert!((msg["height_max_m"].as_f64().unwrap() - manifest.height_max_m as f64).abs() < 0.0001);

        drop(ws);
    }

    /// Terrain streaming: `terrain.tile_request` answers with the requested
    /// tile's bytes, base64-wrapped, in exactly `HeightTile::encode`'s
    /// on-disk format — decoding it back must reproduce the same tile the
    /// baked artifact itself holds, so the streamed tile can never disagree
    /// with the coarse backdrop or the bake tool's own validation.
    #[tokio::test]
    async fn terrain_tile_request_answers_with_the_requested_tiles_bytes() {
        use base64::Engine;

        let (proxy, _dbf, _zone) = proxy_with_db().await;

        let email = format!("surveyor2_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Surveyor2"}).to_string(),
        ))
        .await
        .unwrap();
        recv_until(&mut ws, "welcome").await;

        let expected_terrain = mmo::world::capital().terrain;
        let expected_tile = expected_terrain.height_tile(0, 0).expect("tile (0,0) must exist in the production bake");

        ws.send(Message::Text(json!({"type": "terrain.tile_request", "tx": 0, "ty": 0}).to_string())).await.unwrap();
        let msg = recv_until(&mut ws, "terrain.tile_data").await;

        assert_eq!(msg["tx"].as_i64().unwrap(), 0);
        assert_eq!(msg["ty"].as_i64().unwrap(), 0);
        assert_eq!(msg["side"].as_u64().unwrap(), expected_tile.side as u64);
        assert_eq!(msg["encoding"].as_str().unwrap(), "tile_v1");

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(msg["data_b64"].as_str().unwrap())
            .expect("data_b64 must be valid base64");
        let decoded = terrain_common::HeightTile::decode(&bytes, expected_tile.side).expect("must decode as a valid HeightTile");
        for gy in 0..decoded.side {
            for gx in 0..decoded.side {
                assert_eq!(
                    decoded.get(gx, gy),
                    expected_tile.get(gx, gy),
                    "streamed tile sample ({gx},{gy}) must match the loaded artifact's own tile exactly"
                );
            }
        }

        drop(ws);
    }

    /// An out-of-range tile request (outside the manifest's tile grid) is
    /// silently ignored — same posture as every other directly-answered
    /// message in this dispatch loop when asked for something that doesn't
    /// exist. Confirmed by racing it against a real request that *does*
    /// answer, so a silent hang isn't mistaken for "the bad request also
    /// worked."
    #[tokio::test]
    async fn terrain_tile_request_out_of_range_is_silently_ignored() {
        let (proxy, _dbf, _zone) = proxy_with_db().await;

        let email = format!("surveyor3_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Surveyor3"}).to_string(),
        ))
        .await
        .unwrap();
        recv_until(&mut ws, "welcome").await;

        ws.send(Message::Text(json!({"type": "terrain.tile_request", "tx": 9999, "ty": 9999}).to_string()))
            .await
            .unwrap();
        // No terrain.tile_data for the bad request — but a subsequent good
        // request must still answer normally, proving the bad one didn't
        // wedge anything.
        ws.send(Message::Text(json!({"type": "terrain.tile_request", "tx": 0, "ty": 0}).to_string())).await.unwrap();
        let msg = recv_until(&mut ws, "terrain.tile_data").await;
        assert_eq!(msg["tx"].as_i64().unwrap(), 0);
        assert_eq!(msg["ty"].as_i64().unwrap(), 0);

        drop(ws);
    }

    // --- terrain editing (#75): terrain.delta_request / terrain.delta_data ----

    /// Like `proxy_with_db`, but also hands back the Db so a test can seed
    /// terrain-delta rows before the client asks for them.
    async fn proxy_with_shared_db() -> (Arc<Proxy>, Arc<Db>, TestDb, FakeZone) {
        let dbf = TestDb::new();
        let db = Arc::new(Db::connect(dbf.url()).await.unwrap());
        let proxy = Proxy::new("127.0.0.1", 0, 0, 0, Some(db.clone()));
        let zone = spawn_fake_zone().await;
        proxy
            .register_zone("zone_a".to_string(), zone.uri.clone(), 1, String::new(), Region::whole_world())
            .await;
        (proxy, db, dbf, zone)
    }

    /// An in-range chunk that has never been edited answers explicitly with
    /// `has_delta: false` — never silence. The client must not have to
    /// distinguish "not answered yet" from "answered, nothing here".
    #[tokio::test]
    async fn terrain_delta_request_unedited_chunk_answers_has_delta_false() {
        let (proxy, _db, _dbf, _zone) = proxy_with_shared_db().await;

        let email = format!("editor1_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Editor1"}).to_string(),
        ))
        .await
        .unwrap();
        recv_until(&mut ws, "welcome").await;

        ws.send(Message::Text(json!({"type": "terrain.delta_request", "tx": 4, "ty": 4}).to_string())).await.unwrap();
        let msg = recv_until(&mut ws, "terrain.delta_data").await;
        assert_eq!(msg["tx"].as_i64().unwrap(), 4);
        assert_eq!(msg["ty"].as_i64().unwrap(), 4);
        assert_eq!(msg["has_delta"].as_bool().unwrap(), false);
        assert!(msg.get("data_b64").is_none(), "no payload for an unedited chunk");

        drop(ws);
    }

    /// A chunk with a saved delta answers with base64-wrapped
    /// `SparseHeightDelta::encode` bytes that decode back to exactly the
    /// offsets that were stored, plus the row's revision.
    #[tokio::test]
    async fn terrain_delta_request_answers_with_the_saved_deltas_bytes() {
        use base64::Engine;

        let (proxy, db, _dbf, _zone) = proxy_with_shared_db().await;

        // Seed a delta for chunk (1, 2) straight through the persistence
        // layer — the write path (#77) doesn't exist yet.
        let manifest = mmo::world::capital().terrain.manifest().clone();
        let side = manifest.tile_size as usize + 1;
        let mut hd = terrain_common::SparseHeightDelta::new(side);
        hd.set_offset_cm(10, 10, 300);
        hd.set_offset_cm(100, 60, -150);
        let saved_rev = db
            .save_terrain_delta(&terrain_common::TerrainDelta {
                chunk_tx: 1,
                chunk_ty: 2,
                bake_hash: manifest.bake_hash.clone(),
                revision: 0,
                height_delta: Some(hd.clone()),
                provenance: terrain_common::Provenance {
                    author: terrain_common::AuthorId::Editor("test-editor".to_string()),
                    edited_at: 0,
                },
            })
            .await
            .unwrap();

        let email = format!("editor2_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Editor2"}).to_string(),
        ))
        .await
        .unwrap();
        recv_until(&mut ws, "welcome").await;

        ws.send(Message::Text(json!({"type": "terrain.delta_request", "tx": 1, "ty": 2}).to_string())).await.unwrap();
        let msg = recv_until(&mut ws, "terrain.delta_data").await;

        assert_eq!(msg["tx"].as_i64().unwrap(), 1);
        assert_eq!(msg["ty"].as_i64().unwrap(), 2);
        assert_eq!(msg["has_delta"].as_bool().unwrap(), true);
        assert_eq!(msg["revision"].as_u64().unwrap(), saved_rev);
        assert_eq!(msg["encoding"].as_str().unwrap(), "delta_v1");

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(msg["data_b64"].as_str().unwrap())
            .expect("data_b64 must be valid base64");
        let decoded = terrain_common::SparseHeightDelta::decode(&bytes, side).expect("must decode as a SparseHeightDelta");
        assert_eq!(decoded, hd, "streamed delta must match what was stored, block for block");

        drop(ws);
    }

    /// An out-of-range delta request is silently ignored (same posture as
    /// the tile path) — proven by racing it against an in-range request
    /// that answers, so silence isn't mistaken for success.
    #[tokio::test]
    async fn terrain_delta_request_out_of_range_is_silently_ignored() {
        let (proxy, _db, _dbf, _zone) = proxy_with_shared_db().await;

        let email = format!("editor3_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Editor3"}).to_string(),
        ))
        .await
        .unwrap();
        recv_until(&mut ws, "welcome").await;

        ws.send(Message::Text(json!({"type": "terrain.delta_request", "tx": 9999, "ty": -3}).to_string()))
            .await
            .unwrap();
        ws.send(Message::Text(json!({"type": "terrain.delta_request", "tx": 0, "ty": 0}).to_string())).await.unwrap();
        let msg = recv_until(&mut ws, "terrain.delta_data").await;
        assert_eq!(msg["tx"].as_i64().unwrap(), 0, "only the in-range request answered");
        assert_eq!(msg["ty"].as_i64().unwrap(), 0);

        drop(ws);
    }

    // --- terrain editing (#77): terrain.edit_op write path ---------------------

    /// Seed + log in the editor account; returns its socket.
    async fn dial_editor(proxy: &Arc<Proxy>, db: &Db) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
        let hash = auth::hash_password("h").unwrap();
        db.seed_account_with_role(EDITOR_EMAIL, &hash, "The Editor", 12800, 12800, 100, 0, "editor")
            .await
            .unwrap();
        let mut ws = dial(proxy).await;
        ws.send(Message::Text(
            json!({"type": "login", "email": EDITOR_EMAIL, "password": "h"}).to_string(),
        ))
        .await
        .unwrap();
        let welcome = recv_until(&mut ws, "welcome").await;
        assert_eq!(welcome["role"].as_str().unwrap(), "editor");
        ws
    }

    /// A non-editor's `terrain.edit_op` is rejected with an explicit error
    /// and persists nothing.
    #[tokio::test]
    async fn terrain_edit_op_is_rejected_for_non_editors() {
        let (proxy, db, _dbf, _zone) = proxy_with_shared_db().await;

        let email = format!("scrub_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Scrub"}).to_string(),
        ))
        .await
        .unwrap();
        recv_until(&mut ws, "welcome").await;

        ws.send(Message::Text(
            json!({"type": "terrain.edit_op", "brush": "raise", "cells": [[10, 10, 100]]}).to_string(),
        ))
        .await
        .unwrap();
        let err = recv_until(&mut ws, "terrain.edit_error").await;
        assert!(err["message"].as_str().unwrap().contains("editor"));

        let side = mmo::world::capital().terrain.manifest().tile_size as usize + 1;
        assert!(
            db.load_terrain_delta(0, 0, side).await.unwrap().is_none(),
            "a rejected op must persist nothing"
        );

        drop(ws);
    }

    /// A valid editor op persists (revision 1, then 2 on a second op), is
    /// broadcast as `terrain.delta_patch` to every connected client, and the
    /// patch bytes decode to the accumulated offsets.
    #[tokio::test]
    async fn terrain_edit_op_persists_bumps_revision_and_broadcasts() {
        use base64::Engine;

        let (proxy, db, _dbf, _zone) = proxy_with_shared_db().await;
        let mut editor_ws = dial_editor(&proxy, &db).await;

        // A second, regular client that should also receive the patch.
        let email = format!("watcher_{}@t.test", Uuid::new_v4().simple());
        let mut watcher_ws = dial(&proxy).await;
        watcher_ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Watcher"}).to_string(),
        ))
        .await
        .unwrap();
        recv_until(&mut watcher_ws, "welcome").await;

        // World corner (10,10) is interior to chunk (0,0): exactly one patch.
        editor_ws.send(Message::Text(
            json!({"type": "terrain.edit_op", "brush": "raise", "cells": [[10, 10, 250]]}).to_string(),
        ))
        .await
        .unwrap();

        let patch = recv_until(&mut editor_ws, "terrain.delta_patch").await;
        assert_eq!(patch["tx"].as_i64().unwrap(), 0);
        assert_eq!(patch["ty"].as_i64().unwrap(), 0);
        assert_eq!(patch["revision"].as_u64().unwrap(), 1);
        let watcher_patch = recv_until(&mut watcher_ws, "terrain.delta_patch").await;
        assert_eq!(watcher_patch["revision"].as_u64().unwrap(), 1, "the patch reaches every client");

        // Second op on the same corner: accumulates and bumps the revision.
        editor_ws.send(Message::Text(
            json!({"type": "terrain.edit_op", "brush": "raise", "cells": [[10, 10, 150]]}).to_string(),
        ))
        .await
        .unwrap();
        let patch2 = recv_until(&mut editor_ws, "terrain.delta_patch").await;
        assert_eq!(patch2["revision"].as_u64().unwrap(), 2);

        let side = mmo::world::capital().terrain.manifest().tile_size as usize + 1;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(patch2["data_b64"].as_str().unwrap())
            .unwrap();
        let hd = terrain_common::SparseHeightDelta::decode(&bytes, side).unwrap();
        assert_eq!(hd.offset_cm(10, 10), 400, "250 + 150 accumulated");

        // And it's durable: the persistence layer holds the same state.
        let stored = db.load_terrain_delta(0, 0, side).await.unwrap().expect("row exists");
        assert_eq!(stored.revision, 2);
        assert_eq!(stored.height_delta.unwrap().offset_cm(10, 10), 400);
        assert!(
            matches!(stored.provenance.author, terrain_common::AuthorId::Editor(_)),
            "provenance records the editor"
        );

        drop(editor_ws);
        drop(watcher_ws);
    }

    /// A corner exactly on a chunk seam (cx == tile_size) must be written
    /// into BOTH chunks' deltas — the duplicated-edge convention — or the
    /// two meshes would disagree along the seam.
    #[tokio::test]
    async fn terrain_edit_op_on_a_seam_updates_both_chunks() {
        use base64::Engine;

        let (proxy, db, _dbf, _zone) = proxy_with_shared_db().await;
        let mut ws = dial_editor(&proxy, &db).await;

        let manifest = mmo::world::capital().terrain.manifest().clone();
        let ts = manifest.tile_size as i64;
        let side = manifest.tile_size as usize + 1;
        // World corner (ts, 5) = chunk (0,0)'s last column = chunk (1,0)'s first.
        ws.send(Message::Text(
            json!({"type": "terrain.edit_op", "brush": "raise", "cells": [[ts, 5, 300]]}).to_string(),
        ))
        .await
        .unwrap();

        // Two patches, one per chunk, in either order.
        let mut patched: Vec<(i64, i64)> = Vec::new();
        for _ in 0..2 {
            let patch = recv_until(&mut ws, "terrain.delta_patch").await;
            let (tx, ty) = (patch["tx"].as_i64().unwrap(), patch["ty"].as_i64().unwrap());
            patched.push((tx, ty));
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(patch["data_b64"].as_str().unwrap())
                .unwrap();
            let hd = terrain_common::SparseHeightDelta::decode(&bytes, side).unwrap();
            // Chunk (0,0) stores the seam corner as its last column (gx =
            // side-1); chunk (1,0) as its first (gx = 0). Same world data.
            let gx = if tx == 0 { side - 1 } else { 0 };
            assert_eq!(hd.offset_cm(gx, 5), 300, "chunk ({tx},{ty}) must store the seam offset");
        }
        patched.sort();
        assert_eq!(patched, vec![(0, 0), (1, 0)], "one seam corner, two patched chunks");

        // Durable on both sides too.
        let a = db.load_terrain_delta(0, 0, side).await.unwrap().unwrap();
        let b = db.load_terrain_delta(1, 0, side).await.unwrap().unwrap();
        assert_eq!(a.height_delta.unwrap().offset_cm(side - 1, 5), 300);
        assert_eq!(b.height_delta.unwrap().offset_cm(0, 5), 300);

        drop(ws);
    }

    /// Bounds and caps: out-of-world corners, over-cap increments, and an
    /// accumulation that would breach the total cap are all rejected whole,
    /// persisting nothing beyond what was already there.
    #[tokio::test]
    async fn terrain_edit_op_rejects_out_of_bounds_and_over_cap() {
        let (proxy, db, _dbf, _zone) = proxy_with_shared_db().await;
        let mut ws = dial_editor(&proxy, &db).await;
        let side = mmo::world::capital().terrain.manifest().tile_size as usize + 1;

        // Out of world bounds.
        ws.send(Message::Text(
            json!({"type": "terrain.edit_op", "brush": "raise", "cells": [[99999, 5, 100]]}).to_string(),
        ))
        .await
        .unwrap();
        recv_until(&mut ws, "terrain.edit_error").await;

        // Single increment over the cap.
        ws.send(Message::Text(
            json!({"type": "terrain.edit_op", "brush": "raise", "cells": [[10, 10, 5001]]}).to_string(),
        ))
        .await
        .unwrap();
        recv_until(&mut ws, "terrain.edit_error").await;

        // Two legal increments whose accumulation breaches the cap: the
        // first lands, the second is rejected and changes nothing.
        ws.send(Message::Text(
            json!({"type": "terrain.edit_op", "brush": "raise", "cells": [[10, 10, 4000]]}).to_string(),
        ))
        .await
        .unwrap();
        recv_until(&mut ws, "terrain.delta_patch").await;
        ws.send(Message::Text(
            json!({"type": "terrain.edit_op", "brush": "raise", "cells": [[10, 10, 4000]]}).to_string(),
        ))
        .await
        .unwrap();
        recv_until(&mut ws, "terrain.edit_error").await;

        let stored = db.load_terrain_delta(0, 0, side).await.unwrap().unwrap();
        assert_eq!(stored.revision, 1, "the rejected op must not bump the revision");
        assert_eq!(stored.height_delta.unwrap().offset_cm(10, 10), 4000, "first op only");

        drop(ws);
    }

    // --- terrain editing (#79): undo via terrain.revert_op ---------------------

    /// Undo-last restores exactly the pre-edit state, layer by layer: after
    /// op1 (+300) then op2 (+150 same corner), reverting op2 lands back on
    /// 300 exactly, and reverting op1 deletes the block outright (it didn't
    /// exist before op1). Each revert broadcasts a patch and acks.
    #[tokio::test]
    async fn terrain_revert_op_restores_pre_edit_blocks_exactly() {
        use base64::Engine;

        let (proxy, db, _dbf, _zone) = proxy_with_shared_db().await;
        let mut ws = dial_editor(&proxy, &db).await;
        let side = mmo::world::capital().terrain.manifest().tile_size as usize + 1;

        ws.send(Message::Text(
            json!({"type": "terrain.edit_op", "brush": "raise", "cells": [[10, 10, 300]]}).to_string(),
        ))
        .await
        .unwrap();
        let ack1 = recv_until(&mut ws, "terrain.edit_ack").await;
        let op1 = ack1["op_id"].as_str().unwrap().to_string();
        assert_eq!(ack1["brush"].as_str().unwrap(), "raise");
        recv_until(&mut ws, "terrain.delta_patch").await;

        ws.send(Message::Text(
            json!({"type": "terrain.edit_op", "brush": "raise", "cells": [[10, 10, 150]]}).to_string(),
        ))
        .await
        .unwrap();
        let op2 = recv_until(&mut ws, "terrain.edit_ack").await["op_id"].as_str().unwrap().to_string();
        recv_until(&mut ws, "terrain.delta_patch").await;
        assert_ne!(op1, op2, "each op gets its own id");

        // Revert op2: back to exactly 300, revision bumped (3), patch decodes.
        ws.send(Message::Text(json!({"type": "terrain.revert_op", "op_id": op2}).to_string())).await.unwrap();
        let patch = recv_until(&mut ws, "terrain.delta_patch").await;
        assert_eq!(patch["revision"].as_u64().unwrap(), 3, "a revert bumps the revision like any edit");
        let bytes = base64::engine::general_purpose::STANDARD.decode(patch["data_b64"].as_str().unwrap()).unwrap();
        let hd = terrain_common::SparseHeightDelta::decode(&bytes, side).unwrap();
        assert_eq!(hd.offset_cm(10, 10), 300, "revert of op2 restores op1's exact state");
        let ack = recv_until(&mut ws, "terrain.revert_ack").await;
        assert_eq!(ack["op_id"].as_str().unwrap(), op2);

        // Revert op1: the block didn't exist before it — deleted outright.
        ws.send(Message::Text(json!({"type": "terrain.revert_op", "op_id": op1}).to_string())).await.unwrap();
        let patch2 = recv_until(&mut ws, "terrain.delta_patch").await;
        let bytes2 = base64::engine::general_purpose::STANDARD.decode(patch2["data_b64"].as_str().unwrap()).unwrap();
        let hd2 = terrain_common::SparseHeightDelta::decode(&bytes2, side).unwrap();
        assert!(hd2.is_empty(), "revert of the creating op deletes the block");
        recv_until(&mut ws, "terrain.revert_ack").await;

        let stored = db.load_terrain_delta(0, 0, side).await.unwrap().unwrap();
        assert!(
            stored.height_delta.is_none(),
            "durably back to procedural: a fully-reverted chunk stores NO height layer, so it round-trips as has_delta: false"
        );

        drop(ws);
    }

    /// Double reverts, unknown ids, and non-editor reverts are all rejected
    /// cleanly with terrain.edit_error — never a panic, never a second apply.
    #[tokio::test]
    async fn terrain_revert_op_rejects_double_unknown_and_non_editor() {
        let (proxy, db, _dbf, _zone) = proxy_with_shared_db().await;
        let mut ws = dial_editor(&proxy, &db).await;
        let side = mmo::world::capital().terrain.manifest().tile_size as usize + 1;

        ws.send(Message::Text(
            json!({"type": "terrain.edit_op", "brush": "raise", "cells": [[20, 20, 500]]}).to_string(),
        ))
        .await
        .unwrap();
        let op = recv_until(&mut ws, "terrain.edit_ack").await["op_id"].as_str().unwrap().to_string();
        recv_until(&mut ws, "terrain.delta_patch").await;

        ws.send(Message::Text(json!({"type": "terrain.revert_op", "op_id": op}).to_string())).await.unwrap();
        recv_until(&mut ws, "terrain.revert_ack").await;

        // Second revert of the same op: rejected, and the state is untouched.
        ws.send(Message::Text(json!({"type": "terrain.revert_op", "op_id": op}).to_string())).await.unwrap();
        let err = recv_until(&mut ws, "terrain.edit_error").await;
        assert!(err["message"].as_str().unwrap().contains("already-reverted") || err["message"].as_str().unwrap().contains("unknown"));

        // Unknown id: same rejection.
        ws.send(Message::Text(json!({"type": "terrain.revert_op", "op_id": "nope"}).to_string())).await.unwrap();
        recv_until(&mut ws, "terrain.edit_error").await;

        let stored = db.load_terrain_delta(0, 0, side).await.unwrap().unwrap();
        assert!(stored.height_delta.is_none(), "rejected reverts change nothing (still no height layer)");

        // Non-editor revert: role-gated like edit_op.
        let email = format!("scrub2_{}@t.test", Uuid::new_v4().simple());
        let mut player_ws = dial(&proxy).await;
        player_ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Scrub2"}).to_string(),
        ))
        .await
        .unwrap();
        recv_until(&mut player_ws, "welcome").await;
        player_ws.send(Message::Text(json!({"type": "terrain.revert_op", "op_id": op}).to_string())).await.unwrap();
        let err2 = recv_until(&mut player_ws, "terrain.edit_error").await;
        assert!(err2["message"].as_str().unwrap().contains("editor"));

        drop(ws);
        drop(player_ws);
    }

    /// A seam-crossing op reverts on BOTH chunks (its snapshots span them).
    #[tokio::test]
    async fn terrain_revert_op_spans_chunks_like_the_op_did() {
        let (proxy, db, _dbf, _zone) = proxy_with_shared_db().await;
        let mut ws = dial_editor(&proxy, &db).await;
        let manifest = mmo::world::capital().terrain.manifest().clone();
        let ts = manifest.tile_size as i64;
        let side = manifest.tile_size as usize + 1;

        ws.send(Message::Text(
            json!({"type": "terrain.edit_op", "brush": "raise", "cells": [[ts, 7, 250]]}).to_string(),
        ))
        .await
        .unwrap();
        let op = recv_until(&mut ws, "terrain.edit_ack").await["op_id"].as_str().unwrap().to_string();
        recv_until(&mut ws, "terrain.delta_patch").await;
        recv_until(&mut ws, "terrain.delta_patch").await;

        ws.send(Message::Text(json!({"type": "terrain.revert_op", "op_id": op}).to_string())).await.unwrap();
        recv_until(&mut ws, "terrain.delta_patch").await;
        recv_until(&mut ws, "terrain.delta_patch").await;
        recv_until(&mut ws, "terrain.revert_ack").await;

        let a = db.load_terrain_delta(0, 0, side).await.unwrap().unwrap();
        let b = db.load_terrain_delta(1, 0, side).await.unwrap().unwrap();
        assert!(a.height_delta.is_none(), "chunk (0,0) back to procedural (no height layer)");
        assert!(b.height_delta.is_none(), "chunk (1,0) back to procedural (no height layer)");

        drop(ws);
    }

    // --- terrain editing (#80): the server's composited height answer ----------

    /// The #80 invariant, end-to-end through the real write path: after an
    /// edit op lands over the wire, `composited_ground_height` answers base
    /// + delta exactly; after the op is reverted, it answers base again,
    /// bit-exactly. An untouched point never moves, and the coarse backdrop
    /// (`terrain.data`) deliberately keeps answering base throughout.
    #[tokio::test]
    async fn composited_ground_height_follows_edits_and_reverts() {
        let (proxy, db, _dbf, _zone) = proxy_with_shared_db().await;
        let mut ws = dial_editor(&proxy, &db).await;

        let manifest = mmo::world::capital().terrain.manifest().clone();
        let cell = manifest.cell_size_m;
        // World corner (40, 40) — interior of chunk (0,0); sample exactly on
        // the corner so the expected lift is the full 250cm, no interpolation.
        let (wx, wy) = (40.0 * cell, 40.0 * cell);
        let (ux, uy) = (100.0 * cell, 100.0 * cell); // untouched control point

        let base = proxy.capital.terrain.sample_height(wx, wy);
        assert_eq!(
            proxy.composited_ground_height(wx, wy).await,
            base,
            "no delta row -> composited answer IS the base, bit-exactly"
        );

        ws.send(Message::Text(
            json!({"type": "terrain.edit_op", "brush": "raise", "cells": [[40, 40, 250]]}).to_string(),
        ))
        .await
        .unwrap();
        let op = recv_until(&mut ws, "terrain.edit_ack").await["op_id"].as_str().unwrap().to_string();
        recv_until(&mut ws, "terrain.delta_patch").await;

        let edited = proxy.composited_ground_height(wx, wy).await;
        assert!(
            (edited - (base + 2.5)).abs() < 0.001,
            "composited height must be base + 2.5m (base={base}, got={edited})"
        );
        let control_base = proxy.capital.terrain.sample_height(ux, uy);
        assert_eq!(
            proxy.composited_ground_height(ux, uy).await,
            control_base,
            "an untouched point in the same chunk must not move"
        );
        // The coarse backdrop wire message stays base — it's a static,
        // once-per-session payload (see composited_ground_height's doc).
        assert_eq!(proxy.capital.terrain.sample_height(wx, wy), base);

        ws.send(Message::Text(json!({"type": "terrain.revert_op", "op_id": op}).to_string())).await.unwrap();
        recv_until(&mut ws, "terrain.revert_ack").await;
        assert_eq!(
            proxy.composited_ground_height(wx, wy).await,
            base,
            "after revert the composited answer is the base again, bit-exactly"
        );

        drop(ws);
    }

    /// db-less mode (the proxy can boot without persistence): the composited
    /// answer degrades to base rather than erroring.
    #[tokio::test]
    async fn composited_ground_height_without_a_db_answers_base() {
        let proxy = test_proxy(); // Proxy::new(..., None)
        let base = proxy.capital.terrain.sample_height(500.0, 500.0);
        assert_eq!(proxy.composited_ground_height(500.0, 500.0).await, base);
    }

    // --- placed world props (#85): object.list / object.place / object.delete --

    /// A non-editor's place and delete are both rejected with an explicit
    /// error, and nothing persists.
    #[tokio::test]
    async fn object_place_and_delete_are_rejected_for_non_editors() {
        let (proxy, db, _dbf, _zone) = proxy_with_shared_db().await;

        let email = format!("scrub_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Scrub"}).to_string(),
        ))
        .await
        .unwrap();
        recv_until(&mut ws, "welcome").await;

        ws.send(Message::Text(
            json!({"type": "object.place", "kind": "poison_tree", "x": 100, "y": 200}).to_string(),
        ))
        .await
        .unwrap();
        let err = recv_until(&mut ws, "object.edit_error").await;
        assert!(err["message"].as_str().unwrap().contains("editor"));

        ws.send(Message::Text(
            json!({"type": "object.delete", "object_id": "whatever"}).to_string(),
        ))
        .await
        .unwrap();
        let err = recv_until(&mut ws, "object.edit_error").await;
        assert!(err["message"].as_str().unwrap().contains("editor"));

        assert!(db.list_world_objects().await.unwrap().is_empty(), "a rejected op must persist nothing");
        drop(ws);
    }

    /// The full editor round-trip: place broadcasts `object.placed` to every
    /// client (a bystander included), `object.list` answers the roster, delete
    /// broadcasts `object.removed`, and the roster empties again.
    #[tokio::test]
    async fn object_place_list_delete_round_trip_with_broadcasts() {
        let (proxy, db, _dbf, _zone) = proxy_with_shared_db().await;
        let mut editor_ws = dial_editor(&proxy, &db).await;

        // A plain-player bystander, connected before the placement.
        let email = format!("watcher_{}@t.test", Uuid::new_v4().simple());
        let mut watcher_ws = dial(&proxy).await;
        watcher_ws
            .send(Message::Text(
                json!({"type": "register", "email": email, "password": "pw12", "name": "Watcher"}).to_string(),
            ))
            .await
            .unwrap();
        recv_until(&mut watcher_ws, "welcome").await;

        editor_ws
            .send(Message::Text(
                json!({"type": "object.place", "kind": "poison_tree", "x": 12700, "y": 12750}).to_string(),
            ))
            .await
            .unwrap();
        let placed = recv_until(&mut editor_ws, "object.placed").await;
        let id = placed["id"].as_str().unwrap().to_string();
        assert_eq!(placed["kind"].as_str().unwrap(), "poison_tree");
        assert_eq!(placed["x"].as_i64().unwrap(), 12700);
        assert_eq!(placed["y"].as_i64().unwrap(), 12750);
        let seen = recv_until(&mut watcher_ws, "object.placed").await;
        assert_eq!(seen["id"].as_str().unwrap(), id, "the bystander sees the same placement");

        // The roster answers from the cache, and the row is durable.
        watcher_ws
            .send(Message::Text(json!({"type": "object.list"}).to_string()))
            .await
            .unwrap();
        let roster = recv_until(&mut watcher_ws, "object.list").await;
        let objects = roster["objects"].as_array().unwrap();
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0]["id"].as_str().unwrap(), id);
        assert_eq!(db.list_world_objects().await.unwrap().len(), 1);

        editor_ws
            .send(Message::Text(json!({"type": "object.delete", "object_id": id}).to_string()))
            .await
            .unwrap();
        let removed = recv_until(&mut watcher_ws, "object.removed").await;
        assert_eq!(removed["id"].as_str().unwrap(), id, "the bystander sees the removal too");
        recv_until(&mut editor_ws, "object.removed").await;

        watcher_ws
            .send(Message::Text(json!({"type": "object.list"}).to_string()))
            .await
            .unwrap();
        let roster = recv_until(&mut watcher_ws, "object.list").await;
        assert!(roster["objects"].as_array().unwrap().is_empty(), "the roster empties after delete");
        assert!(db.list_world_objects().await.unwrap().is_empty());

        drop(editor_ws);
        drop(watcher_ws);
    }

    /// Kind and bounds are validated with explicit errors; a delete of an
    /// unknown id errors instead of broadcasting.
    #[tokio::test]
    async fn object_place_validates_kind_bounds_and_delete_validates_existence() {
        let (proxy, db, _dbf, _zone) = proxy_with_shared_db().await;
        let mut ws = dial_editor(&proxy, &db).await;

        ws.send(Message::Text(
            json!({"type": "object.place", "kind": "chocolate_teapot", "x": 100, "y": 100}).to_string(),
        ))
        .await
        .unwrap();
        let err = recv_until(&mut ws, "object.edit_error").await;
        assert!(err["message"].as_str().unwrap().contains("kind"));

        ws.send(Message::Text(
            json!({"type": "object.place", "kind": "poison_tree", "x": -1, "y": 100}).to_string(),
        ))
        .await
        .unwrap();
        let err = recv_until(&mut ws, "object.edit_error").await;
        assert!(err["message"].as_str().unwrap().contains("outside"));

        ws.send(Message::Text(
            json!({"type": "object.place", "kind": "poison_tree", "y": 100}).to_string(),
        ))
        .await
        .unwrap();
        let err = recv_until(&mut ws, "object.edit_error").await;
        assert!(err["message"].as_str().unwrap().contains("malformed"));

        ws.send(Message::Text(
            json!({"type": "object.delete", "object_id": "no-such-id"}).to_string(),
        ))
        .await
        .unwrap();
        let err = recv_until(&mut ws, "object.edit_error").await;
        assert!(err["message"].as_str().unwrap().contains("no such object"));

        assert!(db.list_world_objects().await.unwrap().is_empty(), "nothing persisted by any rejected op");
        drop(ws);
    }

    /// Placed objects survive a gateway restart: a second proxy over the same
    /// DB hydrates its cache from the table and serves the same roster.
    #[tokio::test]
    async fn object_roster_survives_a_gateway_restart() {
        let dbf = TestDb::new();
        let db = Arc::new(Db::connect(dbf.url()).await.unwrap());

        let proxy1 = Proxy::new("127.0.0.1", 0, 0, 0, Some(db.clone()));
        let zone1 = spawn_fake_zone().await;
        proxy1
            .register_zone("zone_a".to_string(), zone1.uri.clone(), 1, String::new(), Region::whole_world())
            .await;
        let mut editor_ws = dial_editor(&proxy1, &db).await;
        editor_ws
            .send(Message::Text(
                json!({"type": "object.place", "kind": "poison_tree", "x": 5000, "y": 6000}).to_string(),
            ))
            .await
            .unwrap();
        let placed = recv_until(&mut editor_ws, "object.placed").await;
        let id = placed["id"].as_str().unwrap().to_string();
        drop(editor_ws);

        // "Restart": a brand-new proxy instance over the same database.
        let proxy2 = Proxy::new("127.0.0.1", 0, 0, 0, Some(db.clone()));
        let zone2 = spawn_fake_zone().await;
        proxy2
            .register_zone("zone_a".to_string(), zone2.uri.clone(), 1, String::new(), Region::whole_world())
            .await;
        let mut ws = dial_editor(&proxy2, &db).await;
        ws.send(Message::Text(json!({"type": "object.list"}).to_string())).await.unwrap();
        let roster = recv_until(&mut ws, "object.list").await;
        let objects = roster["objects"].as_array().unwrap();
        assert_eq!(objects.len(), 1, "the restarted gateway must hydrate its cache from the table");
        assert_eq!(objects[0]["id"].as_str().unwrap(), id);
        assert_eq!(objects[0]["x"].as_i64().unwrap(), 5000);

        drop(ws);
    }

    // --- vitals (#87): the gateway environment tick ----------------------------

    /// Skip zone-bound frames until an `env_state` for `pid` arrives.
    async fn recv_env_state(zone: &mut FakeZone, pid: &str) -> Value {
        loop {
            let v = recv_value(&mut zone.from_proxy).await;
            if v["type"] == "env_state" && v["player_id"] == pid {
                return v;
            }
        }
    }

    /// `env_tick_once` pushes each connected player's flags to their owning
    /// zone: dry on the high-ground spawn, submerged over the genuinely deep
    /// river/bay channel, and submerged in an editor-dug pit below sea level
    /// — the last one proving the check reads *composited* ground (#80's
    /// door), not the immutable base bake.
    #[tokio::test]
    async fn env_tick_flags_deep_water_dry_land_and_editor_dug_ponds() {
        let (proxy, db, _dbf, mut zone) = proxy_with_shared_db().await;

        let email = format!("swimmer_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Swimmer"}).to_string(),
        ))
        .await
        .unwrap();
        let welcome = recv_until(&mut ws, "welcome").await;
        let pid = welcome["player_id"].as_str().unwrap().to_string();

        // 1. At the spawn point (high ground, seeded into entity_state by the
        //    welcome relocate): dry.
        proxy.env_tick_once().await;
        let flags = recv_env_state(&mut zone, &pid).await;
        assert_eq!(flags["submerged"], false, "the town-centre spawn must not drown anyone");

        // 2. Standing in the river/bay (the baked water mask): submerged.
        //    Scan for a mask cell with a 100m margin of water around it (mid
        //    river, not a shoreline corner case) rather than hard-coding one.
        let terrain = &proxy.capital.terrain;
        let sea = terrain.manifest().sea_level_m;
        let mut wet = None;
        'scan: for gy in (0..WORLD_SIZE).step_by(400) {
            for gx in (0..WORLD_SIZE).step_by(400) {
                let all_water = [(0, 0), (100, 0), (-100, 0), (0, 100), (0, -100)]
                    .iter()
                    .all(|(ox, oy)| terrain.is_water((gx + ox) as f32, (gy + oy) as f32));
                if all_water {
                    wet = Some((gx, gy));
                    break 'scan;
                }
            }
        }
        let (dx, dy) = wet.expect("the v3 bake has open water (~10% of the world is masked)");
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: dx, y: dy, hp: 100, gather: None });
        proxy.env_tick_once().await;
        let flags = recv_env_state(&mut zone, &pid).await;
        assert_eq!(flags["submerged"], true, "open water at ({dx},{dy}) must submerge even over the flat 0m NoData fill");

        // 3. An editor-dug pit on dry land: dig one corner of the plot field
        //    below sea level via the delta store, stand there, and the
        //    *composited* ground must read as underwater.
        let (px, py) = (50.0f32, 50.0f32); // corner (10,10) of chunk (0,0), flattened plot field
        let base = terrain.sample_height(px, py);
        assert!(!terrain.is_water(px, py), "precondition: the pit site is not mask water");
        assert!(sea - base < SUBMERGED_DEPTH_M, "precondition: the pit site starts dry");
        let side = terrain.manifest().tile_size as usize + 1;
        let mut hd = terrain_common::SparseHeightDelta::new(side);
        let dig_cm = -(((base - sea) + SUBMERGED_DEPTH_M + 1.0) * 100.0);
        hd.set_offset_cm(10, 10, dig_cm as i16);
        db.save_terrain_delta(&terrain_common::TerrainDelta {
            chunk_tx: 0,
            chunk_ty: 0,
            bake_hash: terrain.manifest().bake_hash.clone(),
            revision: 0,
            height_delta: Some(hd),
            provenance: terrain_common::Provenance {
                author: terrain_common::AuthorId::Editor("test-digger".to_string()),
                edited_at: 0,
            },
        })
        .await
        .unwrap();
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: px as i32, y: py as i32, hp: 100, gather: None });
        proxy.env_tick_once().await;
        let flags = recv_env_state(&mut zone, &pid).await;
        assert_eq!(flags["submerged"], true, "an editor-dug pond must count — the check reads composited ground");

        drop(ws);
    }

    // --- road plans (#94): road.plan -> build order -----------------------------

    /// A non-editor's `road.plan` is rejected with an explicit error and
    /// persists nothing.
    #[tokio::test]
    async fn road_plan_is_rejected_for_non_editors() {
        let (proxy, db, _dbf, _zone) = proxy_with_shared_db().await;

        let email = format!("paver_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Paver"}).to_string(),
        ))
        .await
        .unwrap();
        recv_until(&mut ws, "welcome").await;

        ws.send(Message::Text(
            json!({"type": "road.plan", "points": [[12800, 12800], [12900, 12800]]}).to_string(),
        ))
        .await
        .unwrap();
        let err = recv_until(&mut ws, "road.plan_error").await;
        assert!(err["message"].as_str().unwrap().contains("editor"));

        let orders = db.build_orders_for_district("civic").await.unwrap();
        assert!(!orders.iter().any(|o| o.kind.starts_with("road_")), "a rejected plan must persist nothing");
        drop(ws);
    }

    /// A valid L-shaped plan creates ONE ordinary build order: stone cost
    /// scaled by total length, placement = the first run, the full path in
    /// path_json, district resolved server-side — and the editor gets the
    /// `road.planned` ack.
    #[tokio::test]
    async fn road_plan_creates_a_length_costed_order_with_the_full_path() {
        let (proxy, db, _dbf, _zone) = proxy_with_shared_db().await;
        let mut ws = dial_editor(&proxy, &db).await;

        // Two runs: 100m east then 200m south = 300m -> 75 stone.
        let points = json!([[12800, 12800], [12900, 12800], [12900, 13000]]);
        ws.send(Message::Text(json!({"type": "road.plan", "points": points}).to_string()))
            .await
            .unwrap();
        let ack = recv_until(&mut ws, "road.planned").await;
        let order_id = ack["order_id"].as_str().unwrap().to_string();

        let orders = db.build_orders_for_district("civic").await.unwrap();
        let order = orders.iter().find(|o| o.id == order_id).expect("the acked order exists");
        assert!(order.kind.starts_with("road_"));
        assert_eq!(order.state, "open");
        let required: Value = serde_json::from_str(&order.required_json).unwrap();
        assert_eq!(required["stone"].as_i64().unwrap(), 75, "300m at 1 stone / 4m");
        assert_eq!(order.structure_kind.as_deref(), Some("dirt_road"));
        assert_eq!((order.x, order.y, order.x1, order.y1), (Some(12800), Some(12800), Some(12900), Some(12800)),
            "placement carries the FIRST run for segment-based consumers");
        let path: Value = serde_json::from_str(order.path_json.as_deref().unwrap()).unwrap();
        assert_eq!(path, json!([[12800, 12800], [12900, 12800], [12900, 13000]]),
            "the full polyline rides path_json");
        drop(ws);
    }

    /// Geometry validation: diagonal runs, repeated points, too-short plans,
    /// off-world points, and over-cap total length are all explicit errors,
    /// and a stub road still costs the minimum stone.
    #[tokio::test]
    async fn road_plan_validates_geometry_and_floors_the_cost() {
        let (proxy, db, _dbf, _zone) = proxy_with_shared_db().await;
        let mut ws = dial_editor(&proxy, &db).await;

        let cases = [
            (json!([[100, 100], [100, 100]]), "degenerate"),
            (json!([[100, 100]]), "two points"),
            (json!([[100, 100], [100, -5]]), "outside the world"),
            (json!([[100, 100], [100, 4200]]), "length cap"),
        ];
        for (points, want) in cases {
            ws.send(Message::Text(json!({"type": "road.plan", "points": points}).to_string()))
                .await
                .unwrap();
            let err = recv_until(&mut ws, "road.plan_error").await;
            assert!(
                err["message"].as_str().unwrap().contains(want),
                "expected '{want}' in: {}",
                err["message"]
            );
        }
        assert!(
            !db.build_orders_for_district("civic").await.unwrap().iter().any(|o| o.kind.starts_with("road_")),
            "nothing persisted by any rejected plan"
        );

        // A 4m stub still costs the ROAD_MIN_STONE floor.
        ws.send(Message::Text(
            json!({"type": "road.plan", "points": [[12800, 12800], [12804, 12800]]}).to_string(),
        ))
        .await
        .unwrap();
        let ack = recv_until(&mut ws, "road.planned").await;
        let order = db.build_order_by_id(ack["order_id"].as_str().unwrap()).await.unwrap().unwrap();
        let required: Value = serde_json::from_str(&order.required_json).unwrap();
        assert_eq!(required["stone"].as_i64().unwrap(), ROAD_MIN_STONE, "stub roads cost the floor");
        drop(ws);
    }

    /// A plan crossing a claimed starter plot is rejected — roads are city
    /// work on city land, same rule as the mayor's tool.
    #[tokio::test]
    async fn road_plan_rejects_privately_owned_land() {
        let (proxy, dbf, mut zone) = proxy_with_db().await;
        let db = Arc::new(Db::connect(dbf.url()).await.unwrap());
        db.seed_capital(&mmo::world::capital(), 0).await.unwrap();

        // A registered player claims their starter plot (in the suburbs).
        let mut player_ws = dial(&proxy).await;
        let (_pid, bounds) = registered_with_plot(&proxy, &mut zone, &mut player_ws, "Homeowner").await;
        let px = bounds["x"].as_i64().unwrap() + bounds["w"].as_i64().unwrap() / 2;
        let py = bounds["y"].as_i64().unwrap() + bounds["h"].as_i64().unwrap() / 2;

        let mut ws = dial_editor(&proxy, &db).await;
        // A run starting on the claimed plot (the first starter plot sits at
        // the world's NW corner, so extend east rather than straddle it).
        ws.send(Message::Text(
            json!({"type": "road.plan", "points": [[px, py], [px + 150, py]]}).to_string(),
        ))
        .await
        .unwrap();
        let err = recv_until(&mut ws, "road.plan_error").await;
        assert!(
            err["message"].as_str().unwrap().contains("privately owned"),
            "unexpected rejection: {}",
            err["message"]
        );

        drop(ws);
        drop(player_ws);
    }

    /// A road order accepts contributions anywhere along its path — including
    /// a middle/far run well away from both the board and the first-run
    /// placement — and rejects them away from the path entirely. Completing
    /// the order broadcasts a structure that carries the full path (#96).
    #[tokio::test]
    async fn road_contributions_work_along_the_path_and_completion_carries_it() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let mut editor_ws = dial_editor(&proxy, &db).await;

        // 50m east then 100m south = 150m -> 37 stone (deliberately under the
        // 50-item carry cap so one hauler can finish it in a single trip).
        editor_ws
            .send(Message::Text(
                json!({"type": "road.plan", "points": [[12800, 12800], [12850, 12800], [12850, 12900]]}).to_string(),
            ))
            .await
            .unwrap();
        let order_id = recv_until(&mut editor_ws, "road.planned").await["order_id"].as_str().unwrap().to_string();

        let email = format!("hauler_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Hauler"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();

        // Stone in the pockets (as mining would put it).
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "gather_yield", "player_id": pid,
                "item_id": "stone", "qty": 40, "skill": "gathering", "xp": 1,
            }).to_string()))
            .unwrap();
        recv_until(&mut ws, "inv.update").await;

        // Far from the board, far from run 1, far from every run: rejected.
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: 12850, y: 13250, hp: 100, gather: None });
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "build_contribute", "player_id": pid,
                "order_id": order_id, "item_id": "stone", "qty": 1,
            }).to_string()))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let order = db.build_order_by_id(&order_id).await.unwrap().unwrap();
        assert_eq!(order.progress_json, "{}", "far from the whole path: contribution refused");

        // Near the SECOND run's far end (~90m from the placement segment,
        // ~100m from the civic board): accepted.
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: 12855, y: 12890, hp: 100, gather: None });
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "build_contribute", "player_id": pid,
                "order_id": order_id, "item_id": "stone", "qty": 1,
            }).to_string()))
            .unwrap();
        let progress = recv_until(&mut ws, "build.progress").await;
        assert_eq!(progress["progress"]["stone"].as_i64(), Some(1), "mid-path contribution accepted");

        // Pour in the rest: completion broadcasts a structure with the path.
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "build_contribute", "player_id": pid,
                "order_id": order_id, "item_id": "stone", "qty": 36,
            }).to_string()))
            .unwrap();
        recv_until(&mut ws, "build.completed").await;
        let structure = loop {
            let v = recv_until(&mut ws, "status_update").await;
            if v["state"]["type"] == "structure" && v["state"]["kind"] == "dirt_road" {
                break v;
            }
        };
        assert_eq!(
            structure["state"]["path"],
            json!([[12800, 12800], [12850, 12800], [12850, 12900]]),
            "the built road carries its full multi-run path"
        );

        drop(editor_ws);
        drop(ws);
    }

    /// `build.list` resolves the board from the player's cached POSITION —
    /// the #94 quirk: a zone's region centre only identifies the district
    /// when every district has its own shard.
    #[tokio::test]
    async fn build_list_board_follows_the_players_position() {
        let (proxy, db, _dbf, _zone) = proxy_with_shared_db().await;
        db.insert_build_order("suburbs", "test_hut", r#"{"wood":5}"#, "open", 0, None, 0, None, None)
            .await
            .unwrap();
        db.insert_build_order("civic", "test_fountain", r#"{"stone":5}"#, "open", 0, None, 0, None, None)
            .await
            .unwrap();

        let email = format!("walker_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Walker"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();

        // Standing at the town centre (the welcome relocate cached it): civic.
        ws.send(Message::Text(json!({"type": "build.list"}).to_string())).await.unwrap();
        let board = recv_until(&mut ws, "build.list").await;
        let kinds: Vec<&str> = board["orders"].as_array().unwrap().iter().filter_map(|o| o["kind"].as_str()).collect();
        assert!(kinds.contains(&"test_fountain"), "town centre = the civic board (got {kinds:?})");

        // Walk (cache-wise) into the suburbs: the board follows the player,
        // not the zone's region centre.
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: 600, y: 600, hp: 100, gather: None });
        ws.send(Message::Text(json!({"type": "build.list"}).to_string())).await.unwrap();
        let board = recv_until(&mut ws, "build.list").await;
        let kinds: Vec<&str> = board["orders"].as_array().unwrap().iter().filter_map(|o| o["kind"].as_str()).collect();
        assert!(kinds.contains(&"test_hut") && !kinds.contains(&"test_fountain"),
            "suburbs position = the suburbs board (got {kinds:?})");

        drop(ws);
    }

    /// Free-angle roads (#111): a diagonal plan is accepted and priced by
    /// its Euclidean length; the cap applies to that length too.
    #[tokio::test]
    async fn diagonal_road_plans_price_by_euclidean_length() {
        let (proxy, db, _dbf, _zone) = proxy_with_shared_db().await;
        let mut editor_ws = dial_editor(&proxy, &db).await;
        // A 3-4-5 triangle hypotenuse: 300m east, 400m south of it = 500m.
        editor_ws
            .send(Message::Text(
                json!({"type": "road.plan", "points": [[13000, 12500], [13300, 12900]]}).to_string(),
            ))
            .await
            .unwrap();
        let order_id = recv_until(&mut editor_ws, "road.planned").await["order_id"].as_str().unwrap().to_string();
        let order = db.build_order_by_id(&order_id).await.unwrap().unwrap();
        assert_eq!(order.required_json, r#"{"stone":125}"#, "500m Euclidean -> 125 stone");
        drop(editor_ws);
    }

    /// `road.replan` (#104): re-routes an open plan (path + recomputed cost,
    /// progress kept), rejects everything it should, and completes on the
    /// spot when kept progress covers the recomputed cost.
    #[tokio::test]
    async fn road_replan_moves_open_plans_and_completes_when_covered() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let mut editor_ws = dial_editor(&proxy, &db).await;

        // A 200m plan (50 stone).
        editor_ws
            .send(Message::Text(
                json!({"type": "road.plan", "points": [[13000, 13000], [13200, 13000]]}).to_string(),
            ))
            .await
            .unwrap();
        let order_id = recv_until(&mut editor_ws, "road.planned").await["order_id"].as_str().unwrap().to_string();

        // Non-editor: rejected.
        let email = format!("meddler_{}@t.test", Uuid::new_v4().simple());
        let mut player_ws = dial(&proxy).await;
        player_ws
            .send(Message::Text(
                json!({"type": "register", "email": email, "password": "pw12", "name": "Meddler"}).to_string(),
            ))
            .await
            .unwrap();
        let pid = recv_until(&mut player_ws, "welcome").await["player_id"].as_str().unwrap().to_string();
        player_ws
            .send(Message::Text(
                json!({"type": "road.replan", "order_id": order_id, "points": [[13000, 13000], [13100, 13000]]}).to_string(),
            ))
            .await
            .unwrap();
        let err = recv_until(&mut player_ws, "road.plan_error").await;
        assert!(err["message"].as_str().unwrap().contains("editor"));

        // A repeated point: rejected with the shared validation. (Diagonal
        // runs are legal since #111 — roads are splines now.)
        editor_ws
            .send(Message::Text(
                json!({"type": "road.replan", "order_id": order_id, "points": [[13000, 13000], [13000, 13000]]}).to_string(),
            ))
            .await
            .unwrap();
        let err = recv_until(&mut editor_ws, "road.plan_error").await;
        assert!(err["message"].as_str().unwrap().contains("degenerate"));

        // Contribute 10 of the 50 first — the move must carry it.
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "gather_yield", "player_id": pid,
                "item_id": "stone", "qty": 10, "skill": "gathering", "xp": 1,
            }).to_string()))
            .unwrap();
        recv_until(&mut player_ws, "inv.update").await;
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: 13050, y: 13000, hp: 100, gather: None });
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "build_contribute", "player_id": pid,
                "order_id": order_id, "item_id": "stone", "qty": 10,
            }).to_string()))
            .unwrap();
        recv_until(&mut player_ws, "build.progress").await;

        // Replan to a 300m L: cost recomputes to 75, progress 10 kept.
        editor_ws
            .send(Message::Text(
                json!({"type": "road.replan", "order_id": order_id, "points": [[13000, 13000], [13200, 13000], [13200, 13100]]}).to_string(),
            ))
            .await
            .unwrap();
        recv_until(&mut editor_ws, "road.planned").await;
        let moved = db.build_order_by_id(&order_id).await.unwrap().unwrap();
        assert_eq!(moved.state, "open");
        assert_eq!(moved.required_json, r#"{"stone":75}"#, "cost recomputed from the new length");
        assert_eq!(moved.progress_json, r#"{"stone":10}"#, "contributed progress kept");
        assert_eq!(
            moved.path_json.as_deref(),
            Some("[[13000,13000],[13200,13000],[13200,13100]]"),
            "path swapped"
        );

        // Replan down to a 36m stub (floor cost 9): progress 10 covers it —
        // the order completes on the spot through the ordinary flow.
        editor_ws
            .send(Message::Text(
                json!({"type": "road.replan", "order_id": order_id, "points": [[13000, 13000], [13036, 13000]]}).to_string(),
            ))
            .await
            .unwrap();
        recv_until(&mut editor_ws, "road.planned").await;
        let done = recv_until(&mut player_ws, "build.completed").await;
        assert_eq!(done["order_id"].as_str().unwrap(), order_id, "covered-by-progress replan completes");
        let structure = loop {
            let v = recv_until(&mut player_ws, "status_update").await;
            if v["state"]["type"] == "structure" && v["state"]["kind"] == "dirt_road" {
                break v;
            }
        };
        assert_eq!(structure["state"]["path"], json!([[13000, 13000], [13036, 13000]]));

        // A completed road can't be moved any more.
        editor_ws
            .send(Message::Text(
                json!({"type": "road.replan", "order_id": order_id, "points": [[13000, 13000], [13100, 13000]]}).to_string(),
            ))
            .await
            .unwrap();
        let err = recv_until(&mut editor_ws, "road.plan_error").await;
        assert!(err["message"].as_str().unwrap().contains("demolish"), "built roads move via demolition (got {})", err["message"]);

        drop(editor_ws);
        drop(player_ws);
    }

    /// `road.cancel` + `road.demolish` (#106): the full removal economy —
    /// pristine plans cancel free, anything with stone in it takes a
    /// demolition job that refunds the banked stone to the demolisher's
    /// storage on completion, and the built road's entity despawns.
    #[tokio::test]
    async fn road_cancel_and_demolition_refund_the_banked_stone() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let mut editor_ws = dial_editor(&proxy, &db).await;

        // A pristine 100m plan cancels outright.
        editor_ws
            .send(Message::Text(json!({"type": "road.plan", "points": [[13400, 12600], [13500, 12600]]}).to_string()))
            .await
            .unwrap();
        let pristine = recv_until(&mut editor_ws, "road.planned").await["order_id"].as_str().unwrap().to_string();
        // Demolishing a pristine plan is refused toward cancel...
        editor_ws
            .send(Message::Text(json!({"type": "road.demolish", "order_id": pristine}).to_string()))
            .await
            .unwrap();
        let err = recv_until(&mut editor_ws, "road.plan_error").await;
        assert!(err["message"].as_str().unwrap().contains("cancel"), "pristine demolish points at cancel");
        // ...and cancel removes it.
        editor_ws
            .send(Message::Text(json!({"type": "road.cancel", "order_id": pristine}).to_string()))
            .await
            .unwrap();
        recv_until(&mut editor_ws, "road.cancelled").await;
        assert!(db.build_order_by_id(&pristine).await.unwrap().is_none(), "cancelled plan row gone");

        // Build a 40m stub road (10 stone) end-to-end with a worker.
        editor_ws
            .send(Message::Text(json!({"type": "road.plan", "points": [[13400, 12600], [13440, 12600]]}).to_string()))
            .await
            .unwrap();
        let road_order = recv_until(&mut editor_ws, "road.planned").await["order_id"].as_str().unwrap().to_string();
        let email = format!("wrecker_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Wrecker"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "gather_yield", "player_id": pid,
                "item_id": "stone", "qty": 10, "skill": "gathering", "xp": 1,
            }).to_string()))
            .unwrap();
        recv_until(&mut ws, "inv.update").await;
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: 13420, y: 12600, hp: 100, gather: None });
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "build_contribute", "player_id": pid,
                "order_id": road_order, "item_id": "stone", "qty": 10,
            }).to_string()))
            .unwrap();
        recv_until(&mut ws, "build.completed").await;

        // A cancel on the built road is refused; demolish posts the job.
        editor_ws
            .send(Message::Text(json!({"type": "road.cancel", "order_id": road_order}).to_string()))
            .await
            .unwrap();
        let err = recv_until(&mut editor_ws, "road.plan_error").await;
        assert!(err["message"].as_str().unwrap().contains("demolish"));
        editor_ws
            .send(Message::Text(json!({"type": "road.demolish", "order_id": road_order}).to_string()))
            .await
            .unwrap();
        let posted = recv_until(&mut editor_ws, "road.demolition_planned").await;
        let demo_id = posted["demo_order_id"].as_str().unwrap().to_string();
        let demo = db.build_order_by_id(&demo_id).await.unwrap().unwrap();
        assert_eq!(demo.kind, format!("demo_{road_order}"));
        assert_eq!(demo.required_json, r#"{"tool_kit":1}"#);
        assert!(demo.placement().is_none(), "a demolition must never spawn a structure");
        assert!(demo.path_json.is_some(), "the demo order carries the path for on-site work");
        // Double-demolition guarded.
        editor_ws
            .send(Message::Text(json!({"type": "road.demolish", "order_id": road_order}).to_string()))
            .await
            .unwrap();
        let err = recv_until(&mut editor_ws, "road.plan_error").await;
        assert!(err["message"].as_str().unwrap().contains("already"));

        // The wrecker crafts up a tool kit (granted as a gather would) and
        // works the demolition on site.
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "gather_yield", "player_id": pid,
                "item_id": "tool_kit", "qty": 1, "skill": "gathering", "xp": 1,
            }).to_string()))
            .unwrap();
        recv_until(&mut ws, "inv.update").await;
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "build_contribute", "player_id": pid,
                "order_id": demo_id, "item_id": "tool_kit", "qty": 1,
            }).to_string()))
            .unwrap();

        // Wire order: the refund lands (store.update) before the despawn.
        let storage = recv_until(&mut ws, "store.update").await;
        let items = storage["items"].as_array().unwrap();
        assert!(
            items.iter().any(|it| it["item_id"] == "stone" && it["qty"].as_i64() == Some(10)),
            "the full banked stone refunds to the demolisher's storage (got {items:?})"
        );
        // The road's render entity despawns for connected clients...
        loop {
            let v = recv_until(&mut ws, "despawn").await;
            if v["player_id"].as_str().unwrap().starts_with("structure_road_") {
                break;
            }
        }
        // ...and the target order row is gone.
        assert!(db.build_order_by_id(&road_order).await.unwrap().is_none(), "demolished road order deleted");

        drop(editor_ws);
        drop(ws);
    }

    /// Demolishing a part-built plan refunds its contributed progress (not
    /// the full cost), and posting the demolition freezes contributions.
    #[tokio::test]
    async fn demolishing_a_part_built_plan_refunds_its_progress() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let mut editor_ws = dial_editor(&proxy, &db).await;

        // 200m plan = 50 stone; contribute 12.
        editor_ws
            .send(Message::Text(json!({"type": "road.plan", "points": [[13400, 12500], [13600, 12500]]}).to_string()))
            .await
            .unwrap();
        let road_order = recv_until(&mut editor_ws, "road.planned").await["order_id"].as_str().unwrap().to_string();
        let email = format!("hauler2_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Hauler2"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "gather_yield", "player_id": pid,
                "item_id": "stone", "qty": 13, "skill": "gathering", "xp": 1,
            }).to_string()))
            .unwrap();
        recv_until(&mut ws, "inv.update").await;
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: 13500, y: 12500, hp: 100, gather: None });
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "build_contribute", "player_id": pid,
                "order_id": road_order, "item_id": "stone", "qty": 12,
            }).to_string()))
            .unwrap();
        recv_until(&mut ws, "build.progress").await;

        editor_ws
            .send(Message::Text(json!({"type": "road.demolish", "order_id": road_order}).to_string()))
            .await
            .unwrap();
        let posted = recv_until(&mut editor_ws, "road.demolition_planned").await;
        let demo_id = posted["demo_order_id"].as_str().unwrap().to_string();
        // The frozen plan takes no more stone.
        assert_eq!(db.build_order_by_id(&road_order).await.unwrap().unwrap().state, "demolishing");
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "build_contribute", "player_id": pid,
                "order_id": road_order, "item_id": "stone", "qty": 1,
            }).to_string()))
            .unwrap();

        // Work the demolition; the refund is the 12 contributed, not 50.
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "gather_yield", "player_id": pid,
                "item_id": "tool_kit", "qty": 1, "skill": "gathering", "xp": 1,
            }).to_string()))
            .unwrap();
        recv_until(&mut ws, "inv.update").await;
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "build_contribute", "player_id": pid,
                "order_id": demo_id, "item_id": "tool_kit", "qty": 1,
            }).to_string()))
            .unwrap();
        // Wire order: refund first, then the completion announcements.
        let storage = recv_until(&mut ws, "store.update").await;
        let items = storage["items"].as_array().unwrap();
        assert!(
            items.iter().any(|it| it["item_id"] == "stone" && it["qty"].as_i64() == Some(12)),
            "a part-built plan refunds its contributed progress (got {items:?})"
        );
        recv_until(&mut ws, "build.completed").await;
        assert!(db.build_order_by_id(&road_order).await.unwrap().is_none());

        drop(editor_ws);
        drop(ws);
    }

    /// The env tick's `poison_sources` counts poison trees within
    /// POISON_RADIUS_M of the player, from the object cache (#85): zero far
    /// away, exactly the in-radius count in a grove, back to zero after the
    /// trees are deleted.
    #[tokio::test]
    async fn env_tick_counts_poison_trees_in_radius() {
        let (proxy, db, _dbf, mut zone) = proxy_with_shared_db().await;

        // Seed a grove BEFORE the cache's first touch: two trees inside the
        // radius of (2000, 2000), one just outside it.
        let near_a = db.insert_world_object("poison_tree", 2005, 2000, "editor:t", 0).await.unwrap();
        let _near_b = db.insert_world_object("poison_tree", 2000, 2010, "editor:t", 0).await.unwrap();
        let _far = db
            .insert_world_object("poison_tree", 2000 + POISON_RADIUS_M as i32 + 5, 2000, "editor:t", 0)
            .await
            .unwrap();

        let email = format!("forager_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Forager"}).to_string(),
        ))
        .await
        .unwrap();
        let welcome = recv_until(&mut ws, "welcome").await;
        let pid = welcome["player_id"].as_str().unwrap().to_string();

        // At spawn (town centre, nowhere near the grove): clean.
        proxy.env_tick_once().await;
        let flags = recv_env_state(&mut zone, &pid).await;
        assert_eq!(flags["poison_sources"], 0);

        // In the grove: exactly the two in-radius trees count.
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: 2000, y: 2000, hp: 100, gather: None });
        proxy.env_tick_once().await;
        let flags = recv_env_state(&mut zone, &pid).await;
        assert_eq!(flags["poison_sources"], 2, "two trees in radius, the third is just outside");

        // Deleting a tree (the editor's delete path keeps the cache
        // write-through) takes effect on the next pass.
        assert!(db.delete_world_object(&near_a.id).await.unwrap());
        proxy.world_object_cache().await.lock().unwrap().remove(&near_a.id);
        proxy.env_tick_once().await;
        let flags = recv_env_state(&mut zone, &pid).await;
        assert_eq!(flags["poison_sources"], 1);

        drop(ws);
    }
}
