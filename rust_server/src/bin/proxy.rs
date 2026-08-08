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
/// Progressive road building (#131, issue #132): a plan's path is chopped
/// into chunks this long (by arc length) at plan/replan time, each priced
/// and built independently — see `cut_road_cells`.
const ROAD_CELL_LEN_M: f64 = 5.0;

// The market's tunables — range, warehouse slots, rate limit, page size, order
// bounds and every fee rate — moved to the repo-root `market.toml` in #152 (see
// `mmo::market_config`). They are reached through `self.market_cfg(district)`,
// never as consts, so no code path can charge one number while the panel
// previews another.
//
// The two INTERVALS below stay consts on purpose: they're operational cadence,
// not economy tuning. Nothing a player sees depends on them, an operator has no
// reason to tune them per district, and making them per-market would be
// meaningless for jobs that sweep every market at once.

/// A market the caller is standing at: its id, and the **district** that owns
/// it. The district travels alongside because market tuning is keyed on it
/// (#152) — a market's own id is a `Uuid::new_v4()` and so can't be named in a
/// config file, while districts are authored and stable.
struct MarketAt {
    id: String,
    district: String,
    x: i64,
    y: i64,
}

/// Where market tuning is read from, unless overridden by `MARKET_CONFIG`.
///
/// Resolved at COMPILE TIME against this crate's manifest directory, not the
/// process's cwd — exactly as `world::DEFAULT_TERRAIN_DIR` does, and for the
/// same reason: the cwd varies. `start_servers.ps1` launches the proxy with its
/// working directory set to `rust_server/`, while a workspace-wide
/// `cargo run -p proxy` runs from the repo root. A cwd-relative path would have
/// meant the dev server silently never finding the file while its boot log
/// claimed it had loaded one — the precise quiet failure this config's
/// strictness exists to prevent.
const DEFAULT_MARKET_CONFIG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../market.toml");
/// Where interior zone geometry is read from (#165). Resolved against the
/// manifest dir for the same reason as the market config: the proxy runs with
/// its cwd set to `rust_server/`, so a cwd-relative path would silently miss.
const DEFAULT_ZONE_CONFIG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../zones.toml");
const DEFAULT_CRAFTING_CONFIG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../crafting.toml");

/// Load the repo-root `market.toml` (#152), or **refuse to start**.
///
/// A MISSING file is fine and resolves to the values #136-#143 shipped — config
/// is an override mechanism, not a required input, and a fresh clone must run
/// the same economy.
///
/// A PRESENT but broken file is fatal. The operator wrote it expecting it to
/// take effect; booting with silently-substituted defaults would run an economy
/// nobody chose, and by the time anyone noticed the trades would already have
/// happened. `panic!` here is the same reasoning as `book_health`'s below: a
/// market that won't start beats a market quietly doing the wrong thing.
/// Load the repo-root `zones.toml` (#165), or refuse to start.
///
/// Same contract as `load_market_config`: a MISSING file is fine and means "no
/// interiors", which is exactly the world before this issue. A PRESENT but
/// broken one is fatal — a layout that would strand players is not something to
/// discover by watching one fall through the floor.
/// Deposit/station/recipe tuning (#166). Missing means none; malformed is fatal.
fn load_crafting_config() -> mmo::crafting_config::CraftingConfig {
    let path = std::env::var("CRAFTING_CONFIG")
        .unwrap_or_else(|_| DEFAULT_CRAFTING_CONFIG.to_string());
    match mmo::crafting_config::CraftingConfig::load(std::path::Path::new(&path)) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("[Proxy] FATAL: {e}");
            panic!("crafting config is unusable — refusing to start");
        }
    }
}

/// `tutorial.toml`, on the same strict contract as the others: a missing file
/// means no tutorial and no handouts — a playable world, since nothing is gated
/// behind the track — and a malformed one refuses the boot naming the step.
fn load_tutorial_config() -> mmo::tutorial_config::TutorialConfig {
    let path = format!("{}/../tutorial.toml", env!("CARGO_MANIFEST_DIR"));
    match mmo::tutorial_config::TutorialConfig::load(std::path::Path::new(&path)) {
        Ok(cfg) => {
            if cfg.steps.is_empty() && cfg.handouts.is_empty() {
                println!("[Proxy] tutorial.toml ABSENT — no track, no handouts");
            } else {
                println!(
                    "[Proxy] tutorial.toml: {} step(s), {} handout(s)",
                    cfg.steps.len(),
                    cfg.handouts.len()
                );
            }
            cfg
        }
        // Refusing to boot is correct here. A handout condition that failed
        // silently would either strand every new player or print pickaxes.
        Err(e) => panic!("{e}"),
    }
}

fn load_zone_config() -> mmo::zone_config::ZoneConfig {
    let path = std::env::var("ZONE_CONFIG").unwrap_or_else(|_| DEFAULT_ZONE_CONFIG.to_string());
    match mmo::zone_config::ZoneConfig::load(std::path::Path::new(&path)) {
        Ok(cfg) => {
            for (id, z) in &cfg.interior {
                println!(
                    "[Proxy] Interior zone `{id}` ({}) — {} volume(s), {} portal(s)",
                    z.display_name,
                    z.volumes.len(),
                    z.portals.len()
                );
            }
            cfg
        }
        Err(e) => {
            eprintln!("[Proxy] FATAL: {e}");
            panic!("zone config is unusable — refusing to start");
        }
    }
}

fn load_market_config() -> mmo::market_config::MarketConfigSet {
    let path =
        std::env::var("MARKET_CONFIG").unwrap_or_else(|_| DEFAULT_MARKET_CONFIG.to_string());
    match mmo::market_config::MarketConfigSet::load(std::path::Path::new(&path)) {
        Ok(set) => {
            let d = set.defaults();
            let whence = if std::path::Path::new(&path).exists() {
                path.clone()
            } else {
                // Never let "loaded" stand in for "absent". A missing file is
                // legitimate (it means the shipped defaults), but an operator
                // who thinks they edited a live file must be able to see that
                // the server never read one.
                format!("{path} (ABSENT — using shipped defaults)")
            };
            println!(
                "[Proxy] Market config from {whence} — listing fee {}/{} (min {}g),                  sale tax {}/{}, {} slots, range {}",
                d.listing_fee_num, d.listing_fee_den, d.listing_fee_min_gold,
                d.sale_tax_num, d.sale_tax_den, d.warehouse_slots, d.range,
            );
            // Log what actually resolved, per district — an operator should be
            // able to confirm the override landed without reading the file back.
            for (district, cfg) in set.overrides() {
                println!(
                    "[Proxy]   district {district}: listing fee {}/{} (min {}g), sale tax {}/{}, {} slots",
                    cfg.listing_fee_num, cfg.listing_fee_den, cfg.listing_fee_min_gold,
                    cfg.sale_tax_num, cfg.sale_tax_den, cfg.warehouse_slots,
                );
            }
            set
        }
        Err(e) => {
            eprintln!("[Proxy] FATAL: {e}");
            panic!("market config is unusable — refusing to start");
        }
    }
}

/// How often expired resting orders are swept and their escrow released.
const ORDER_EXPIRY_INTERVAL: Duration = Duration::from_secs(60);
/// How often the trade ledger is rolled into candles (#143). A background
/// cadence — aggregation must never sit in front of a trade.
const CANDLE_ROLLUP_INTERVAL: Duration = Duration::from_secs(120);
/// How often storage billing WAKES (#155). Whether anyone is actually charged
/// is decided by `last_charged_at`, not by this — a job that relied on its own
/// cadence for correctness would double-bill after every restart.
const STORAGE_BILLING_INTERVAL: Duration = Duration::from_secs(600);
/// How often the NPC provisioner re-posts its standing bid and ask (#154).
/// Frequent enough that a swept-out floor comes back quickly, slow enough that
/// it isn't rewriting the book under active traders.
const PROVISIONER_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Build wages (#145): gold the city pays per UNIT actually contributed to a
/// city build order — the game's first gold faucet. Before this, gold was
/// minted once at character creation (500, migration 0006) and only ever
/// drained by rent, so every purse trended to zero.
///
/// Deliberately tied to commissioned work: gold enters the economy only as
/// fast as the city posts build orders, which makes whoever plans them the
/// de facto central bank. For scale at 1/unit: a town well (30 units) pays
/// 30, a 300m road (75 stone) pays 75, against `RENT_COST_GOLD` of 50.
/// A real balance pass owns the number (see #129); this is a starting point.
const BUILD_WAGE_GOLD_PER_UNIT: i64 = 1;

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
    /// The slice of the world this zone owns. Meaningless for an interior,
    /// which is why interiors are excluded from every geometry decision — see
    /// `Zone::interior`.
    region: Region,
    /// An INTERIOR zone (#165): its coordinates are its own, not the world's.
    ///
    /// Interiors are invisible to `zone_at`, never split, never merged, and
    /// never handed a share of the world — so the only way in or out is an
    /// explicit portal. That is deliberate: no amount of walking, dying or
    /// region reshuffling can drop somebody into the mine by accident.
    interior: bool,
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
/// `status_update`s — position/hp for recreating it elsewhere on a
/// split/merge/rolling-update (#16). Used to carry an in-progress gather
/// job too, back when gathering was a channel a player could walk away
/// from mid-yield; every resource is an instant ability swing now (#125),
/// so there's nothing left to carry across a handoff.
#[derive(Clone)]
struct EntityCache {
    x: i32,
    y: i32,
    hp: i32,
    /// Which zone that position is IN (#165).
    ///
    /// A position without a zone is meaningless once interiors exist: two
    /// players at (100, 100) in different zones are nowhere near each other,
    /// and every proximity gate reading this cache — the market, the bounty,
    /// the environment pass — would otherwise happily treat them as co-located.
    /// Carried alongside the coordinates so a reader cannot forget it.
    zone: String,
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
    /// The zone this character was last saved in (#165).
    ///
    /// `flush_once` has always written `ClientInfo::current_zone` into the
    /// `character.district` column — it just was never read back, because
    /// login could always re-derive the zone from the position. That stops
    /// being true the moment interiors exist: an interior position means
    /// nothing on the surface map, so the zone has to be remembered rather
    /// than recomputed.
    saved_zone: String,
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
    /// keyed by player_id. Used to recreate entities at their real position
    /// in a freshly-spawned zone instance during a split/merge/rolling update.
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
    /// Market tuning from the repo-root `market.toml` (#152), resolved per
    /// district. Loaded once at boot — there is no hot reload, so a rate can't
    /// change underneath an order that's mid-flight.
    market_cfg: mmo::market_config::MarketConfigSet,
    /// Authored interior zones (#165), loaded once at boot from `zones.toml`.
    /// The zone processes load the same file for their own geometry — one file,
    /// no wire format to keep in step.
    zone_cfg: mmo::zone_config::ZoneConfig,
    /// Deposit and skill tuning (#166), shared with the zone processes — the
    /// gateway needs it because it enforces the swing cooldown and knows skill
    /// levels, neither of which a zone can see.
    crafting_cfg: mmo::crafting_config::CraftingConfig,
    tutorial_cfg: mmo::tutorial_config::TutorialConfig,
    /// Items some `gained` condition watches, precomputed at boot so the
    /// gather path is a set lookup rather than a config walk per swing.
    tutorial_counted: std::collections::BTreeSet<String>,
    tutorial_made: std::collections::BTreeSet<String>,
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
    /// Per-(character, ability) last-use instant (mining/abilities epic
    /// #123, #117) — server-authoritative cooldown enforcement. In-memory
    /// only: a gateway restart clears everyone's cooldowns, same as any
    /// other session-scoped guard (e.g. `cooldowns` above, for zone splits).
    ability_cooldowns: Mutex<HashMap<(String, String), Instant>>,
    /// Per-character market command timestamps in the last minute (#140), for
    /// the rate limit. In-memory and session-scoped like the cooldowns above:
    /// a restart forgiving everyone's rate history is harmless.
    market_rate: Mutex<HashMap<String, Vec<Instant>>>,
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

/// Chop a validated road path into `ROAD_CELL_LEN_M` chunks by arc length
/// (progressive road building epic #131, issue #132) and price each chunk
/// as a share of `total_stone` proportional to its own length, remainder
/// folded into the last cell so the parts always sum to exactly
/// `total_stone`. Pricing the WHOLE road first and only then splitting it —
/// rather than re-applying `ROAD_MIN_STONE` per cell — keeps a road's total
/// cost identical to the pre-#131 pooled model; flooring per 5m cell
/// instead would have made a 50m road roughly 4x pricier for no reason
/// (10 cells x a 5-stone floor vs. today's ~12-stone total).
///
/// A cell may cross an original waypoint corner — the cut runs on arc
/// length, not on the plan's turns — so a cell's `(x0,y0)-(x1,y1)` is a
/// straight chord approximating that stretch, good enough for the
/// proximity check #133 does against it (every other proximity gate in
/// this file already tolerates the same corner-cutting approximation).
fn cut_road_cells(points: &[(i64, i64)], total_stone: i64) -> Vec<mmo::persistence::RoadCellSpec> {
    struct Cut { x: f64, y: f64, len: f64 }
    let mut cuts = vec![Cut { x: points[0].0 as f64, y: points[0].1 as f64, len: 0.0 }];
    let mut carried = 0.0f64;
    let mut total_len = 0.0f64;
    for w in points.windows(2) {
        let (x0, y0) = (w[0].0 as f64, w[0].1 as f64);
        let (x1, y1) = (w[1].0 as f64, w[1].1 as f64);
        let seg_len = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt();
        if seg_len <= 0.0 {
            continue;
        }
        let (dx, dy) = ((x1 - x0) / seg_len, (y1 - y0) / seg_len);
        let mut walked = 0.0f64;
        while carried + (seg_len - walked) >= ROAD_CELL_LEN_M {
            walked += ROAD_CELL_LEN_M - carried;
            carried = 0.0;
            total_len += ROAD_CELL_LEN_M;
            cuts.push(Cut { x: x0 + dx * walked, y: y0 + dy * walked, len: total_len });
        }
        carried += seg_len - walked;
        total_len += seg_len - walked;
    }
    let last = points[points.len() - 1];
    if cuts.last().is_some_and(|c| (c.x.round() as i64, c.y.round() as i64) != last) {
        cuts.push(Cut { x: last.0 as f64, y: last.1 as f64, len: total_len });
    }

    let mut cells = Vec::with_capacity(cuts.len().saturating_sub(1));
    let mut spent = 0i64;
    for w in cuts.windows(2) {
        let cell_len = w[1].len - w[0].len;
        let share = if total_len > 0.0 {
            ((total_stone as f64) * cell_len / total_len).round() as i64
        } else {
            0
        };
        spent += share;
        cells.push(mmo::persistence::RoadCellSpec {
            x0: w[0].x.round() as i64,
            y0: w[0].y.round() as i64,
            x1: w[1].x.round() as i64,
            y1: w[1].y.round() as i64,
            required_json: json!({ "stone": share.max(0) }).to_string(),
        });
    }
    // Fold the rounding remainder into the last cell so the parts always
    // sum to exactly `total_stone`; defensively floor at 1 so a cell never
    // prices at (or below) zero, which would trivially auto-complete it.
    if let Some(last_cell) = cells.last_mut() {
        let remainder = total_stone - spent;
        let current = last_cell.required_json.as_str();
        let bumped = (parse_road_cell_stone(current) + remainder).max(1);
        last_cell.required_json = json!({ "stone": bumped }).to_string();
    }
    cells
}

/// Pull the `stone` field back out of a `required_json` blob built by
/// [`cut_road_cells`] itself — a tiny local helper rather than pulling in
/// `persistence`'s `parse_cost` for one field, since this stays entirely
/// inside `cut_road_cells`'s own bookkeeping.
fn parse_road_cell_stone(required_json: &str) -> i64 {
    serde_json::from_str::<Value>(required_json)
        .ok()
        .and_then(|v| v.get("stone").and_then(|s| s.as_i64()))
        .unwrap_or(0)
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
    /// Construct with the SHIPPED market defaults. Test-only since #152 —
    /// production goes through `new_with_market_config` with the set `main`
    /// validated at startup, which is also why tests never touch the repo's real
    /// `market.toml`: a suite whose expected fees moved when someone tuned a
    /// live tuning file would be worse than no suite.
    #[cfg(test)]
    fn new(
        host: &str,
        port: u16,
        registration_port: u16,
        admin_port: u16,
        db: Option<Arc<Db>>,
    ) -> Arc<Self> {
        Self::new_with_market_config(
            host,
            port,
            registration_port,
            admin_port,
            db,
            mmo::market_config::MarketConfigSet::default(),
        )
    }

    /// The market config as an explicit argument (#152), so a gateway test can
    /// prove a `[districts.<id>]` override is actually CHARGED rather than
    /// merely reported. Also keeps file I/O out of the common constructor.
    fn new_with_market_config(
        host: &str,
        port: u16,
        registration_port: u16,
        admin_port: u16,
        db: Option<Arc<Db>>,
        market_cfg: mmo::market_config::MarketConfigSet,
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
            market_cfg,
            zone_cfg: load_zone_config(),
            crafting_cfg: load_crafting_config(),
            tutorial_cfg: load_tutorial_config(),
            tutorial_counted: load_tutorial_config().counted_items(),
            tutorial_made: load_tutorial_config().made_items(),
            rent_reclaim_log: Mutex::new(VecDeque::new()),
            db_write_latencies_ms: Mutex::new(VecDeque::new()),
            terrain_edit_lock: tokio::sync::Mutex::new(()),
            world_objects: tokio::sync::OnceCell::new(),
            ability_cooldowns: Mutex::new(HashMap::new()),
            market_rate: Mutex::new(HashMap::new()),
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
        // A zone registering under an authored interior's id IS that interior
        // (#165). Both processes read the same `zones.toml`, so there is no
        // handshake to get wrong and no way for the two to disagree.
        let interior = self.zone_cfg.is_interior(&zone_id);

        {
            let mut zones = self.zones.lock().unwrap();
            zones.insert(
                zone_id.clone(),
                Zone {
                    interior,
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
        self.sync_deposit_state_to_zone(&zone_id).await;
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
                            self.entity_state.lock().unwrap().insert(pid.to_string(), EntityCache { x, y, hp, zone: zone_id.to_string() });
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
                Some("deposit_depleted") => {
                    // Durable half of a seam being worked out (#166). The zone
                    // owns the live charges; this is the one fact that can't be
                    // recomputed from config on restart.
                    if let (Some(db), Some(node)) = (
                        self.db.clone(),
                        data.get("node_id").and_then(|v| v.as_str()),
                    ) {
                        let _ = db.mark_deposit_depleted(node, now_secs()).await;
                    }
                }
                Some("deposit_respawned") => {
                    if let (Some(db), Some(node)) = (
                        self.db.clone(),
                        data.get("node_id").and_then(|v| v.as_str()),
                    ) {
                        let _ = db.clear_deposit_depleted(node).await;
                    }
                }
                Some("weapon_used") => {
                    // Internal: a zone reported a swing that CONNECTED (#160).
                    // Durability lives here, so the wearing does too — the same
                    // split as a gathering swing, which the zone also reports
                    // rather than resolving.
                    if let Some(pid) = target_player.as_deref() {
                        self.wear_weapon(pid).await;
                    }
                }
                Some("gather_yield") => {
                    // Internal: a zone yielded a gathered unit. Persist it and push
                    // the authoritative inventory/skill to the client (not forwarded).
                    // `ability_id` (#128) is only present when this came from a real
                    // ability swing (`apply_ability_swing`) — test fixtures and any
                    // other internal caller that hands over starting items via this
                    // same message never wear down a tool, by design.
                    if let Some(pid) = target_player.as_deref() {
                        let item = data.get("item_id").and_then(|v| v.as_str()).unwrap_or("");
                        let qty = data.get("qty").and_then(|v| v.as_i64()).unwrap_or(0);
                        let skill = data.get("skill").and_then(|v| v.as_str()).unwrap_or("gathering");
                        let xp = data.get("xp").and_then(|v| v.as_i64()).unwrap_or(0);
                        let ability_id = data.get("ability_id").and_then(|v| v.as_str()).map(str::to_string);
                        // `source` (#159) distinguishes a kill's loot from a
                        // gather swing, so a drop that a full bag couldn't take
                        // can say so — a player who did the work and got nothing
                        // must never be left guessing.
                        let source = data.get("source").and_then(|v| v.as_str()).map(str::to_string);
                        self.apply_gather_yield(
                            pid, item, qty, skill, xp, ability_id.as_deref(), source.as_deref(),
                        )
                        .await;
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
                Some("npc_interact") => {
                    // Internal: a zone validated talk-range proximity. Whether the
                    // NPC hands over anything (mining/abilities epic #123, #118) is
                    // authoritative here — only the gateway knows inventory/equipment.
                    if let Some(pid) = target_player.as_deref() {
                        let npc_id = data.get("npc_id").and_then(|v| v.as_str()).unwrap_or("");
                        self.apply_npc_interact(pid, npc_id).await;
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
    /// `handle_player_died` (a respawn, which may also cross zones).
    fn relocate_player(&self, pid: &str, x: i32, y: i32, hp: i32, target: &str) {
        let target_tx = self.zones.lock().unwrap().get(target).map(|z| z.tx.clone());
        let Some(tx) = target_tx else { return };
        let msg = json!({"type": "spawn_entity", "player_id": pid, "x": x, "y": y, "hp": hp});
        let _ = tx.send(Message::Text(msg.to_string()));
        self.entity_state.lock().unwrap().insert(
            pid.to_string(),
            EntityCache { x, y, hp, zone: target.to_string() },
        );

        // Follow the player's client session (every entity is a client).
        let mut clients = self.clients.lock().unwrap();
        if let Some(info) = clients.get_mut(pid) {
            info.current_zone = target.to_string();
            let _ = info.tx.try_send(Message::Text(
                json!({"type": "zone_migration", "zone": target}).to_string(),
            ));
        }
    }

    /// Find which SURFACE zone owns world position (x, y).
    ///
    /// Interiors are deliberately invisible here (#165). Their coordinates are
    /// their own, so asking "who owns (100, 100)" of an interior is a category
    /// error — and excluding them is what guarantees an explicit portal is the
    /// only way in or out. Every caller of this is a geometry decision: a
    /// boundary crossing, a respawn, a login placement. None of them should be
    /// able to land somebody underground by accident.
    fn zone_at(&self, x: i32, y: i32) -> Option<String> {
        self.zones
            .lock()
            .unwrap()
            .iter()
            .find(|(_, z)| !z.interior && z.region.contains(x, y))
            .map(|(id, _)| id.clone())
    }

    /// Whether `zone_id` is a registered interior.
    fn zone_is_interior(&self, zone_id: &str) -> bool {
        self.zones.lock().unwrap().get(zone_id).map(|z| z.interior).unwrap_or(false)
    }

    /// The zone a player is currently in, per the position cache (#165).
    /// `None` for an untracked player.
    fn zone_of(&self, pid: &str) -> Option<String> {
        self.entity_state.lock().unwrap().get(pid).map(|c| c.zone.clone())
    }

    /// Whether `pid` is on the surface — the precondition for every gate that
    /// reasons about world geometry (markets, the bounty, weather, terrain).
    /// An untracked player counts as surface, matching the pre-#165 world.
    fn on_surface(&self, pid: &str) -> bool {
        match self.zone_of(pid) {
            Some(z) => !self.zone_is_interior(&z),
            None => true,
        }
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

        // 4. Recreate every entity in the new instance at its cached position.
        for p in &players {
            let cached = self.entity_state.lock().unwrap().get(p).cloned();
            let (x, y, hp) = match cached {
                Some(c) => (c.x, c.y, c.hp),
                None => (WORLD_SIZE / 2, WORLD_SIZE / 2, 100),
            };
            let msg = json!({"type": "spawn_entity", "player_id": p, "x": x, "y": y, "hp": hp});
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

            // Interiors are excluded from the whole auto-scaler (#165): they
            // own no slice of the world, so splitting one in half is
            // meaningless and merging one into a surface neighbour would hand
            // it world geometry it cannot represent. A crowded mine is a
            // capacity question for a later issue, not a partitioning one.
            let infos: Vec<(String, Region, usize)> = self
                .zones
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, z)| !z.interior)
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

        // Players currently in this zone, with cached world positions.
        let players: Vec<(String, i32, i32, i32)> = {
            let clients = self.clients.lock().unwrap();
            let state = self.entity_state.lock().unwrap();
            clients
                .values()
                .filter(|i| i.current_zone == zone_id)
                .map(|i| match state.get(&i.player_id).cloned() {
                    Some(c) => (i.player_id.clone(), c.x, c.y, c.hp),
                    None => (i.player_id.clone(), region.x0, region.y0, 100),
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
                    interior: false,
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
        for (pid, x, y, hp) in &players {
            if !give.contains(*x, *y) {
                continue;
            }
            let _ = old_tx.send(Message::Text(
                json!({"type": "player_leave", "player_id": pid}).to_string(),
            ));
            let msg = json!({"type": "spawn_entity", "player_id": pid, "x": x, "y": y, "hp": hp});
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

        // Players to move out of the retiring zone, with their world positions.
        let movers: Vec<(String, i32, i32, i32)> = {
            let clients = self.clients.lock().unwrap();
            let state = self.entity_state.lock().unwrap();
            clients
                .values()
                .filter(|i| i.current_zone == drop_id)
                .map(|i| match state.get(&i.player_id).cloned() {
                    Some(c) => (i.player_id.clone(), c.x, c.y, c.hp),
                    None => (i.player_id.clone(), union.x0, union.y0, 100),
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
        for (pid, x, y, hp) in &movers {
            let msg = json!({"type": "spawn_entity", "player_id": pid, "x": x, "y": y, "hp": hp});
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
    /// `inv.update`. Every row carries its own `id` now (#128) — needed to equip or
    /// repair a SPECIFIC tool instance once "the pickaxe" stops being unambiguous;
    /// `durability`/`max_durability` ride along only for actual tool instances.
    async fn send_inventory(&self, pid: &str) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        if let Ok(items) = db.inventory_for_character(pid).await {
            let used: i64 = items.iter().map(|it| it.qty).sum();
            let arr: Vec<Value> = items
                .iter()
                .map(|it| {
                    let mut v = json!({"id": it.id, "item_id": it.item_id, "qty": it.qty, "slot": it.slot});
                    if let Some(durability) = it.durability {
                        v["durability"] = json!(durability);
                        v["max_durability"] = json!(mmo::world::tool_max_durability(&it.item_id).unwrap_or(durability));
                    }
                    v
                })
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
    ///
    /// Tools are blocked from storage entirely (#128): the storehouse has no
    /// concept of "which instance" (it only ever moves by item_id+qty), so
    /// a worn tool deposited and withdrawn would silently come back at full
    /// durability — durability laundering. Not solving "storage for
    /// individually-tracked items" here; instance rows just aren't a fit
    /// for a stack-based stash.
    async fn apply_store_op(&self, pid: &str, op: &str, item_id: &str, qty: i64) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        if mmo::world::tool_max_durability(item_id).is_some() {
            return;
        }
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

    /// The BUILT market the player is standing at, as `(market_id, x, y)`, or
    /// `None` if there isn't one in range (market epic #136, issue #137).
    ///
    /// A market is an ordinary completed build order whose `structure_kind` is
    /// `market` — so "is there a market here" is a DB question, and the order's
    /// own id **is** the market id. Books, warehouses, and listings are all
    /// keyed by that id from day one: only the capital's market exists in v1,
    /// but per-market state is the whole point of the design (#136), and
    /// retrofitting a key later is worse than carrying one now.
    async fn market_at(&self, db: &Db, pid: &str) -> Option<MarketAt> {
        // Markets are surface fixtures. Without this, an interior player whose
        // local coordinates happened to fall near one would be trading from
        // underground (#165) — the cache holds a position, and a position
        // without a zone means nothing.
        if !self.on_surface(pid) {
            return None;
        }
        let (px, py) = self.entity_state.lock().unwrap().get(pid).map(|c| (c.x, c.y))?;
        let district = self.capital.district_at(px, py)?.id.to_string();
        let range = self.market_cfg.for_district(&district).range;
        let orders = db.build_orders_for_district(&district).await.ok()?;
        orders.iter().find_map(|o| {
            if o.state != "completed" || o.structure_kind.as_deref() != Some("market") {
                return None;
            }
            let (x, y) = (o.x?, o.y?);
            (dist2(px, py, x as i32, y as i32) <= (range as i64).pow(2)).then(|| MarketAt {
                id: o.id.clone(),
                district: district.clone(),
                x,
                y,
            })
        })
    }

    /// The tuning in force at `district` (#152). Every market command resolves
    /// its rates through here rather than a const, so a district override and
    /// the numbers the client was told in `market.opened` cannot disagree.
    fn market_cfg(&self, district: &str) -> &mmo::market_config::MarketConfig {
        self.market_cfg.for_district(district)
    }

    /// Apply `market.open` (#137): the client asking to trade at whatever
    /// market it's standing next to. Deliberately carries no market id — the
    /// server resolves it from the caller's live position, exactly like every
    /// other proximity-gated action here, so a client can't name a market it
    /// isn't at. Answered with `market.opened {market_id}`, or `market.error`
    /// when there's no built market in range.
    ///
    /// This is the market subsystem's first command, and it establishes the
    /// range gate every later one inherits (`market_at` returning `None` IS
    /// the refusal).
    async fn apply_market_open(&self, pid: &str) {
        let Some(db) = self.db.clone() else { return };
        match self.market_at(&db, pid).await {
            Some(at) => {
                let market_id = at.id;
                // The rules ride along (#152). They used to be compile-time
                // consts the client mirrored; now that they're per-district
                // data, a mirrored copy would be a LIE — the panel would
                // preview a 3% tax while the server charged whatever
                // `market.toml` said, and the player would find out by being
                // short-changed. Anything the client shows as a number the
                // server will charge has to come from here.
                self.push_to_player(pid, json!({
                    "type": "market.opened", "market_id": market_id,
                    "x": at.x, "y": at.y,
                    "district": at.district,
                    "rules": self.market_cfg(&at.district).wire_rules(),
                }));
                // Hydrate what you're holding here, so the panel is useful the
                // moment it opens rather than after a round trip (#138), plus
                // your own resting orders (#139).
                self.send_warehouse(pid, &market_id).await;
                self.send_own_orders(pid, &market_id).await;
                self.send_listings(pid, &market_id, &json!({})).await;
            }
            None => self.push_to_player(pid, json!({
                "type": "market.error", "code": "out_of_range",
                "detail": "stand at a built market to trade",
            })),
        }
    }

    /// Push this player's warehouse at `market_id` (#138). `available` and
    /// `locked` travel as distinct rows, not a merged total: locked stock is
    /// escrowed against an open order and can't be withdrawn, and a player
    /// staring at goods they can't take needs to see WHY.
    async fn send_warehouse(&self, pid: &str, market_id: &str) {
        let Some(db) = self.db.clone() else { return };
        let Ok(rows) = db.warehouse_for_character(market_id, pid).await else { return };
        // Resolve the district here rather than threading it through a dozen
        // call sites (several of them background sweeps that have only a market
        // id). One indexed lookup on a player-triggered UI push is cheap, and
        // it keeps "which slot count applies" in one place.
        let slots = match self.district_of_market(&db, market_id).await {
            Some(d) => self.market_cfg(&d).warehouse_slots,
            None => self.market_cfg.defaults().warehouse_slots,
        };
        let items: Vec<Value> = rows
            .iter()
            .map(|r| {
                let mut v = json!({
                    "id": r.id, "item_id": r.item_id, "qty": r.qty, "state": r.state,
                });
                if let Some(d) = r.durability {
                    v["durability"] = json!(d);
                    if let Some(max) = mmo::world::tool_max_durability(&r.item_id) {
                        v["max_durability"] = json!(max);
                    }
                }
                v
            })
            .collect();
        // Storage arrears (#155): 0 unless an operator has turned storage fees
        // on. Sent with the warehouse rather than as an error, because it is a
        // STATE of the warehouse — the panel should say why it's locked while
        // showing the goods that are still safely there, not just refuse.
        let arrears = db.warehouse_arrears(market_id, pid).await.unwrap_or(0);
        self.push_to_player(pid, json!({
            "type": "warehouse.state", "market_id": market_id,
            "items": items, "used": rows.len(), "slots": slots,
            "arrears": arrears,
        }));
    }

    /// Apply `warehouse.deposit` / `warehouse.withdraw` (#138). Both are gated
    /// through `market_at` — the same server-side range check `market.open`
    /// established (#137) — so a client can't stock a market it isn't standing
    /// at. Guests have no durable inventory, so they're a no-op.
    async fn apply_warehouse_op(&self, pid: &str, op: &str, item_id: &str, qty: i64) {
        let Some(db) = self.db.clone() else { return };
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
        let Some(at) = self.market_at(&db, pid).await else {
            self.push_to_player(pid, json!({
                "type": "market.error", "code": "out_of_range",
                "detail": "stand at a built market to use its warehouse",
            }));
            return;
        };
        let (market_id, slots) =
            (at.id, self.market_cfg(&at.district).warehouse_slots);
        // Storage arrears (#155). Settle first, so a player who has come back
        // with gold is unlocked by the act of using the warehouse rather than
        // having to wait for the next daily tick — being locked out is a nudge
        // to pay, not a sentence to serve.
        //
        // Only DEPOSIT and WITHDRAW are gated. Selling stays open on purpose: a
        // sell is how someone in arrears earns the gold to clear them, and a
        // lock that removed the only way out would trap a player with their
        // goods hostage — which is the outcome this whole design exists to
        // avoid. Withdrawing is blocked because that is taking goods out
        // without settling; selling pays the debt as a side effect.
        let owed = db
            .settle_warehouse_arrears(&market_id, pid, now_secs())
            .await
            .unwrap_or(0);
        if owed > 0 {
            self.push_to_player(pid, json!({
                "type": "market.error", "code": "storage_arrears",
                "detail": format!(
                    "you owe {owed}g in storage here — your goods are safe, but the warehouse                      is locked until it's paid (selling still works)"
                ),
            }));
            return;
        }
        let moved = match op {
            "deposit" => db.warehouse_deposit(&market_id, pid, item_id, qty, slots).await,
            "withdraw" => db.warehouse_withdraw(&market_id, pid, item_id, qty).await,
            _ => Ok(0),
        };
        match moved {
            Ok(0) if op == "deposit" => self.push_to_player(pid, json!({
                "type": "market.error", "code": "warehouse_full",
                "detail": "no room in your warehouse here",
            })),
            Ok(_) => {}
            Err(e) => {
                // Never fail silently: a player whose deposit died on a
                // transient DB error would otherwise see nothing at all and
                // assume the goods vanished.
                eprintln!("[Proxy] warehouse.{op}: {e}");
                self.push_to_player(pid, json!({
                    "type": "market.error", "code": "server_error",
                    "detail": "that didn't go through — try again",
                }));
                return;
            }
        }
        self.send_inventory(pid).await;
        self.send_warehouse(pid, &market_id).await;
    }

    /// A commodity's aggregated book as a wire message (#139). Levels only —
    /// individual order ownership is never broadcast, which keeps the message
    /// small and stops players reading each other's positions.
    async fn book_json(&self, market_id: &str, item_id: &str) -> Option<Value> {
        let db = self.db.clone()?;
        let asks = db.book_for(market_id, item_id, "sell").await.ok()?;
        let bids = db.book_for(market_id, item_id, "buy").await.ok()?;
        let level = |l: &mmo::persistence::BookLevel| json!({"price": l.unit_price, "qty": l.qty});
        Some(json!({
            "type": "market.book", "market_id": market_id, "item_id": item_id,
            "asks": asks.iter().map(level).collect::<Vec<_>>(),
            "bids": bids.iter().map(level).collect::<Vec<_>>(),
        }))
    }

    /// Answer one player's `market.book_request`.
    async fn send_book(&self, pid: &str, market_id: &str, item_id: &str) {
        if let Some(v) = self.book_json(market_id, item_id).await {
            self.push_to_player(pid, v);
        }
    }

    /// Push the changed book to everyone in the district. Deliberately not a
    /// subscription model yet: markets are per-district and depth messages are
    /// small, so broadcasting on change keeps every onlooker's book live with
    /// no extra machinery. If traffic ever matters, the design doc's
    /// `MarketSubscribe` is the upgrade path.
    async fn broadcast_book(&self, district: &str, market_id: &str, item_id: &str) {
        if let Some(v) = self.book_json(market_id, item_id).await {
            self.broadcast_to_district(district, v);
        }
    }

    /// Push this player's own resting orders at a market (#139).
    async fn send_own_orders(&self, pid: &str, market_id: &str) {
        let Some(db) = self.db.clone() else { return };
        let Ok(orders) = db.open_orders_for_character(market_id, pid).await else { return };
        let arr: Vec<Value> = orders
            .iter()
            .map(|o| json!({
                "order_id": o.id, "side": o.side, "item_id": o.item_id,
                "unit_price": o.unit_price, "qty_total": o.qty_total,
                "qty_remaining": o.qty_remaining,
            }))
            .collect();
        self.push_to_player(pid, json!({
            "type": "market.orders", "market_id": market_id, "orders": arr,
        }));
    }

    /// Push a market's listing board (#142), optionally filtered.
    async fn send_listings(&self, pid: &str, market_id: &str, data: &Value) {
        let Some(db) = self.db.clone() else { return };
        let item = data.get("item_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
        let min_dur = data.get("min_durability").and_then(|v| v.as_i64());
        let max_price = data.get("max_price").and_then(|v| v.as_i64());
        let page_limit = match self.district_of_market(&db, market_id).await {
            Some(d) => self.market_cfg(&d).listing_page_limit,
            None => self.market_cfg.defaults().listing_page_limit,
        };
        let Ok(rows) = db
            .listings_for_market(market_id, item, min_dur, max_price, page_limit)
            .await
        else {
            return;
        };
        let listings: Vec<Value> = rows
            .iter()
            .map(|l| {
                let mut v = json!({
                    "listing_id": l.id, "item_id": l.item_id, "ask_price": l.ask_price,
                    "mine": l.seller_id == pid, "expires_at": l.expires_at,
                });
                if let Some(d) = l.durability {
                    v["durability"] = json!(d);
                    if let Some(max) = mmo::world::tool_max_durability(&l.item_id) {
                        v["max_durability"] = json!(max);
                    }
                }
                v
            })
            .collect();
        self.push_to_player(pid, json!({
            "type": "listing.page", "market_id": market_id, "listings": listings,
        }));
    }

    /// Apply `listing.place` / `listing.buy` / `listing.cancel` (#142) — the
    /// board for unique items. Shares `market.open`'s range gate and the same
    /// rate limit as the order book.
    async fn apply_listing_op(&self, pid: &str, op: &str, data: &Value) {
        let Some(db) = self.db.clone() else { return };
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
        let reject = |code: &str, detail: &str| {
            self.push_to_player(pid, json!({
                "type": "market.error", "code": code, "detail": detail,
            }));
        };
        if !self.allow_market_command(pid) {
            let r = mmo::world::OrderReject::RateLimited;
            reject(r.code(), r.detail());
            return;
        }
        let Some(at) = self.market_at(&db, pid).await else {
            reject("out_of_range", "stand at a built market to trade");
            return;
        };
        let cfg = self.market_cfg(&at.district).clone();
        let market_id = at.id;
        let command_id = data.get("command_id").and_then(|v| v.as_str()).unwrap_or("");
        let now = now_secs();

        match op {
            "place" => {
                let wh_id = data.get("warehouse_item_id").and_then(|v| v.as_str()).unwrap_or("");
                let ask = data.get("ask_price").and_then(|v| v.as_i64()).unwrap_or(0);
                let hours = cfg.order_duration_hours(
                    data.get("duration_hours").and_then(|v| v.as_i64()).unwrap_or(0),
                );
                if ask <= 0 {
                    let r = mmo::world::OrderReject::BadPrice;
                    reject(r.code(), r.detail());
                    return;
                }
                match db
                    .place_listing(&market_id, pid, wh_id, ask, now + hours * 3600, &cfg, command_id, now)
                    .await
                {
                    Ok(Some(l)) => {
                        self.push_to_player(pid, json!({
                            "type": "market.fees", "market_id": market_id,
                            "listing_fee": cfg.listing_fee(l.ask_price), "sale_tax": 0,
                        }));
                        self.push_gold(pid, 0, "listing_placed").await;
                        self.send_warehouse(pid, &market_id).await;
                        self.send_listings(pid, &market_id, &json!({})).await;
                    }
                    Ok(None) => reject(
                        "cannot_list",
                        "that has to be a unique item sitting in this market's warehouse, and you must afford the listing fee",
                    ),
                    Err(e) => {
                        eprintln!("[Proxy] listing.place: {e}");
                        reject("server_error", "that didn't go through — try again");
                    }
                }
            }
            "buy" => {
                let listing_id = data.get("listing_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let expected = data.get("expected_price").and_then(|v| v.as_i64()).unwrap_or(-1);
                // Read the seller BEFORE the buy: a successful purchase deletes
                // the listing, so afterwards there's nothing left to ask.
                let seller = db.listing_by_id(&listing_id).await.ok().flatten().map(|l| l.seller_id);
                match db
                    .buy_listing(pid, &listing_id, expected, &cfg, command_id, now)
                    .await
                {
                    Ok(Ok((l, tax))) => {
                        self.push_gold(pid, -l.ask_price, "listing_bought").await;
                        self.send_warehouse(pid, &market_id).await;
                        if let Some(seller) = seller {
                            self.push_gold(&seller, l.ask_price - tax, "listing_sold").await;
                            self.push_to_player(&seller, json!({
                                "type": "market.fees", "market_id": market_id,
                                "listing_fee": 0, "sale_tax": tax,
                            }));
                            self.send_warehouse(&seller, &market_id).await;
                            self.send_listings(&seller, &market_id, &json!({})).await;
                        }
                        self.send_listings(pid, &market_id, &json!({})).await;
                        // The board is a shared view, so everyone's copy is stale.
                        if let Some(d) = self.district_of_market(&db, &market_id).await {
                            self.broadcast_to_district(&d, json!({
                                "type": "listing.sold", "market_id": market_id,
                                "listing_id": l.id, "item_id": l.item_id, "ask_price": l.ask_price,
                            }));
                        }
                    }
                    Ok(Err(r)) => reject(r.code(), r.detail()),
                    Err(e) => {
                        eprintln!("[Proxy] listing.buy: {e}");
                        reject("server_error", "that didn't go through — try again");
                    }
                }
            }
            "cancel" => {
                let listing_id = data.get("listing_id").and_then(|v| v.as_str()).unwrap_or("");
                match db.cancel_listing(pid, listing_id).await {
                    Ok(Some(_)) => {
                        self.send_warehouse(pid, &market_id).await;
                        self.send_listings(pid, &market_id, &json!({})).await;
                    }
                    Ok(None) => reject("no_such_listing", "that listing isn't yours, or is already gone"),
                    Err(e) => eprintln!("[Proxy] listing.cancel: {e}"),
                }
            }
            _ => {}
        }
    }

    /// Apply `market.sell` / `market.buy` / `market.cancel` (#139). All three
    /// share `market.open`'s server-side range gate, and all three refuse with
    /// a typed `market.error` rather than a silent no-op, so a client can tell
    /// "rejected" from "nothing matched".
    async fn apply_market_order(&self, pid: &str, op: &str, data: &Value) {
        let Some(db) = self.db.clone() else { return };
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
        let reject = |code: &str, detail: &str| {
            self.push_to_player(pid, json!({
                "type": "market.error", "code": code, "detail": detail,
            }));
        };
        if !self.allow_market_command(pid) {
            let r = mmo::world::OrderReject::RateLimited;
            reject(r.code(), r.detail());
            return;
        }
        let Some(at) = self.market_at(&db, pid).await else {
            reject("out_of_range", "stand at a built market to trade");
            return;
        };
        // The district comes from `market_at`, which resolved it from the same
        // position that passed the range gate — so the rates charged here are
        // provably the ones this player was quoted in `market.opened` (#152).
        let mcfg = self.market_cfg(&at.district).clone();
        let (market_id, district) = (at.id, at.district);
        let command_id = data.get("command_id").and_then(|v| v.as_str()).unwrap_or("");
        let now = now_secs();

        if op == "cancel" {
            let order_id = data.get("order_id").and_then(|v| v.as_str()).unwrap_or("");
            match db.cancel_order(pid, order_id).await {
                Ok(Some(o)) => {
                    self.send_warehouse(pid, &market_id).await;
                    self.send_own_orders(pid, &market_id).await;
                    self.broadcast_book(&district, &market_id, &o.item_id).await;
                }
                Ok(None) => reject("no_such_order", "that order isn't yours, or is already gone"),
                Err(e) => eprintln!("[Proxy] market.cancel: {e}"),
            }
            return;
        }

        let item_id = data.get("item_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let unit_price = data.get("unit_price").and_then(|v| v.as_i64()).unwrap_or(0);
        let qty = data.get("qty").and_then(|v| v.as_i64()).unwrap_or(0);
        if let Err(r) = mcfg.validate_order(&item_id, unit_price, qty) {
            reject(r.code(), r.detail());
            return;
        }

        // Either side may now rest (#140), so both go through one path.
        let hours = mcfg.order_duration_hours(
            data.get("duration_hours").and_then(|v| v.as_i64()).unwrap_or(0),
        );
        let expires_at = now + hours * 3600;
        match db
            .place_order(
                &market_id, pid, op, &item_id, unit_price, qty, expires_at,
                &mcfg, command_id, now,
            )
            .await
        {
            // A resent command was already answered the first time — stay
            // silent rather than report a failure that didn't happen.
            Ok(out) if out.deduped => {}
            Ok(out) if out.fee_unaffordable => {
                let r = mmo::world::OrderReject::CannotAffordFee;
                reject(r.code(), r.detail());
            }
            Ok(out) if out.filled == 0 && out.resting_order_id.is_none() => {
                // Nothing traded and nothing rested: say which it was, since
                // "couldn't escrow" and "hit the cap" need different fixes.
                if op == "sell" {
                    reject("no_stock", "deposit that item into this market's warehouse before selling it")
                } else {
                    reject("no_funds", "not enough gold to escrow that order")
                }
            }
            Ok(out) => {
                // The placer's gold moved — ALWAYS push the balance, not just
                // on a fill: resting a buy escrows gold out of the purse, and
                // a client whose readout silently went stale would think it
                // still had money it doesn't. `delta` stays the trade result
                // (0 for a pure escrow), so escrow doesn't flash as a gain.
                let delta = out.earned + out.refunded - out.spent - out.listing_fee - out.sale_tax;
                self.push_gold(pid, delta, "market_trade").await;
                // Tell them what the house took (#141) — a fee you can't see
                // is a fee you'll assume is a bug.
                if out.listing_fee > 0 || out.sale_tax > 0 {
                    self.push_to_player(pid, json!({
                        "type": "market.fees", "market_id": market_id,
                        "listing_fee": out.listing_fee, "sale_tax": out.sale_tax,
                    }));
                }
                self.send_warehouse(pid, &market_id).await;
                self.send_own_orders(pid, &market_id).await;

                // Each counterparty learns their order shrank, gets paid or
                // receives goods, and sees their own escrow drop. Proceeds are
                // summed per seller so the delta is a real "+N gold", not a
                // bare balance.
                let mut proceeds: std::collections::BTreeMap<&str, i64> = Default::default();
                for t in &out.fills {
                    if t.seller_id != pid {
                        // NET of the sale tax that came out of this fill
                        // (#141) — showing the gross would be a lie about
                        // what landed in their purse.
                        *proceeds.entry(t.seller_id.as_str()).or_insert(0) +=
                            t.unit_price * t.qty - t.sale_tax_gold;
                    }
                }
                for (seller_id, earned) in &proceeds {
                    self.push_gold(seller_id, *earned, "market_sale").await;
                }
                for (_, owner, _) in &out.touched {
                    self.send_warehouse(owner, &market_id).await;
                    self.send_own_orders(owner, &market_id).await;
                }
                for t in &out.fills {
                    self.broadcast_to_district(&district, json!({
                        "type": "market.trade", "market_id": market_id,
                        "item_id": t.item_id, "unit_price": t.unit_price, "qty": t.qty,
                    }));
                }
                self.broadcast_book(&district, &market_id, &item_id).await;
            }
            Err(e) => {
                eprintln!("[Proxy] market.{op}: {e}");
                reject("server_error", "that order didn't go through — try again");
            }
        }
    }

    /// Per-player market command rate limit (#140): a sliding one-minute
    /// window. Order placement is the cheapest way to make the server do
    /// expensive work (a book sweep plus a write per fill), so it needs a
    /// ceiling that a human trading by hand will never reach.
    fn allow_market_command(&self, pid: &str) -> bool {
        let now = Instant::now();
        let mut hits = self.market_rate.lock().unwrap();
        let stamps = hits.entry(pid.to_string()).or_default();
        stamps.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
        if stamps.len() as i64 >= self.market_cfg.defaults().commands_per_minute {
            return false;
        }
        stamps.push(now);
        true
    }

    /// Charge daily warehouse storage at every built market (#155).
    ///
    /// **Does nothing on a stock server**: `storage_fee_per_slot_per_day` is 0
    /// by default, and `charge_storage` returns immediately. The job runs anyway
    /// so that turning the rate on in `market.toml` needs only a restart.
    ///
    /// The tick is far more frequent than a day because the DAY is enforced by
    /// `last_charged_at`, not by the sleep — a job that relied on its own
    /// cadence for correctness would double-bill after any restart.
    async fn storage_billing(self: Arc<Self>) {
        loop {
            sleep(STORAGE_BILLING_INTERVAL).await;
            let Some(db) = self.db.clone() else { continue };
            let now = now_secs();
            for d in &self.capital.districts {
                let cfg = self.market_cfg(d.id);
                if cfg.storage_fee_per_slot_per_day <= 0 {
                    continue;
                }
                let Ok(orders) = db.build_orders_for_district(d.id).await else { continue };
                for o in orders.iter().filter(|o| {
                    o.state == "completed" && o.structure_kind.as_deref() == Some("market")
                }) {
                    match db.charge_storage(&o.id, cfg, now).await {
                        Ok((0, 0)) => {}
                        Ok((charged, accrued)) => println!(
                            "[Proxy] MARKET: storage at {} — {charged}g burned, {accrued}g                              went unpaid into arrears",
                            d.id
                        ),
                        Err(e) => eprintln!("[Proxy] storage billing at {}: {e}", d.id),
                    }
                }
            }
        }
    }

    /// Keep the NPC provisioner's standing bid and ask posted at every built
    /// market (#154), and report trades that escaped the bounds.
    ///
    /// A background job like the expiry sweep and the candle rollup: the
    /// provisioner rests ORDINARY orders, so nothing here sits in front of a
    /// player's trade. Re-posting is idempotent, so a tick that lands while
    /// nothing has changed leaves the book exactly as it was.
    async fn provisioner_refresh(self: Arc<Self>) {
        // One tick immediately, so a fresh server's book is tradable from the
        // moment the market is built rather than after the first interval.
        let mut first = true;
        loop {
            if !first {
                sleep(PROVISIONER_REFRESH_INTERVAL).await;
            }
            first = false;
            let Some(db) = self.db.clone() else { continue };
            let now = now_secs();
            for d in &self.capital.districts {
                let cfg = self.market_cfg(d.id);
                if cfg.provisioner.is_empty() {
                    continue;
                }
                let Ok(orders) = db.build_orders_for_district(d.id).await else { continue };
                for o in orders.iter().filter(|o| {
                    o.state == "completed" && o.structure_kind.as_deref() == Some("market")
                }) {
                    match db.refresh_provisioner(&o.id, cfg, now).await {
                        Ok(minted) if minted > 0 => println!(
                            "[Proxy] MARKET: provisioner minted {minted}g to fund its floor at \
                             {} ({})",
                            d.id, o.id
                        ),
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("[Proxy] provisioner refresh at {}: {e}", d.id);
                            continue;
                        }
                    }
                    // Balance telemetry (design doc §7). The provisioner rests
                    // orders AT the bounds, so a trade beyond them means either
                    // this job is lagging or the bounds no longer fit the
                    // economy that grew around them. Both are worth knowing, and
                    // neither is visible any other way — this is the data #129's
                    // balance pass has never had.
                    let since = now - PROVISIONER_REFRESH_INTERVAL.as_secs() as i64 * 2;
                    match db.trades_outside_bounds(&o.id, cfg, since).await {
                        Ok(rows) => {
                            for (item, price, lo, hi) in rows {
                                println!(
                                    "[Proxy] MARKET TELEMETRY: {item} traded at {price}g outside \
                                     the provisioner's {lo}-{hi}g band at {} — bounds may be stale",
                                    d.id
                                );
                            }
                        }
                        Err(e) => eprintln!("[Proxy] provisioner telemetry at {}: {e}", d.id),
                    }
                }
            }
        }
    }

    /// Roll the trade ledger into OHLCV candles and prune old ones (#143).
    ///
    /// A background job on purpose: aggregation never sits in front of a trade.
    /// It re-rolls a window that overlaps the current bucket rather than only
    /// the closed one, because the in-progress hour keeps changing — and the
    /// rollup is idempotent, so redoing it costs nothing but CPU.
    async fn candle_rollup(self: Arc<Self>) {
        loop {
            sleep(CANDLE_ROLLUP_INTERVAL).await;
            let Some(db) = self.db.clone() else { continue };
            let now = now_secs();
            // The defaults, not a district's: this job rolls up EVERY market in
            // one pass, so there is only one interval it could use. That is why
            // `market.toml` REFUSES these two keys in a district table (#152) —
            // an override here would silently do nothing, which is exactly the
            // failure mode config validation exists to prevent.
            let interval = self.market_cfg.defaults().candle_interval_secs;
            // Two intervals back, so a late-arriving trade in the previous
            // bucket is still picked up.
            let from = mmo::world::candle_bucket(now, interval) - interval;
            if let Err(e) = db.roll_up_candles(interval, from, now + 1).await {
                eprintln!("[Proxy] candle rollup: {e}");
                continue;
            }
            let cutoff = mmo::world::candle_bucket(
                now - self.market_cfg.defaults().history_retain_days * 86_400,
                interval,
            );
            match db.prune_candles(cutoff).await {
                Ok(n) if n > 0 => println!("[Proxy] MARKET: pruned {n} candle(s) past retention"),
                Ok(_) => {}
                Err(e) => eprintln!("[Proxy] candle prune: {e}"),
            }
        }
    }

    /// Answer `market.history_request` (#143) with a commodity's candles.
    async fn send_history(&self, pid: &str, market_id: &str, item_id: &str, days: i64) {
        let Some(db) = self.db.clone() else { return };
        // Must match what `candle_rollup` materialised, so these read the same
        // global values rather than this market's district.
        let interval = self.market_cfg.defaults().candle_interval_secs;
        let now = now_secs();
        let days = days.clamp(1, self.market_cfg.defaults().history_retain_days);
        let from = mmo::world::candle_bucket(now - days * 86_400, interval);
        let Ok(candles) = db.candles(market_id, item_id, interval, from, now + interval).await else {
            return;
        };
        let arr: Vec<Value> = candles
            .iter()
            .map(|c| json!({
                "t": c.bucket_start, "o": c.open, "h": c.high, "l": c.low,
                "c": c.close, "v": c.volume, "n": c.trades,
            }))
            .collect();
        self.push_to_player(pid, json!({
            "type": "market.history", "market_id": market_id, "item_id": item_id,
            "interval_secs": interval, "candles": arr,
        }));
    }

    /// Release the escrow of every expired order (#140), then tell each owner.
    /// An order left resting holds goods or gold hostage, so this is what
    /// stops a forgotten order stranding them forever.
    async fn sweep_expired_orders(self: Arc<Self>) {
        loop {
            sleep(ORDER_EXPIRY_INTERVAL).await;
            let Some(db) = self.db.clone() else { continue };
            // Listings expire on the same tick, and release identically (#142).
            match db.expire_listings(now_secs()).await {
                Ok(gone) if !gone.is_empty() => {
                    println!("[Proxy] MARKET: expired {} listing(s), items returned", gone.len());
                    for l in &gone {
                        self.send_warehouse(&l.seller_id, &l.market_id).await;
                        self.send_listings(&l.seller_id, &l.market_id, &json!({})).await;
                    }
                }
                Ok(_) => {}
                Err(e) => eprintln!("[Proxy] listing expiry sweep: {e}"),
            }
            let expired = match db.expire_orders(now_secs()).await {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[Proxy] order expiry sweep: {e}");
                    continue;
                }
            };
            if expired.is_empty() {
                continue;
            }
            println!("[Proxy] MARKET: expired {} resting order(s), escrow released", expired.len());
            for o in &expired {
                self.push_gold(&o.character_id, 0, "order_expired").await;
                self.send_warehouse(&o.character_id, &o.market_id).await;
                self.send_own_orders(&o.character_id, &o.market_id).await;
                if let Some(d) = self.district_of_market(&db, &o.market_id).await {
                    self.broadcast_book(&d, &o.market_id, &o.item_id).await;
                }
            }
        }
    }

    /// The district a market sits in, for book broadcasts that aren't driven
    /// by a player's own position (the expiry sweep).
    async fn district_of_market(&self, db: &Db, market_id: &str) -> Option<String> {
        let o = db.build_order_by_id(market_id).await.ok()??;
        Some(o.district)
    }

    /// Push a player's authoritative gold balance (#145's `gold.update`), used
    /// wherever gold moves for a reason other than wages. `delta` is
    /// informational; the balance is read back from the DB either way.
    async fn push_gold(&self, pid: &str, delta: i64, reason: &str) {
        let Some(db) = self.db.clone() else { return };
        let balance = db.character_gold(pid).await.unwrap_or(0);
        self.push_to_player(pid, json!({
            "type": "gold.update", "gold": balance, "delta": delta, "reason": reason,
        }));
    }

    /// The wage rate a given order kind pays per contributed unit (#145).
    ///
    /// Demolition orders (`demo_*`, #106) pay **nothing**. Tearing a road down
    /// is labour too, but demolition refunds the stone while wages already paid
    /// aren't clawed back — so paying for teardown *as well as* rebuild would
    /// make the place → earn → demolish → replace loop profitable in both
    /// directions instead of one. The loop is knowingly left open (it needs an
    /// editor to post the demolition, so players can't reach it), but it
    /// shouldn't be subsidised twice. Revisit before roads ever become
    /// player-demolishable.
    fn wage_for(&self, kind: &str) -> i64 {
        if kind.starts_with("demo_") { 0 } else { BUILD_WAGE_GOLD_PER_UNIT }
    }

    /// Announce wages that were already credited inside the contribution's own
    /// transaction (#145): push the new balance and log the payout. The log line
    /// is the only visibility into faucet rate, and doubles as the alarm for the
    /// demolish → rebuild loop described on `wage_for`.
    async fn pay_wages(&self, pid: &str, order_id: &str, units: i64, wages: i64) {
        if wages <= 0 {
            return;
        }
        let Some(db) = self.db.clone() else { return };
        let balance = db.character_gold(pid).await.unwrap_or(0);
        println!("[Proxy] WAGES: {pid} +{wages}g for {units} unit(s) on {order_id} (balance {balance})");
        self.push_to_player(pid, json!({
            "type": "gold.update", "gold": balance, "delta": wages, "reason": "build_wages",
        }));
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

        // Road plans (#131/#132/#133) route through per-cell contribution
        // instead — unlike every other order, they're deliberately NOT
        // contributable from a district's build board, only from the
        // specific stretch of path you're standing at. `kind` distinguishes
        // this from a demolition order (`demo_<id>`, which also carries
        // `path_json` but never gets cells — see `create_demolition` — and
        // keeps using the ordinary near-any-run proximity below). A road
        // planned before #132 shipped has no cells at all — fall through to
        // the ordinary pooled path below rather than silently refusing
        // every contribution forever.
        if order.kind.starts_with("road_") {
            if let Ok(cells) = db.road_cells_for_order(order_id).await {
                if !cells.is_empty() {
                    self.apply_road_cell_contribute(&db, pid, &order, &cells, item_id, qty).await;
                    return;
                }
            }
        }

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

        let res = match db.contribute(pid, order_id, item_id, qty, self.wage_for(&order.kind)).await {
            Ok(r) => r,
            Err(_) => return,
        };
        if res.moved > 0 {
            // Contributed items left the player's carry — refresh it, then tell the
            // district how the order's progress advanced.
            self.send_inventory(pid).await;
            self.pay_wages(pid, order_id, res.moved, res.wages).await;
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

    /// Answer `road.cells_request` (#134) with a road order's full cell list
    /// — geometry, per-cell cost/progress, and completion — a stateless DB
    /// read like `terrain.list`/`object.list`, no proximity or role gate.
    /// The client uses this to seed its progressive pavement render and its
    /// nearest-cell contribution readout for an open road plan the moment it
    /// first sees one on the board, then keeps it current live via
    /// `road.cell_progress`. A bad/unknown order id just answers empty.
    async fn send_road_cells(&self, pid: &str, order_id: &str) {
        let Some(db) = self.db.clone() else { return };
        let cells = db.road_cells_for_order(order_id).await.unwrap_or_default();
        let arr: Vec<Value> = cells
            .iter()
            .map(|c| {
                json!({
                    "cell_index": c.cell_index,
                    "x0": c.x0, "y0": c.y0, "x1": c.x1, "y1": c.y1,
                    "required": serde_json::from_str::<Value>(&c.required_json).unwrap_or(json!({})),
                    "progress": serde_json::from_str::<Value>(&c.progress_json).unwrap_or(json!({})),
                    "completed": c.completed_at.is_some(),
                })
            })
            .collect();
        self.push_to_player(pid, json!({"type": "road.cells", "order_id": order_id, "cells": arr}));
    }

    /// The road half of `build.contribute` (#131/#132/#133): find the
    /// nearest INCOMPLETE cell within `BOARD_RANGE` of the contributor and
    /// route the deposit there. No board fallback — the whole point of
    /// per-cell roads is that you build the stretch you're standing on, not
    /// bank stone into an arbitrary point along the path from the civic
    /// board.
    async fn apply_road_cell_contribute(
        &self,
        db: &Db,
        pid: &str,
        order: &mmo::persistence::BuildOrder,
        cells: &[mmo::persistence::RoadCell],
        item_id: &str,
        qty: i64,
    ) {
        let Some((px, py)) = self.entity_state.lock().unwrap().get(pid).map(|c| (c.x, c.y)) else { return };
        let nearest_cell = cells
            .iter()
            .filter(|c| c.completed_at.is_none())
            .filter_map(|c| {
                let d2 = point_segment_dist2(px, py, c.x0 as i32, c.y0 as i32, c.x1 as i32, c.y1 as i32);
                (d2 <= (BOARD_RANGE as i64).pow(2)).then_some((c.cell_index, d2))
            })
            .min_by_key(|(_, d2)| *d2);
        let Some((cell_index, _)) = nearest_cell else { return };

        let res = match db
            .contribute_to_road_cell(pid, &order.id, cell_index, item_id, qty, self.wage_for(&order.kind))
            .await
        {
            Ok(r) => r,
            Err(_) => return,
        };
        if res.moved > 0 {
            self.send_inventory(pid).await;
            self.pay_wages(pid, &order.id, res.moved, res.wages).await;
            self.broadcast_to_district(&res.district, json!({
                "type": "road.cell_progress", "order_id": order.id, "cell_index": cell_index,
                "required": cost_json(&res.required), "progress": cost_json(&res.progress),
                "completed": res.cell_completed,
            }));
            // The order's own pooled total still moves in lockstep (mirrored
            // by `contribute_to_road_cell`) — broadcast it too so anything
            // still reading the aggregate (the board list) keeps working
            // unchanged, same message shape as an ordinary order's.
            self.broadcast_to_district(&res.district, json!({
                "type": "build.progress", "order_id": order.id,
                "required": cost_json(&res.order_required), "progress": cost_json(&res.order_progress),
            }));
        }
        if !res.order_completed {
            return;
        }
        self.announce_order_completion(
            db, &order.id, &res.kind, &res.district,
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
        //
        // A dependent can live in a DIFFERENT district than the order that
        // unlocked it — the second market (#153) is exactly that: finishing the
        // capital's market in `civic` opens `market_east` out in the Market
        // District. So the announcement and the board refresh have to follow
        // the dependent, not the completer. Announcing only to `district` would
        // leave anyone standing where the new order actually appeared staring
        // at a stale board until they happened to re-request it.
        let dependents: Vec<(&str, &str)> = self
            .capital
            .build_orders
            .iter()
            .filter(|o| o.prereq == Some(kind))
            .map(|o| (o.district, o.kind))
            .collect();
        let mut unlocked: Vec<(String, String)> = Vec::new(); // (district, order id)
        for (d, k) in dependents {
            if let Ok(Some(o)) = db.open_build_order(d, k).await {
                unlocked.push((d.to_string(), o.id));
            }
        }
        if !unlocked.is_empty() {
            // Group by district so each one hears about its own new orders.
            let mut by_district: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (d, id) in unlocked {
                by_district.entry(d).or_default().push(id);
            }
            for (d, ids) in by_district {
                self.broadcast_to_district(&d, json!({
                    "type": "build.unlocked", "order_ids": ids,
                }));
                if d != district {
                    self.broadcast_build_list(&d).await;
                }
            }
        }
        // Refresh the board for the completing order's district (it just changed
        // state, and any same-district dependents now appear).
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
                let cells = cut_road_cells(&points, stone);
                if let Err(e) = db.insert_road_cells(&order.id, &cells).await {
                    eprintln!("[Proxy] road.plan: persisting cells failed: {e}");
                }
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
        let cells = cut_road_cells(&points, stone);
        match db
            .replan_road_order(&order_id, &district, &required_json, &path_json, &placement, &cells, now_secs())
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
    /// Replay worked-out seams to a zone that has just come up (#166).
    ///
    /// Without this a restart refills the whole mine, which is a free reset for
    /// anyone who notices the server bounce. The zone works out how much of each
    /// respawn window is left; a seam whose window elapsed while the server was
    /// down simply comes back, because the world doesn't pause.
    async fn sync_deposit_state_to_zone(&self, zone_id: &str) {
        let Some(db) = self.db.clone() else { return };
        let Ok(rows) = db.depleted_deposits().await else { return };
        // Only this zone's own rocks. `deposit_state` is keyed by deposit id
        // alone, so without this every zone was sent every worked-out seam in the
        // world — harmless, since a zone drops ids it doesn't know, but the log
        // line then claimed a replay to zones that had nothing replayed to them,
        // which is exactly the sort of false detail you end up trusting later.
        let mine: Vec<(String, i64)> = match self.zone_cfg.interior(zone_id) {
            Some(interior) => rows
                .into_iter()
                .filter(|(id, _)| interior.deposits.iter().any(|d| &d.id == id))
                .collect(),
            None => Vec::new(),
        };
        if mine.is_empty() {
            return;
        }
        let rows = mine;
        let tx = self.zones.lock().unwrap().get(zone_id).map(|z| z.tx.clone());
        let Some(tx) = tx else { return };
        let now = now_secs();
        for (node_id, depleted_at) in &rows {
            let _ = tx.send(Message::Text(
                json!({
                    "type": "deposit_state", "node_id": node_id,
                    "depleted_at": depleted_at, "now": now,
                })
                .to_string(),
            ));
        }
        println!("[Proxy] Replayed {} depleted deposit(s) to {zone_id}", rows.len());
    }

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

    // --- Equipment (mining/abilities epic #123; instanced in #128) ----------

    /// Push a character's tool slot + the abilities it grants to its client as
    /// `equip.update`, now including the live durability of the specific
    /// instance worn (#128) so the HUD's "in hand" line can show wear
    /// without opening Inventory. This is the **only** place ability
    /// cooldowns are computed for display — each `cooldown_ms` is already
    /// level-scaled via [`mmo::world::ability_cooldown_ms`], so the hotbar
    /// always shows exactly what the gateway will enforce on `ability.use`
    /// (#117).
    /// Wear the equipped weapon by one notch after a connecting swing (#160),
    /// and re-push what changed.
    ///
    /// A no-op bare-handed: fists don't wear out, and a player between swords
    /// must not be silently punished for having none. On a break, #128's
    /// auto-unequip applies exactly as it does for a tool, and the zone is told
    /// at once so the very next swing is worth bare-handed damage rather than
    /// the sword's.
    async fn wear_weapon(&self, pid: &str) {
        let Some(db) = self.db.clone() else { return };
        let Ok(Some(_)) = db.wear_equipped_tool(pid, "weapon", 1).await else { return };
        // `send_equipment` re-pushes durability AND re-pushes the loadout, which
        // is what makes a broken sword stop hitting like one immediately.
        self.send_inventory(pid).await;
        self.send_equipment(pid).await;
    }

    /// Walk through a portal (#165) — in from the surface, or back out.
    ///
    /// One command for both directions, resolved from where the player actually
    /// is. A client that could name its destination could name any destination;
    /// asking "which portal are you standing at" instead means the answer is
    /// always a place they could reach on foot.
    ///
    /// Routing lives here rather than in a zone because only the gateway knows
    /// which zones exist, which one a player belongs to, and how to hand them
    /// over without two zones briefly both holding them.
    async fn apply_portal_enter(&self, pid: &str) {
        let Some((x, y, hp, from_zone)) = self
            .entity_state
            .lock()
            .unwrap()
            .get(pid)
            .map(|c| (c.x, c.y, c.hp, c.zone.clone()))
        else {
            return;
        };

        // Which way are we going? Inside a zone we know is an interior, the only
        // portals that count are that interior's own.
        let (target_zone, to) = if self.zone_is_interior(&from_zone) {
            match self.zone_cfg.portal_from_inside(&from_zone, x, y) {
                Some(p) => {
                    let world = p.world;
                    match self.zone_at(world.0, world.1) {
                        Some(z) => (z, world),
                        None => {
                            // The surface zone that owns the exit isn't running.
                            // Refusing beats teleporting somebody into a zone
                            // that doesn't exist — they keep their position and
                            // can try again.
                            self.push_to_player(pid, json!({
                                "type": "portal.error", "code": "no_destination",
                                "detail": "the way out is blocked",
                            }));
                            return;
                        }
                    }
                }
                None => {
                    self.push_to_player(pid, json!({
                        "type": "portal.error", "code": "out_of_range",
                        "detail": "stand at the entrance to leave",
                    }));
                    return;
                }
            }
        } else {
            match self.zone_cfg.portal_from_world(x, y) {
                Some((zone_id, p)) => {
                    if !self.zones.lock().unwrap().contains_key(zone_id) {
                        // Authored but not running: say so rather than
                        // silently doing nothing.
                        self.push_to_player(pid, json!({
                            "type": "portal.error", "code": "closed",
                            "detail": "that way is closed",
                        }));
                        return;
                    }
                    (zone_id.to_string(), p.inside)
                }
                None => {
                    self.push_to_player(pid, json!({
                        "type": "portal.error", "code": "out_of_range",
                        "detail": "stand at the entrance to go in",
                    }));
                    return;
                }
            }
        };

        // Tell the source zone to let go BEFORE the destination is told to
        // spawn. Two zones holding the same player is the failure mode this
        // whole path exists to avoid, and the migration work in #12 learned the
        // hard way that ordering here is not cosmetic.
        let from_tx = self.zones.lock().unwrap().get(&from_zone).map(|z| z.tx.clone());
        if let Some(tx) = from_tx {
            // `player_leave` is the zone's existing "drop this entity" message
            // — reused rather than inventing a second removal path that could
            // drift from it.
            let _ = tx.send(Message::Text(
                json!({"type": "player_leave", "player_id": pid}).to_string(),
            ));
        }
        self.relocate_player(pid, to.0, to.1, hp, &target_zone);
        self.push_to_player(pid, json!({
            "type": "portal.entered", "zone": target_zone,
            "x": to.0, "y": to.1,
            "interior": self.zone_is_interior(&target_zone),
            // Empty/full-light rather than null for a surface destination: an
            // Option serialises to `null`, and a client asking for a string
            // with a default gets the null instead of the default. Absent and
            // "no name" are the same thing here, so say it in a way that can't
            // be mistyped on the way out.
            "display_name": self
                .zone_cfg
                .interior(&target_zone)
                .map(|z| z.display_name.clone())
                .unwrap_or_default(),
            "ambient_light": self
                .zone_cfg
                .interior(&target_zone)
                .map(|z| z.ambient_light)
                .unwrap_or(1.0),
            // The floor plan, so the client can draw a tunnel instead of the
            // surface it can no longer see. The client cannot know the layout
            // any other way — it doesn't read the server's config, and
            // shouldn't. Empty when stepping back out to the world.
            "volumes": self
                .zone_cfg
                .interior(&target_zone)
                .map(|z| {
                    z.volumes
                        .iter()
                        .map(|v| json!({"x0": v.x0, "y0": v.y0, "x1": v.x1, "y1": v.y1}))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        }));
        println!("[Proxy] PORTAL: {pid} {from_zone} -> {target_zone} at ({}, {})", to.0, to.1);
    }

    /// The deposit type a node id names, if it is an authored seam (#166).
    ///
    /// The gateway can answer this because deposits are authored — placement in
    /// `zones.toml`, behaviour in `crafting.toml`, both of which it loads. It
    /// needs to, because the swing cooldown is enforced here and a seam sets its
    /// own swing time.
    fn deposit_for_node(&self, node_id: &str) -> Option<&mmo::crafting_config::DepositType> {
        let kind = self
            .zone_cfg
            .interior
            .values()
            .flat_map(|z| z.deposits.iter())
            .find(|d| d.id == node_id)
            .map(|d| d.kind.as_str())?;
        self.crafting_cfg.deposit(kind)
    }

    /// Whether `pid` is standing close enough to the weapon master to deal with
    /// him (#161) — the same shape as `market_at`: resolved from the gateway's
    /// own position cache, so a client can't claim to be somewhere it isn't.
    fn at_weapon_master(&self, pid: &str) -> bool {
        // Surface fixture, same reasoning as `market_at` (#165).
        if !self.on_surface(pid) {
            return false;
        }
        let Some((px, py)) = self.entity_state.lock().unwrap().get(pid).map(|c| (c.x, c.y)) else {
            return false;
        };
        let (wx, wy) = mmo::world::WEAPON_MASTER_AT;
        dist2(px, py, wx, wy) <= (mmo::world::BOUNTY_RANGE as i64).pow(2)
    }

    /// Hand in trophies for the bounty (#161).
    ///
    /// Answers with `bounty.state` either way — paid or not — because the panel
    /// needs to show progress ("7 of 10") regardless, and a refusal that says
    /// nothing is indistinguishable from a dropped frame.
    // --- The tutorial track and Marlow's handouts (#169) --------------------

    /// Evaluate one condition for a player.
    ///
    /// State conditions read the player; history conditions read the counters
    /// that have been running since their first login. Nothing here can fail
    /// partially: an unreadable database answers `false`, which closes a
    /// handout rather than opening one. That asymmetry is deliberate — the
    /// failure mode of a condition that wrongly answers `true` is an item
    /// printer.
    async fn condition_holds(
        &self,
        db: &Db,
        pid: &str,
        c: &mmo::tutorial_config::Condition,
        counters: &std::collections::HashMap<String, i64>,
    ) -> bool {
        use mmo::tutorial_config::Condition as C;
        match c {
            C::HasItem(item) => {
                db.inventory_qty(pid, item).await.unwrap_or(0) > 0
                    || self.holds_equipped(db, pid, item).await
            }
            C::NoItem(item) => {
                db.inventory_qty(pid, item).await.unwrap_or(-1) == 0
                    && !self.holds_equipped(db, pid, item).await
            }
            C::InventoryBelow(item, n) => match db.inventory_qty(pid, item).await {
                Ok(have) => have < *n,
                Err(_) => false,
            },
            C::Gained(item, n) => counters.get(&format!("gained:{item}")).copied().unwrap_or(0) >= *n,
            C::Made(item) => counters.get(&format!("made:{item}")).copied().unwrap_or(0) > 0,
            C::LoadedFuel => counters.get("loaded_fuel").copied().unwrap_or(0) > 0,
        }
    }

    /// Whether this item is in any equipment slot. A pickaxe in hand is a
    /// pickaxe owned — `no_item pickaxe` must not open for someone holding one.
    async fn holds_equipped(&self, db: &Db, pid: &str, item: &str) -> bool {
        for slot in ["tool", "weapon", "catalyst"] {
            if db.equipped(pid, slot).await.ok().flatten().as_deref() == Some(item) {
                return true;
            }
        }
        false
    }

    async fn counters_for(&self, db: &Db, pid: &str) -> std::collections::HashMap<String, i64> {
        db.tutorial_counters(pid).await.unwrap_or_default().into_iter().collect()
    }

    /// Offer any handout whose conditions all hold. Returns the lines to speak.
    async fn apply_handouts(&self, db: &Db, pid: &str, npc_id: &str) -> Vec<String> {
        let mut spoken = Vec::new();
        let handouts: Vec<_> = self
            .tutorial_cfg
            .handouts_from(npc_id)
            .into_iter()
            .cloned()
            .collect();
        if handouts.is_empty() {
            return spoken;
        }
        let now = now_secs();
        let counters = self.counters_for(db, pid).await;
        for h in handouts {
            // Rate limit BEFORE conditions: cheaper, and a handout inside its
            // cooldown is refused regardless of how deserving the player looks.
            match db.handout_state(pid, &h.npc, &h.item).await {
                Ok(Some((granted_at, _))) => {
                    if h.once || now - granted_at < h.cooldown_secs {
                        continue;
                    }
                }
                Ok(None) => {}
                Err(_) => continue,
            }
            let mut ok = true;
            for c in &h.when {
                if !self.condition_holds(db, pid, c, &counters).await {
                    ok = false;
                    break;
                }
            }
            if !ok {
                continue;
            }
            if db.grant_handout(pid, &h.npc, &h.item, h.qty, h.once, now).await.unwrap_or(0) > 0 {
                if !h.line.is_empty() {
                    spoken.push(h.line.clone());
                }
                println!("[Proxy] {npc_id} handed {pid} {} x{}", h.item, h.qty);
            }
        }
        if !spoken.is_empty() {
            self.send_inventory(pid).await;
        }
        spoken
    }

    /// The track as this player stands against it, with every step already
    /// evaluated — so a step completed before they ever met Marlow arrives
    /// ticked rather than waiting to be noticed.
    async fn send_tutorial_state(&self, pid: &str) {
        let Some(db) = self.db.clone() else { return };
        if self.tutorial_cfg.steps.is_empty() || !self.is_persistent(pid) {
            return;
        }
        let counters = self.counters_for(&db, pid).await;
        let mut steps = Vec::new();
        let mut done = 0;
        for st in &self.tutorial_cfg.steps {
            let complete = self.condition_holds(&db, pid, &st.when, &counters).await;
            if complete {
                done += 1;
            }
            steps.push(json!({"id": st.id, "text": st.text, "done": complete}));
        }
        self.push_to_player(
            pid,
            json!({
                "type": "tutorial.state",
                "steps": steps,
                "done": done,
                "total": self.tutorial_cfg.steps.len(),
            }),
        );
        // Finishing pays out once, through the handout log so the once-ever
        // rule is the same mechanism the charcoal bundle uses rather than a
        // second bespoke flag.
        if done == self.tutorial_cfg.steps.len() && !self.tutorial_cfg.reward.is_empty() {
            let now = now_secs();
            for (item, qty) in &self.tutorial_cfg.reward {
                // `once` makes this atomic — no read-then-write, because two
                // `send_tutorial_state` calls genuinely overlap (the login push
                // and an event push) and both would otherwise pay out.
                if db.grant_handout(pid, "tutorial", item, *qty, true, now).await.unwrap_or(0) > 0 {
                    self.push_to_player(
                        pid,
                        json!({"type": "tutorial.complete", "item": item, "qty": qty}),
                    );
                    self.send_inventory(pid).await;
                }
            }
        }
    }

    /// Record a watched event, then re-push the track if it moved.
    ///
    /// Called for EVERY persistent character, not only ones following the
    /// track. There is no "tutorial started" state to be inside or outside of,
    /// which is exactly what makes the steps retroactive.
    async fn note_tutorial(&self, pid: &str, event: &str, by: i64) {
        let Some(db) = self.db.clone() else { return };
        if !self.is_persistent(pid) {
            return; // guests have no durable progress, and nothing breaks
        }
        if db.note_tutorial_event(pid, event, by, now_secs()).await.is_ok() {
            self.send_tutorial_state(pid).await;
        }
    }

    /// Whether any condition anywhere watches gathering this item — checked
    /// before writing, so an unwatched item costs one set lookup.
    fn tutorial_counts_gain(&self, item_id: &str) -> bool {
        self.tutorial_counted.contains(item_id)
    }

    fn tutorial_counts_made(&self, item_id: &str) -> bool {
        self.tutorial_made.contains(item_id)
    }

    // --- Stations, fuel and timed jobs (mine epic #164, issue #167) ---------

    /// The station this player is standing at, if any.
    ///
    /// Resolved gateway-side from the position cache, exactly like
    /// [`Proxy::market_at`] — a station is a PANEL, not a swing. The zone
    /// validates range for things it simulates (a pick hitting a seam); a panel
    /// has no zone-side representation to validate against, and routing one
    /// through the zone would add a synchronisation surface for nothing.
    ///
    /// **The zone check is not decoration.** Since #165 the same (x, y) exists
    /// both underground and above it, so a station that matched on position
    /// alone could be used from the wrong side of the rock — the mine yard
    /// furnace sits at (12860, 13520), and so does a patch of Gallery B's floor
    /// in the interior's local frame.
    fn station_at(&self, pid: &str) -> Option<(&mmo::zone_config::StationPlacement, &mmo::crafting_config::StationType)> {
        let (px, py, zone) = {
            let cache = self.entity_state.lock().unwrap();
            let c = cache.get(pid)?;
            (c.x, c.y, c.zone.clone())
        };
        let interior = self.zone_is_interior(&zone);
        // NEAREST, not first-in-config-order. Two stations whose radii overlap
        // are authored out by validation, but order-dependence would still be
        // the wrong rule: the client picks the nearest, and a server that
        // picked the first would put the two into quiet disagreement about
        // which station the player is even standing at. (It did — the mine
        // yard's wheel and furnace were 40 apart with radius 40, and the live
        // probe walked to the wheel and was handed the furnace.)
        let mut best: Option<(i64, &mmo::zone_config::StationPlacement, &mmo::crafting_config::StationType)> = None;
        for st in self.zone_cfg.station.iter() {
            // A surface station is unreachable from inside an interior and vice
            // versa, whatever the coordinates say.
            match (&st.interior, interior) {
                (Some(z), true) if *z == zone => {}
                (None, false) => {}
                _ => continue,
            }
            let Some(t) = self.crafting_cfg.station(&st.kind) else { continue };
            let d2 = dist2(px, py, st.pos.0, st.pos.1);
            if d2 > (t.radius as i64).pow(2) {
                continue;
            }
            if best.map(|(bd, _, _)| d2 < bd).unwrap_or(true) {
                best = Some((d2, st, t));
            }
        }
        best.map(|(_, st, t)| (st, t))
    }

    /// Refuse to boot on two stations whose radii overlap (#168).
    ///
    /// This is validated HERE rather than in `zone_config` because it needs both
    /// files: `zones.toml` has the positions, `crafting.toml` has the radii, and
    /// neither can see the other. It has to be validated somewhere, though —
    /// overlapping stations mean a player standing between them can only ever
    /// reach one, and which one depends on a distance comparison they cannot
    /// see. The mine yard shipped exactly that for an afternoon: a wheel and a
    /// furnace 40 apart, both with radius 40.
    fn check_station_spacing(&self) {
        let placed: Vec<_> = self
            .zone_cfg
            .station
            .iter()
            .filter_map(|st| self.crafting_cfg.station(&st.kind).map(|t| (st, t)))
            .collect();
        for (i, (a, ta)) in placed.iter().enumerate() {
            for (b, tb) in placed.iter().skip(i + 1) {
                if a.interior != b.interior {
                    continue;
                }
                let need = ta.radius + tb.radius;
                let d2 = dist2(a.pos.0, a.pos.1, b.pos.0, b.pos.1);
                if d2 < (need as i64).pow(2) {
                    panic!(
                        "stations `{}` and `{}` are {:.0} units apart but their radii total {need}                          — they overlap, so standing between them would reach whichever the                          distance check happened to favour. Move one, or shrink a radius.",
                        a.id, b.id, (d2 as f64).sqrt()
                    );
                }
            }
        }
    }

    /// Where the world's portals are, so a player can SEE one and walk into it.
    ///
    /// This was the gap that made the whole mine epic unreachable: the client
    /// only ever learned a portal existed from `portal.entered`, which arrives
    /// after you are already inside. Nothing was drawn at the adit and nothing
    /// offered to enter it, so Kedron Cut — six issues of content — could only
    /// be reached by a test probe calling `portal.enter` directly.
    ///
    /// Same contract as `send_station_list`: static config, sent once, used by
    /// the client ONLY to decide what to draw and when to offer. The server
    /// re-checks range on the actual `portal.enter` exactly as before.
    fn send_portal_list(&self, pid: &str) {
        let portals: Vec<Value> = self
            .zone_cfg
            .interior
            .iter()
            .flat_map(|(zone_id, z)| {
                z.portals.iter().map(move |p| {
                    json!({
                        "id": p.id,
                        "zone": zone_id,
                        "display_name": z.display_name,
                        "x": p.world.0, "y": p.world.1,
                        // The far side, so the client can offer "leave" from
                        // inside too. One portal entry describes both
                        // directions (#165) and both need a prompt.
                        "inside_x": p.inside.0, "inside_y": p.inside.1,
                        "radius": p.radius,
                    })
                })
            })
            .collect();
        self.push_to_player(pid, json!({"type": "portal.list", "portals": portals}));
    }

    /// The authored station placements, so the client knows where to expect one.
    ///
    /// Includes the radius so the client's "am I close enough to ask?" test
    /// matches the server's "are you close enough to act?" test. They are still
    /// two separate checks — this one just stops the client asking pointlessly.
    fn send_station_list(&self, pid: &str) {
        let stations: Vec<Value> = self
            .zone_cfg
            .station
            .iter()
            .filter_map(|st| {
                let t = self.crafting_cfg.station(&st.kind)?;
                Some(json!({
                    "id": st.id, "name": t.display_name,
                    "x": st.pos.0, "y": st.pos.1,
                    "radius": t.radius,
                    // "" means a surface fixture. The client has to honour this
                    // for the same reason the server does: the same coordinates
                    // exist on both sides of the rock (#165).
                    "zone": st.interior.clone().unwrap_or_default(),
                }))
            })
            .collect();
        self.push_to_player(pid, json!({"type": "station.list", "stations": stations}));
    }

    /// Everything the station panel needs: what it is, how hot it is, what it
    /// can make, and the caller's own jobs.
    ///
    /// The recipe list is derived from config here rather than mirrored in the
    /// client, for the reason #155 made the market's fee rules server-sent: a
    /// client-side copy of a server-side rule becomes a lie the moment the file
    /// is edited.
    async fn send_station_state(&self, pid: &str) {
        let Some(db) = self.db.clone() else { return };
        if !self.is_persistent(pid) {
            return; // guests have no durable inventory or purse
        }
        let cid = pid;
        let Some((placement, t)) = self.station_at(pid).map(|(p, t)| (p.clone(), t.clone())) else {
            self.push_to_player(pid, json!({"type": "station.closed"}));
            return;
        };
        let fuel = db.station_fuel(&placement.id).await.unwrap_or(0);
        let jobs = db.station_jobs(&placement.id, cid).await.unwrap_or_default();
        let now = now_secs();
        let level = db.skill_level(cid, "smelting").await.unwrap_or(0);

        let recipes: Vec<Value> = self
            .crafting_cfg
            .recipes_for(&t)
            .into_iter()
            .map(|(id, r)| {
                let curve = self.crafting_cfg.skill(&r.skill);
                json!({
                    "id": id,
                    "name": r.display_name,
                    "inputs": r.inputs.iter().map(|i| json!({"item": i.item, "qty": i.qty}))
                        .collect::<Vec<_>>(),
                    "output_item": r.output_item,
                    "output_qty": r.output_qty,
                    "fuel_units": r.fuel_units,
                    "fee_gold": t.usage_fee_gold * r.fee_multiplier.max(1),
                    // The duration the CALLER would actually get, with their own
                    // Smelting level already applied. Sending the base and
                    // letting the client apply the curve would put the same rule
                    // in two places.
                    "duration_ms": curve.swing_time_ms(r.duration_ms, level),
                    "skill": r.skill,
                    "required_level": r.required_level,
                    "locked": level < r.required_level,
                    // The caller's ACTUAL odds at their level, not the recipe's
                    // base — same reasoning as `duration_ms` above. A player
                    // deciding whether to risk two clay should be shown the
                    // number that will really be rolled.
                    "failure_pct": (self.crafting_cfg.skill(&r.skill)
                        .failure_chance(r.failure_chance, level) * 100.0 * 10.0).round() / 10.0,
                    "catalyst": r.catalyst.as_ref().map(|c| json!({
                        "item": c.item, "bonus_chance": c.bonus_chance,
                        "speed_bonus_pct": c.speed_bonus_pct,
                    })),
                })
            })
            .collect();

        let jobs: Vec<Value> = jobs
            .iter()
            .map(|j| {
                json!({
                    "id": j.id, "slot": j.slot, "recipe_id": j.recipe_id,
                    "output_item": j.output_item, "output_qty": j.output_qty,
                    "state": j.state, "fail_reason": j.fail_reason,
                    "ready_at": j.ready_at, "started_at": j.started_at,
                    "remaining_secs": (j.ready_at - now).max(0),
                    // A failed job hands back its escrow, so the panel can say
                    // what is waiting rather than just "failed".
                    "refund": j.inputs.iter().map(|(i, q)| json!({"item": i, "qty": q}))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();

        self.push_to_player(
            pid,
            json!({
                "type": "station.state",
                "station_id": placement.id,
                "name": t.display_name,
                "kind": if t.kind == mmo::crafting_config::StationKind::Heat { "heat" } else { "shaping" },
                "fuel_units": fuel,
                "fuels": t.fuels.iter().map(|(i, u)| json!({"item": i, "units": u}))
                    .collect::<Vec<_>>(),
                "job_slots": t.job_slots,
                "usage_fee_gold": t.usage_fee_gold,
                // The panel has to say this out loud (#168): a station you must
                // stay at behaves differently from one you can walk away from,
                // and discovering that by losing a job is the wrong way to learn.
                "requires_presence": t.requires_presence,
                "skill_level": level,
                "recipes": recipes,
                "jobs": jobs,
            }),
        );
    }

    /// Load fuel into the station the player is standing at.
    async fn apply_station_load_fuel(&self, pid: &str, item_id: &str, qty: i64) {
        let Some(db) = self.db.clone() else { return };
        if !self.is_persistent(pid) {
            return; // guests have no durable inventory or purse
        }
        let cid = pid;
        let Some((placement, t)) = self.station_at(pid).map(|(p, t)| (p.clone(), t.clone())) else {
            self.push_to_player(pid, json!({"type": "station.error", "reason": "out_of_range"}));
            return;
        };
        let Some(units_per) = t.fuels.get(item_id).copied() else {
            self.push_to_player(
                pid,
                json!({"type": "station.error", "reason": "not_a_fuel", "item_id": item_id}),
            );
            return;
        };
        match db
            .load_station_fuel(&placement.id, cid, item_id, qty, units_per, now_secs())
            .await
        {
            Ok(Some(_)) => {
                self.note_tutorial(pid, "loaded_fuel", 1).await;
                self.send_inventory(pid).await;
                self.send_station_state(pid).await;
            }
            Ok(None) => self.push_to_player(
                pid,
                json!({"type": "station.error", "reason": "no_fuel_to_load", "item_id": item_id}),
            ),
            Err(e) => eprintln!("[Proxy] load fuel failed: {e}"),
        }
    }

    /// Start a job in the first free slot.
    async fn apply_station_start(&self, pid: &str, recipe_id: &str) {
        let Some(db) = self.db.clone() else { return };
        if !self.is_persistent(pid) {
            return; // guests have no durable inventory or purse
        }
        let cid = pid;
        let Some((placement, t)) = self.station_at(pid).map(|(p, t)| (p.clone(), t.clone())) else {
            self.push_to_player(pid, json!({"type": "station.error", "reason": "out_of_range"}));
            return;
        };
        let Some(recipe) = self.crafting_cfg.recipe(recipe_id).cloned() else {
            self.push_to_player(
                pid,
                json!({"type": "station.error", "reason": "no_such_recipe", "recipe_id": recipe_id}),
            );
            return;
        };
        if !self.crafting_cfg.station_accepts(&t, &recipe) {
            self.push_to_player(
                pid,
                json!({"type": "station.error", "reason": "wrong_station", "recipe_id": recipe_id}),
            );
            return;
        }
        let level = db.skill_level(cid, &recipe.skill).await.unwrap_or(0);
        if level < recipe.required_level {
            self.push_to_player(
                pid,
                json!({
                    "type": "station.error", "reason": "skill_too_low",
                    "recipe_id": recipe_id, "need": recipe.required_level, "have": level,
                }),
            );
            return;
        }

        // First free slot, bounded by the station type. The unique index is what
        // actually enforces this — reading here only picks a candidate.
        let taken: Vec<i64> = db
            .station_jobs(&placement.id, cid)
            .await
            .unwrap_or_default()
            .iter()
            .map(|j| j.slot)
            .collect();
        let Some(slot) = (0..t.job_slots).find(|s| !taken.contains(s)) else {
            self.push_to_player(pid, json!({"type": "station.error", "reason": "no_free_slot"}));
            return;
        };

        let curve = self.crafting_cfg.skill(&recipe.skill);
        let mut duration = curve.swing_time_ms(recipe.duration_ms, level);

        // The catalyst (#168), if they have one with life left in it. Entirely
        // optional: a smelt with no crucible is a perfectly ordinary smelt, and
        // that is deliberate — a required catalyst would mean a clay shortage
        // stalls the whole iron economy.
        let mut catalyst: Option<(String, i64, f64)> = None;
        if let Some(c) = &recipe.catalyst {
            if let Ok(Some(eq)) = db.equipped_tool(cid, "catalyst").await {
                if eq.item_id == c.item && eq.durability > 0 {
                    duration = (duration as f64 * (1.0 - c.speed_bonus_pct / 100.0)).round() as i64;
                    catalyst = Some((eq.instance_id, c.wear, c.bonus_chance));
                }
            }
        }

        // Rolled HERE and fixed on the row, not at collect. Whether an attempt
        // spoils decides whether its inputs come back, and that is a custody
        // question — custody is settled when the goods change hands, not when
        // the player gets round to clicking Collect.
        let fail_chance = curve.failure_chance(recipe.failure_chance, level);
        let will_fail = fail_chance > 0.0 && rand::random::<f64>() < fail_chance;

        match db
            .start_station_job(
                &placement.id, cid, slot, recipe_id, &recipe,
                t.usage_fee_gold * recipe.fee_multiplier.max(1),
                duration.max(1), will_fail, catalyst, now_secs(),
            )
            .await
        {
            Ok(Ok(_)) => {
                self.send_inventory(pid).await;
                self.push_gold(pid, 0, "station_fee").await;
                self.send_station_state(pid).await;
            }
            Ok(Err(e)) => {
                let mut msg = json!({"type": "station.error", "reason": e.code()});
                if let mmo::persistence::StartJobError::MissingInput { item, need, have } = &e {
                    msg["item_id"] = json!(item);
                    msg["need"] = json!(need);
                    msg["have"] = json!(have);
                } else if let mmo::persistence::StartJobError::NotEnoughFuel { need, have }
                    | mmo::persistence::StartJobError::NotEnoughGold { need, have } = &e
                {
                    msg["need"] = json!(need);
                    msg["have"] = json!(have);
                }
                self.push_to_player(pid, msg);
            }
            Err(e) => eprintln!("[Proxy] start job failed: {e}"),
        }
    }

    /// Collect a finished job.
    ///
    /// The bonus roll happens HERE rather than at start, so a player who levels
    /// Smelting while a job runs gets the benefit — and so the roll can't be
    /// seen and then cancelled.
    async fn apply_station_collect(&self, pid: &str, job_id: &str) {
        let Some(db) = self.db.clone() else { return };
        if !self.is_persistent(pid) {
            return; // guests have no durable inventory or purse
        }
        let cid = pid;
        // Deliberately NOT range-gated. Collecting is taking back what is
        // already yours, and a player who walks away mid-job then logs in
        // somewhere else should not have their materials stranded.
        let jobs = db.all_station_jobs().await.unwrap_or_default();
        let Some(job) = jobs.into_iter().find(|j| j.id == job_id && j.character_id == cid) else {
            self.push_to_player(pid, json!({"type": "station.error", "reason": "no_such_job"}));
            return;
        };
        let level = db.skill_level(cid, &job.skill).await.unwrap_or(0);
        // Two independent chances at one extra unit: the player's skill, and the
        // crucible they burned. `catalyst_bonus` was recorded when the crucible
        // was worn, so a crucible that has since broken or been traded away
        // still pays out on the job it funded.
        let chance = self.crafting_cfg.skill(&job.skill).bonus_yield_chance(level) * 100.0
            + job.catalyst_bonus;
        let bonus = if job.state == "ready" && rand::random::<f64>() * 100.0 < chance { 1 } else { 0 };
        // What a spoiled attempt still teaches. Zero if the recipe is gone —
        // there is nothing left to read the fraction off, and a refunded job
        // grants nothing anyway.
        let spoiled_xp = self
            .crafting_cfg
            .recipe(&job.recipe_id)
            .map(|r| (job.xp as f64 * r.failure_xp_fraction).round() as i64)
            .unwrap_or(0);

        match db.collect_station_job(job_id, cid, bonus, spoiled_xp, now_secs()).await {
            Ok(Ok(done)) => {
                // Spoiled attempts included: failure that teaches nothing is
                // just a tax, and `done.xp` already carries the reduced figure.
                if !done.failed && done.xp > 0 && !done.skill.is_empty() {
                    if let Ok(gain) = db.grant_skill_xp(cid, &done.skill, done.xp).await {
                        self.push_skill_gain(pid, &gain);
                    }
                }
                if !done.failed && !done.spoiled {
                    for (item, _) in &done.payout {
                        if self.tutorial_counts_made(item) {
                            self.note_tutorial(pid, &format!("made:{item}"), 1).await;
                        }
                    }
                }
                self.send_inventory(pid).await;
                self.push_to_player(
                    pid,
                    json!({
                        "type": "station.collected",
                        "slot": done.slot,
                        "failed": done.failed,
                        "spoiled": done.spoiled,
                        "fail_reason": done.fail_reason,
                        "bonus": bonus,
                        "items": done.payout.iter().map(|(i, q)| json!({"item": i, "qty": q}))
                            .collect::<Vec<_>>(),
                    }),
                );
                self.send_station_state(pid).await;
            }
            Ok(Err(e)) => {
                let mut msg = json!({"type": "station.error"});
                match e {
                    mmo::persistence::CollectError::NoSuchJob => msg["reason"] = json!("no_such_job"),
                    mmo::persistence::CollectError::NotReady { ready_at } => {
                        msg["reason"] = json!("not_ready");
                        msg["ready_at"] = json!(ready_at);
                    }
                    // The job is untouched and still holding the goods. Saying
                    // so is the whole point — a silent failure here looks
                    // exactly like the output having been destroyed.
                    mmo::persistence::CollectError::NoRoom { need, room } => {
                        msg["reason"] = json!("no_room");
                        msg["need"] = json!(need);
                        msg["room"] = json!(room);
                    }
                }
                self.push_to_player(pid, msg);
            }
            Err(e) => eprintln!("[Proxy] collect job failed: {e}"),
        }
    }

    /// Cancel every running job whose owner has walked away from a station that
    /// requires them to stay (#168).
    ///
    /// `requires_presence` was declared on `StationType` in #167 and read
    /// nowhere — dead config, which is the same trap the mine epic has now hit
    /// three times. The wheel is what makes it real: shaping is an ACTIVE
    /// action, and walking away has to stop it.
    ///
    /// Cancelling refunds everything. That is the deliberate asymmetry with
    /// spoiling: losing clay to bad luck is the mechanic, losing it to a
    /// disconnect is a bug wearing the mechanic's clothes.
    async fn cancel_absent_station_jobs(&self) {
        let Some(db) = self.db.clone() else { return };
        let watched: Vec<(String, i64)> = self
            .zone_cfg
            .station
            .iter()
            .filter_map(|st| {
                let t = self.crafting_cfg.station(&st.kind)?;
                t.requires_presence.then(|| (st.id.clone(), 0))
            })
            .collect();
        if watched.is_empty() {
            return;
        }
        let jobs = db.all_station_jobs().await.unwrap_or_default();
        for job in jobs {
            if job.state != "running" || !watched.iter().any(|(id, _)| *id == job.station_id) {
                continue;
            }
            // Still standing there? `station_at` answers with the gateway's own
            // position cache, so this is the same test the start had to pass.
            let present = self
                .station_at(&job.character_id)
                .map(|(st, _)| st.id == job.station_id)
                .unwrap_or(false);
            if present {
                continue;
            }
            if let Ok(Some(cancelled)) = db.cancel_station_job(&job.id, now_secs()).await {
                println!(
                    "[Proxy] Cancelled {} at {} — walked away; escrow returned",
                    cancelled.recipe_id, cancelled.station_id
                );
                let pid = cancelled.character_id.clone();
                self.push_to_player(
                    &pid,
                    json!({
                        "type": "station.cancelled", "station_id": cancelled.station_id,
                        "slot": cancelled.slot, "recipe_id": cancelled.recipe_id,
                        "reason": "walked_away",
                    }),
                );
                self.send_inventory(&pid).await;
                self.send_station_state(&pid).await;
            }
        }
    }

    /// Ripen every job that has come due and tell whoever is online.
    ///
    /// Runs on the periodic sweep rather than on a per-job timer, so a job
    /// started before a restart still finishes — the row carries an absolute
    /// `ready_at`, and nothing has to be rescheduled at boot.
    async fn sweep_station_jobs(&self) {
        let Some(db) = self.db.clone() else { return };
        let ripe = match db.ripen_station_jobs(now_secs()).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[Proxy] ripen jobs failed: {e}");
                return;
            }
        };
        for job in ripe {
            let pid = job.character_id.clone();
            let pid = pid.as_str();
            self.push_to_player(
                pid,
                json!({
                    "type": "station.ready",
                    "station_id": job.station_id, "job_id": job.id, "slot": job.slot,
                    "output_item": job.output_item, "output_qty": job.output_qty,
                }),
            );
            self.send_station_state(pid).await;
        }
    }

    /// Ripen due jobs once a second.
    async fn station_job_monitor(&self) {
        loop {
            sleep(Duration::from_secs(1)).await;
            // Presence first: a job the player abandoned should be handed back
            // rather than ripened into something they then have to walk back for.
            self.cancel_absent_station_jobs().await;
            self.sweep_station_jobs().await;
        }
    }

    /// At boot, fail any job whose recipe has gone from `crafting.toml`.
    ///
    /// There is no hot reload in this server: config is read once at startup.
    /// So the moment a recipe can vanish out from under a live job is a
    /// RESTART with an edited file — a hand-edited config plus rows that
    /// outlived it, which is an ordinary Tuesday rather than an invariant
    /// violation. The escrow is refunded to the slot and the player is told why.
    async fn fail_orphaned_station_jobs(&self) {
        let Some(db) = self.db.clone() else { return };
        let jobs = match db.all_station_jobs().await {
            Ok(j) => j,
            Err(e) => {
                eprintln!("[Proxy] reading station jobs failed: {e}");
                return;
            }
        };
        let mut failed = 0;
        for job in jobs {
            if job.state == "failed" {
                continue;
            }
            let gone = self.crafting_cfg.recipe(&job.recipe_id).is_none();
            let homeless = !self.zone_cfg.station.iter().any(|s| s.id == job.station_id);
            let reason = if gone {
                "recipe_removed"
            } else if homeless {
                "station_removed"
            } else {
                continue;
            };
            if db.fail_station_job(&job.id, reason).await.unwrap_or(false) {
                failed += 1;
            }
        }
        if failed > 0 {
            println!("[Proxy] Failed {failed} station job(s) whose recipe or station is gone — escrow refunded");
        }
    }

    /// Whether this connection has a durable character behind it. Guests have
    /// no inventory or purse, so every persistence-touching interaction has to
    /// ask — the bounty (#161) does the same check inline.
    fn is_persistent(&self, pid: &str) -> bool {
        self.clients.lock().unwrap().get(pid).map(|i| i.persistent).unwrap_or(false)
    }

    async fn apply_bounty_turn_in(&self, pid: &str, command_id: &str) {
        let Some(db) = self.db.clone() else { return };
        let persistent = self
            .clients
            .lock()
            .unwrap()
            .get(pid)
            .map(|i| i.persistent)
            .unwrap_or(false);
        if !persistent {
            return; // guests have no durable inventory or purse
        }
        if !self.at_weapon_master(pid) {
            self.push_to_player(pid, json!({
                "type": "bounty.error", "code": "out_of_range",
                "detail": "stand with the weapon master to claim the bounty",
            }));
            return;
        }
        let cfg = self.market_cfg.bounty();
        match db.turn_in_bounty(pid, cfg, command_id, now_secs()).await {
            Ok((paid, held)) => {
                if paid > 0 {
                    println!("[Proxy] BOUNTY: paid {paid}g for {} {}", cfg.required, cfg.item_id);
                    self.send_inventory(pid).await;
                    self.push_gold(pid, paid, "bounty").await;
                }
                self.push_to_player(pid, json!({
                    "type": "bounty.state", "item_id": cfg.item_id,
                    "required": cfg.required, "gold": cfg.gold,
                    "held": held, "paid": paid,
                }));
            }
            Err(e) => {
                eprintln!("[Proxy] bounty.turn_in: {e}");
                self.push_to_player(pid, json!({
                    "type": "server_error", "detail": "the bounty could not be paid",
                }));
            }
        }
    }

    /// Tell the zone that owns `pid` what their swing is worth (#160).
    ///
    /// The zone resolves melee — arc, reach, who got hit — but cannot see
    /// equipment, which lives in this gateway's database. Same split as
    /// `env_state` (#87): the gateway computes the verdict, the zone stores it
    /// and applies it. Sent on every equipment change and again on the periodic
    /// sweep, so a migrated entity reconverges rather than staying stuck.
    fn push_loadout_to_zone(&self, pid: &str, weapon_item: Option<&str>) {
        let zone_id = match self.clients.lock().unwrap().get(pid) {
            Some(i) => i.current_zone.clone(),
            None => return,
        };
        let tx = self.zones.lock().unwrap().get(&zone_id).map(|z| z.tx.clone());
        if let Some(tx) = tx {
            let _ = tx.send(Message::Text(
                json!({
                    "type": "loadout", "player_id": pid,
                    "melee_damage": mmo::world::melee_damage(weapon_item),
                })
                .to_string(),
            ));
        }
    }

    async fn send_equipment(&self, pid: &str) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        let equipped = db.equipped_tool(pid, "tool").await.ok().flatten();
        let mut abilities: Vec<Value> = Vec::new();
        if let Some(tool) = &equipped {
            for a in mmo::world::abilities_for_item(&tool.item_id) {
                let level = match mmo::world::governing_skill(a.id) {
                    Some(skill) => db.skill_level(pid, skill).await.unwrap_or(0),
                    None => 0,
                };
                abilities.push(json!({
                    "id": a.id, "name": a.name,
                    "cooldown_ms": mmo::world::ability_cooldown_ms(a.id, level),
                }));
            }
        }
        // The weapon slot (#160) rides the same message as the tool, in its own
        // fields. Two slots means the wire has to carry both without either
        // clobbering the other — a client reading `tool` and a client reading
        // `weapon` are looking at different equipment.
        let weapon = db.equipped_tool(pid, "weapon").await.ok().flatten();
        self.push_to_player(pid, json!({
            "type": "equip.update", "player_id": pid,
            "tool": equipped.as_ref().map(|t| t.item_id.clone()),
            "durability": equipped.as_ref().map(|t| t.durability),
            "max_durability": equipped.as_ref().map(|t| t.max_durability),
            "abilities": abilities,
            "weapon": weapon.as_ref().map(|w| w.item_id.clone()),
            "weapon_durability": weapon.as_ref().map(|w| w.durability),
            "weapon_max_durability": weapon.as_ref().map(|w| w.max_durability),
            "melee_damage": mmo::world::melee_damage(weapon.as_ref().map(|w| w.item_id.as_str())),
        }));
        // ...and the zone is told immediately, so a sword drawn is a sword that
        // hits harder on the very next swing rather than up to a second later
        // when the periodic sweep catches up.
        self.push_loadout_to_zone(pid, weapon.as_ref().map(|w| w.item_id.as_str()));
    }

    /// Arm a SPECIFIC owned tool instance (#128 — "the pickaxe" stopped
    /// being well-defined once tools carry their own durability); the slot
    /// is derived from the instance's own item. Not owning it (an unknown
    /// instance id, or one belonging to someone else — e.g. a stale
    /// inventory view) gets an explicit `equip_error`.
    async fn apply_equip(&self, pid: &str, instance_id: &str) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        match db.equip_instance(pid, instance_id).await {
            Ok(Some(_)) => self.send_equipment(pid).await,
            Ok(None) => self.push_to_player(pid, json!({
                "type": "equip_error", "message": "you don't have one of those",
            })),
            Err(_) => {}
        }
    }

    /// Repair a specific owned tool instance at a crafting station (#128) —
    /// same "confirm they own a crafting-kind structure on their own plot"
    /// gate [`Db::apply_craft_make`] uses. Silent no-op on failure (no
    /// station, unknown/unowned instance, nothing missing, can't afford it)
    /// — mirrors crafting's existing failure posture.
    async fn apply_repair(&self, pid: &str, instance_id: &str) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        let Ok(Some(plot)) = db.plot_for_character(pid).await else { return };
        let Ok(structures) = db.structures_for_plot(&plot.id).await else { return };
        if !structures.iter().any(|s| s.kind == "crafting") {
            return;
        }
        let Ok(Some(outcome)) = db.repair_instance(pid, instance_id).await else { return };
        self.send_inventory(pid).await;
        self.send_equipment(pid).await; // durability may be the currently-equipped instance's
        self.push_to_player(pid, json!({
            "type": "repair.done", "instance_id": instance_id, "item_id": outcome.item_id,
            "cost": Value::Object(outcome.cost.into_iter().map(|(k, v)| (k, json!(v))).collect()),
        }));
    }

    /// Clear the tool slot.
    async fn apply_unequip(&self, pid: &str) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        if db.unequip(pid, "tool").await.is_ok() {
            self.send_equipment(pid).await;
        }
    }

    // --- Abilities (mining/abilities epic #123, #117) -----------------------

    /// The gateway's half of an ability use: confirm the wielder actually
    /// has a tool granting `ability_id` and that its per-character cooldown
    /// has elapsed — both server-authoritative, both things only the
    /// gateway knows (equipment + skill levels live in the DB). If both
    /// pass, hand off to the current zone for range/stock/target
    /// validation, which is the only party with live positions and node
    /// state. The cooldown ledger is stamped **before** forwarding — a
    /// swing costs its cooldown the moment it's thrown, hit or miss, same
    /// as a real swing would.
    async fn apply_ability_use(&self, pid: &str, ability_id: &str, node_id: &str) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        let tool = db.equipped(pid, "tool").await.ok().flatten();
        let granted = tool
            .as_deref()
            .map(|t| mmo::world::abilities_for_item(t).iter().any(|a| a.id == ability_id))
            .unwrap_or(false);
        if !granted {
            self.push_to_player(pid, json!({
                "type": "ability.result", "id": ability_id, "ok": false, "reason": "no_tool",
            }));
            return;
        }
        let level = match mmo::world::governing_skill(ability_id) {
            Some(skill) => db.skill_level(pid, skill).await.unwrap_or(0),
            None => 0,
        };
        // A DEPOSIT sets its own swing time (#166): clay is quicker to work than
        // iron, and that difference is authored per seam rather than being a
        // property of the arm swinging the pick. Without this the deposit's
        // `swing_time_ms` would be a config value that did nothing, since the
        // ability curve alone decided the rate.
        //
        // The Mining curve then takes its percentage off that, capped. Ordinary
        // surface nodes keep #129's ability curve exactly as it was.
        let cooldown_ms = match self.deposit_for_node(node_id) {
            Some(d) => self.crafting_cfg.skill(&d.skill).swing_time_ms(d.swing_time_ms, level),
            None => mmo::world::ability_cooldown_ms(ability_id, level),
        };
        let now = Instant::now();
        {
            let mut ledger = self.ability_cooldowns.lock().unwrap();
            let key = (pid.to_string(), ability_id.to_string());
            if let Some(&last) = ledger.get(&key) {
                if now.saturating_duration_since(last) < Duration::from_millis(cooldown_ms as u64) {
                    self.push_to_player(pid, json!({
                        "type": "ability.result", "id": ability_id, "ok": false,
                        "reason": "cooldown", "cooldown_ms": cooldown_ms,
                    }));
                    return;
                }
            }
            ledger.insert(key, now);
        }
        self.route_client_frame(pid, json!({
            "type": "ability_swing", "id": ability_id, "node_id": node_id, "cooldown_ms": cooldown_ms,
            // The swinger's level in the governing skill (#166). The zone rolls
            // the loot table but cannot see skills, so the bonus-yield chance
            // has to travel with the swing — same shape as the pre-scaled
            // cooldown beside it.
            "skill_level": level,
        }));
    }

    // --- NPCs (mining/abilities epic #123, #118, generalized #126) -----------

    /// Resolve a talk the zone already confirmed was in range: fully
    /// data-driven off the NPC's own authored `grants_item`/`lines_*` (#126)
    /// — hands over that item the first time (and any time since — it's a
    /// safety net, not a farm) a character has none at all, in inventory or
    /// in hand; otherwise (or for an NPC with no `grants_item`) just talks.
    /// Unknown NPC ids are a silent no-op.
    /// Tell `pid` where they stand against the bounty (#161), WITHOUT paying it.
    ///
    /// Talking to the weapon master reports the offer; claiming it is a separate,
    /// deliberate act. Conflating the two would mean walking up to him silently
    /// spends ten pelts the moment you have them.
    async fn send_bounty_state(&self, pid: &str) {
        let Some(db) = self.db.clone() else { return };
        let cfg = self.market_cfg.bounty();
        let held = db.inventory_qty(pid, &cfg.item_id).await.unwrap_or(0);
        self.push_to_player(pid, json!({
            "type": "bounty.state", "item_id": cfg.item_id,
            "required": cfg.required, "gold": cfg.gold,
            "held": held, "paid": 0,
        }));
    }

    async fn apply_npc_interact(&self, pid: &str, npc_id: &str) {
        let db = match &self.db { Some(d) => d.clone(), None => return };
        let Some(npc) = mmo::world::npc(npc_id) else { return };
        let mut granted = false;
        if let Some(item_id) = npc.grants_item {
            let owned = db.inventory_qty(pid, item_id).await.unwrap_or(0);
            let equipped = db.equipped(pid, "tool").await.ok().flatten();
            let has_one = owned > 0 || equipped.as_deref() == Some(item_id);
            if !has_one {
                let added = db.add_to_inventory(pid, item_id, 1).await.unwrap_or(0);
                if added > 0 {
                    granted = true;
                    self.send_inventory(pid).await;
                }
            }
        }
        // Conditional handouts (#169) sit alongside the legacy single-item
        // `grants_item`, not inside it: the static field cannot express "no
        // pickaxe AND no ore, and not in the last ten minutes", and a second
        // hardcoded special case beside Bram's is how a registry becomes a pile.
        let extra = if self.is_persistent(pid) {
            self.apply_handouts(&db, pid, npc_id).await
        } else {
            Vec::new() // guests have no durable inventory to hand anything into
        };
        let mut lines: Vec<String> = if granted { npc.lines_granted } else { npc.lines_repeat }
            .iter()
            .map(|l| (*l).to_string())
            .collect();
        let granted = granted || !extra.is_empty();
        // A handout's own line replaces the idle chatter rather than following
        // it — being told what you were just given matters more than flavour.
        if !extra.is_empty() {
            lines = extra;
        }
        self.push_to_player(pid, json!({
            "type": "npc.dialogue", "npc_id": npc_id, "name": npc.name,
            "lines": lines, "granted": granted,
        }));
        // Talking is not what advances the track, but it IS the moment a player
        // asks where they stand — so answer, with every step already evaluated.
        self.send_tutorial_state(pid).await;
        // Talking to the weapon master reports the bounty (#161); claiming it is
        // a separate, deliberate act. Conflating the two would mean walking up
        // to him silently spends ten pelts the moment you have them.
        if npc.grants_item == Some("sword") {
            self.send_bounty_state(pid).await;
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
            // An interior player's coordinates mean nothing on the surface map
            // (#165). Reading the water mask or the poison-tree positions at
            // them would drown or poison somebody standing in a dry tunnel,
            // purely because their interior coordinates happened to land in the
            // river. Push a cleared environment instead of a wrong one — the
            // zone treats it as the gateway's verdict either way.
            if self.zone_is_interior(&zone_id) {
                let tx = self.zones.lock().unwrap().get(&zone_id).map(|z| z.tx.clone());
                if let Some(tx) = tx {
                    let _ = tx.send(Message::Text(
                        json!({
                            "type": "env_state", "player_id": pid,
                            "submerged": false, "poison_sources": 0,
                        })
                        .to_string(),
                    ));
                }
                continue;
            }
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
            // The loadout rides the same sweep (#160). Equipment changes push
            // immediately; this is the backstop that heals an entity recreated
            // by a migration or a respawn, exactly as the env flags do.
            let weapon = match &self.db {
                Some(db) => db.equipped_tool(&pid, "weapon").await.ok().flatten(),
                None => None,
            };
            self.push_loadout_to_zone(&pid, weapon.as_ref().map(|w| w.item_id.as_str()));
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
    ///
    /// `ability_id`, when present (#128), wears down whatever tool that
    /// ability's swing used: normally -1 durability, a flat 15% chance of
    /// -2 (a "rough" swing) — rolled here, not in persistence, since it's
    /// gameplay tuning. Hitting 0 auto-unequips (the durable write does
    /// that atomically); either way the client needs a fresh `equip.update`
    /// afterward since its cooldown display or the armed tool itself may
    /// have just changed.
    #[allow(clippy::too_many_arguments)]
    async fn apply_gather_yield(
        &self,
        pid: &str,
        item_id: &str,
        qty: i64,
        skill: &str,
        xp: i64,
        ability_id: Option<&str>,
        source: Option<&str>,
    ) {
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
        // A deposit swing that rolled nothing still gets here (#166): the
        // charge was spent, the tool wore down and the XP was earned, there is
        // simply no item. Skip the grant rather than treating an empty id as an
        // error — a miss is a normal outcome, not a failure.
        let added = if item_id.is_empty() || qty <= 0 {
            0
        } else {
            match db.add_to_inventory(pid, item_id, qty).await {
                Ok(n) => {
                    // The track counts what was actually GATHERED, not what fit
                    // (#169). A full pack is a carrying problem; it should not
                    // also silently stall the tutorial.
                    if self.tutorial_counts_gain(item_id) {
                        self.note_tutorial(pid, &format!("gained:{item_id}"), qty).await;
                    }
                    n
                }
                Err(_) => return,
            }
        };
        // A full bag must not silently eat a kill's loot (#159). Gathering can
        // afford to be quiet about it — you're standing at the node and can see
        // the count refuse to move — but a creature is GONE, and doing the work
        // for nothing with no explanation is the version of this that feels
        // broken. `MAX_CARRY` is the cap; the storehouse and the warehouse are
        // the answer, so say so.
        if added < qty && !item_id.is_empty() && source == Some("kill") {
            self.push_to_player(pid, json!({
                "type": "loot.lost", "item_id": item_id, "qty": qty - added,
                "detail": "your pack is full — the kill's loot was left behind",
            }));
        }
        self.send_inventory(pid).await;
        if let Ok(gain) = db.grant_skill_xp(pid, skill, xp).await {
            self.push_skill_gain(pid, &gain);
        }
        let is_tool_swing = ability_id.and_then(mmo::world::governing_tool).is_some();
        if is_tool_swing {
            let loss = if rand::random::<f64>() < 0.15 { 2 } else { 1 };
            if let Ok(Some(_outcome)) = db.wear_equipped_tool(pid, "tool", loss).await {
                // Re-push inventory unconditionally, not just on break — the
                // Inventory panel shows per-instance durability on every row,
                // which just went stale the moment this swing landed, same
                // as the equip.update below (the HUD's in-hand line) always does.
                self.send_inventory(pid).await;
                self.send_equipment(pid).await; // live durability either way; cleared tool if it broke
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

        // Where a returning character reappears (#165).
        //
        // Surface: whichever zone owns the saved position, as always.
        //
        // Interior: the saved ZONE, because the saved position is in that
        // zone's own space and means nothing outside it. The position is then
        // re-validated against the CURRENT geometry — if the layout changed and
        // that spot is now solid rock, the player lands on the spawn anchor
        // instead. Checking the position directly is stronger than comparing a
        // geometry version: it tests the thing that actually matters (is this
        // still floor?) rather than a proxy for it.
        //
        // An interior that no longer exists, or isn't running, falls back to
        // the surface — nobody is stranded in a zone that isn't there.
        let mut spawn_pos = (identity.x, identity.y);
        let spawn_zone_id = if !identity.persistent {
            default_zone_id.clone()
        } else if self.zone_is_interior(&identity.saved_zone) {
            match self.zone_cfg.interior(&identity.saved_zone) {
                Some(z) => {
                    spawn_pos = z.nearest_walkable(identity.x, identity.y);
                    identity.saved_zone.clone()
                }
                None => {
                    spawn_pos = (SPAWN_X, SPAWN_Y);
                    self.zone_at(SPAWN_X, SPAWN_Y).unwrap_or_else(|| default_zone_id.clone())
                }
            }
        } else {
            self.zone_at(identity.x, identity.y)
                .unwrap_or_else(|| default_zone_id.clone())
        };
        let (identity_x, identity_y) = spawn_pos;

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
                        EntityCache {
                            x: identity_x,
                            y: identity_y,
                            hp: identity.hp,
                            zone: spawn_zone_id.clone(),
                        },
                    );
                    let _ = zone.tx.send(Message::Text(
                        json!({"type": "spawn_entity", "player_id": player_id,
                               "x": identity_x, "y": identity_y, "hp": identity.hp})
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

        // Hydrate the client's gameplay state: inventory, storage, skills,
        // equipped tool + its abilities, the district's build-order board, any
        // already-completed city structures, the character's starter plot
        // (allocating one on a brand-new character), every home structure in
        // the district (everyone's homes, not just theirs), and their own
        // plot's rent status.
        if identity.persistent {
            self.send_inventory(&player_id).await;
            self.send_storage(&player_id).await;
            self.send_skills(&player_id).await;
            // Where the world's stations stand (#167). Sent once, because it is
            // static config — the client uses it ONLY to decide when to ask
            // `station.open`, never as permission. The server re-resolves the
            // station from its own position cache on every command, exactly as
            // it does for markets.
            self.send_station_list(&player_id);
            self.send_portal_list(&player_id);
            // The track, with every step already evaluated (#169). Sent at
            // login rather than on meeting Marlow: the counters have been
            // running since this character's first session, so a player who
            // has already done half of it sees half of it ticked.
            self.send_tutorial_state(&player_id).await;
            self.send_equipment(&player_id).await;
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
                    // `equip`/`unequip` (mining/abilities epic #123, instanced in
                    // #128): arming a tool is pure inventory bookkeeping, no live
                    // position or proximity involved — answer directly, same
                    // reasoning as `craft.list`.
                    if data.get("type").and_then(|v| v.as_str()) == Some("equip") {
                        let instance_id = data.get("instance_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        self.apply_equip(&player_id, &instance_id).await;
                        continue;
                    }
                    if data.get("type").and_then(|v| v.as_str()) == Some("unequip") {
                        self.apply_unequip(&player_id).await;
                        continue;
                    }
                    // `repair` (#128): same "owns a crafting station on their own
                    // plot" gate as `craft.make` — no live position/proximity
                    // involved, answer directly.
                    if data.get("type").and_then(|v| v.as_str()) == Some("repair") {
                        let instance_id = data.get("instance_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        self.apply_repair(&player_id, &instance_id).await;
                        continue;
                    }
                    // `ability.use` (mining/abilities epic #123, #117) needs the
                    // gateway's DB (equipped tool, skill level, cooldown ledger)
                    // before the zone's range/stock check even makes sense —
                    // answer directly like `equip`, then forward internally.
                    if data.get("type").and_then(|v| v.as_str()) == Some("ability.use") {
                        let id = data.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let node_id = data.get("node_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        self.apply_ability_use(&player_id, &id, &node_id).await;
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
                    // `market.open` (#137) resolves the market from the
                    // caller's live position, so it's answered here with the
                    // gateway's position cache rather than round-tripping the
                    // zone — same as `build_contribute`'s own proximity gate.
                    if data.get("type").and_then(|v| v.as_str()) == Some("market.open") {
                        self.apply_market_open(&player_id).await;
                        continue;
                    }
                    // Stations (#167). All four share `station_at`'s gateway-side
                    // range gate, the same one `market.open` uses — a station is
                    // a panel, so the zone has no part in it.
                    if let Some(op) = data
                        .get("type")
                        .and_then(|v| v.as_str())
                        .and_then(|t| t.strip_prefix("station."))
                        .filter(|op| matches!(*op, "open" | "load_fuel" | "start" | "collect"))
                    {
                        let op = op.to_string();
                        match op.as_str() {
                            "open" => self.send_station_state(&player_id).await,
                            "load_fuel" => {
                                let item = data.get("item_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                let qty = data.get("qty").and_then(|v| v.as_i64()).unwrap_or(1);
                                self.apply_station_load_fuel(&player_id, &item, qty).await;
                            }
                            "start" => {
                                let r = data.get("recipe_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                self.apply_station_start(&player_id, &r).await;
                            }
                            _ => {
                                let j = data.get("job_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                self.apply_station_collect(&player_id, &j).await;
                            }
                        }
                        continue;
                    }
                    // Interior portals (#165). Range-gated on the gateway's
                    // position cache like every other interaction, and
                    // gateway-owned because it is a ROUTING decision: only the
                    // gateway knows which zones exist and which one a player
                    // currently belongs to.
                    if data.get("type").and_then(|v| v.as_str()) == Some("portal.enter") {
                        self.apply_portal_enter(&player_id).await;
                        continue;
                    }
                    // The creature bounty (#161). Range-gated here on the
                    // gateway's position cache, exactly like `market.open` —
                    // standing next to the man who pays is a server-side fact,
                    // not something a client asserts.
                    if data.get("type").and_then(|v| v.as_str()) == Some("bounty.turn_in") {
                        let command_id =
                            data.get("command_id").and_then(|v| v.as_str()).unwrap_or("");
                        self.apply_bounty_turn_in(&player_id, command_id).await;
                        continue;
                    }
                    // `listing.*` (#142) share market.open's range gate.
                    if let Some(op) = data
                        .get("type")
                        .and_then(|v| v.as_str())
                        .and_then(|t| t.strip_prefix("listing."))
                        .filter(|op| matches!(*op, "place" | "buy" | "cancel"))
                    {
                        let op = op.to_string();
                        self.apply_listing_op(&player_id, &op, &data).await;
                        continue;
                    }
                    // `listing.list` (#142) is a stateless, filterable read.
                    if data.get("type").and_then(|v| v.as_str()) == Some("listing.list") {
                        if let Some(db) = self.db.clone() {
                            if let Some(MarketAt { id: market_id, .. }) = self.market_at(&db, &player_id).await {
                                self.send_listings(&player_id, &market_id, &data).await;
                            }
                        }
                        continue;
                    }
                    // `market.history_request` (#143): a stateless read of the
                    // derived candle cache.
                    if data.get("type").and_then(|v| v.as_str()) == Some("market.history_request") {
                        let item_id = data.get("item_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let days = data.get("days").and_then(|v| v.as_i64()).unwrap_or(7);
                        if let Some(db) = self.db.clone() {
                            if let Some(MarketAt { id: market_id, .. }) = self.market_at(&db, &player_id).await {
                                self.send_history(&player_id, &market_id, &item_id, days).await;
                            }
                        }
                        continue;
                    }
                    // `market.book_request` (#139): a stateless read of one
                    // commodity's depth, so the client can look at a book
                    // without waiting for someone else's trade to push one.
                    if data.get("type").and_then(|v| v.as_str()) == Some("market.book_request") {
                        let item_id = data.get("item_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        if let Some(db) = self.db.clone() {
                            if let Some(MarketAt { id: market_id, .. }) = self.market_at(&db, &player_id).await {
                                self.send_book(&player_id, &market_id, &item_id).await;
                            }
                        }
                        continue;
                    }
                    // `market.sell` / `market.buy` / `market.cancel` (#139)
                    // share market.open's range gate.
                    if let Some(op) = data
                        .get("type")
                        .and_then(|v| v.as_str())
                        .and_then(|t| t.strip_prefix("market."))
                        .filter(|op| matches!(*op, "sell" | "buy" | "cancel"))
                    {
                        let op = op.to_string();
                        self.apply_market_order(&player_id, &op, &data).await;
                        continue;
                    }
                    // `warehouse.*` (#138) share market.open's range gate.
                    if let Some(op) = data
                        .get("type")
                        .and_then(|v| v.as_str())
                        .and_then(|t| t.strip_prefix("warehouse."))
                        .filter(|op| *op == "deposit" || *op == "withdraw")
                    {
                        let op = op.to_string();
                        let item_id = data.get("item_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let qty = data.get("qty").and_then(|v| v.as_i64()).unwrap_or(0);
                        self.apply_warehouse_op(&player_id, &op, &item_id, qty).await;
                        continue;
                    }
                    // `road.cells_request` (#134) is a stateless read, same
                    // reasoning as `terrain.list`/`object.list`.
                    if data.get("type").and_then(|v| v.as_str()) == Some("road.cells_request") {
                        let order_id = data.get("order_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        self.send_road_cells(&player_id, &order_id).await;
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
                        .unwrap_or((identity_x, identity_y, identity.hp));
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

        // Order expiry sweep (#140): releases escrow from orders nobody came
        // back for.
        let me = self.clone();
        tokio::spawn(async move { me.sweep_expired_orders().await });

        // Station jobs (#167): ripen what is due. A sweep rather than a timer
        // per job, because `ready_at` is absolute — a job started before a
        // restart finishes on time with nothing to reschedule at boot.
        // Config sanity before anything can use it. A panic here is correct:
        // an overlapping pair is unfixable at runtime and silently wrong.
        self.check_station_spacing();

        let me = self.clone();
        tokio::spawn(async move { me.station_job_monitor().await });

        // ...and once, at boot, fail any job whose recipe or station has gone
        // from config. There is no hot reload here, so an edited file plus rows
        // that outlived it is a startup condition, not a runtime one.
        let me = self.clone();
        tokio::spawn(async move { me.fail_orphaned_station_jobs().await });

        // Price-history rollup (#143): ledger -> candles, off the trade path.
        let me = self.clone();
        tokio::spawn(async move { me.candle_rollup().await });

        // NPC provisioner (#154): keeps a floor and a ceiling standing so a
        // fresh server's book is never dead content, and reports trades that
        // escaped the band.
        let me = self.clone();
        tokio::spawn(async move { me.provisioner_refresh().await });

        // Warehouse storage billing (#155). A no-op unless an operator has set
        // a rate — the mechanism ships, the policy doesn't.
        let me = self.clone();
        tokio::spawn(async move { me.storage_billing().await });

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
        saved_zone: String::new(),
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
        saved_zone: ch.district,
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
    // Market tuning FIRST (#152). It's the cheapest thing that can refuse the
    // boot, so validating it before touching the database means a typo'd rate is
    // reported in milliseconds instead of after migrations, capital seeding and
    // book reconciliation have run. Nothing below depends on it, so this is
    // purely about failing fast and legibly.
    let market_cfg = load_market_config();

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
            // Market book reconciliation (#136 §8.3, issue #140). A HARD
            // failure, not a warning: every violation here means goods or gold
            // have been duplicated or destroyed, and a market that has
            // silently minted stock is far worse than one that won't start.
            // The money supply must equal purses plus escrow (#154). A gap
            // means some path created or destroyed gold without telling the
            // ledger — the precise class of bug the ledger exists to make
            // impossible. A WARNING rather than a panic: unlike a crossed book
            // this doesn't corrupt anyone's holdings, and a pre-#154 database
            // legitimately starts with an unexplained supply (nobody was
            // recording when those characters were created).
            match db.gold_supply_gap().await {
                Ok(0) => println!("[Proxy] Gold supply reconciled — ledger matches purses + escrow"),
                Ok(gap) => println!(
                    "[Proxy] NOTE: gold supply gap of {gap}g — expected on a database that                      predates the #154 ledger; new activity is fully recorded"
                ),
                Err(e) => eprintln!("[Proxy] gold supply check: {e}"),
            }
            match db.book_health().await {
                Ok(problems) if problems.is_empty() => {
                    println!("[Proxy] Market books reconciled — escrow matches the open book")
                }
                Ok(problems) => {
                    for p in &problems {
                        eprintln!("[Proxy] FATAL: {p}");
                    }
                    panic!("market book reconciliation failed ({} problem(s)) — refusing to start", problems.len());
                }
                Err(e) => panic!("market book reconciliation could not run: {e}"),
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

    let proxy =
        Proxy::new_with_market_config("127.0.0.1", 8766, 8764, 8767, db, market_cfg);
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

    /// The market tuning these tests assert against (#152): the values #136-#143
    /// shipped, matching what the test `Proxy` constructor installs. Deliberately
    /// NOT the repo's `market.toml` — a suite whose expected fees moved when
    /// someone tuned a live config file would be worse than no suite.
    /// A proxy with authored interiors (#165). `Proxy::new` reads the repo's
    /// real `zones.toml`; tests want a layout they control, for the same reason
    /// they don't read the live `market.toml`.
    fn test_proxy_with_zone_config(cfg: mmo::zone_config::ZoneConfig) -> Arc<Proxy> {
        let mut p = Proxy::new("127.0.0.1", 0, 0, 0, None);
        Arc::get_mut(&mut p).expect("not yet shared").zone_cfg = cfg;
        p
    }

    fn test_market_cfg() -> mmo::market_config::MarketConfig {
        mmo::market_config::MarketConfig::default()
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
            // Tests get the shipped defaults, never the repo's `market.toml`:
            // a suite whose expected fees changed when someone tuned a live
            // config file would be worse than no suite. Config LOADING is
            // covered by `market_config`'s own tests, and per-district
            // resolution by `market_rules_ride_on_market_opened`.
            market_cfg: mmo::market_config::MarketConfigSet::default(),
            zone_cfg: load_zone_config(),
            crafting_cfg: load_crafting_config(),
            tutorial_cfg: load_tutorial_config(),
            tutorial_counted: load_tutorial_config().counted_items(),
            tutorial_made: load_tutorial_config().made_items(),
            rent_reclaim_log: Mutex::new(VecDeque::new()),
            db_write_latencies_ms: Mutex::new(VecDeque::new()),
            terrain_edit_lock: tokio::sync::Mutex::new(()),
            world_objects: tokio::sync::OnceCell::new(),
            ability_cooldowns: Mutex::new(HashMap::new()),
            market_rate: Mutex::new(HashMap::new()),
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
                interior: false,
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
    fn cut_road_cells_splits_evenly_and_prices_cells_summing_to_the_total() {
        // 100m east then 200m south = 300m, an exact multiple of the 5m
        // cell length, at 75 stone total (matches
        // `road_plan_creates_a_length_costed_order_with_the_full_path`).
        let points = vec![(12800, 12800), (12900, 12800), (12900, 13000)];
        let cells = cut_road_cells(&points, 75);
        assert_eq!(cells.len(), 60, "300m / 5m cells");
        assert_eq!(cells[0].x0, 12800);
        assert_eq!(cells[0].y0, 12800);
        assert_eq!(cells[0].x1, 12805);
        assert_eq!(cells[0].y1, 12800);
        let last = cells.last().unwrap();
        assert_eq!((last.x1, last.y1), (12900, 13000), "last cell ends exactly on the path's end");
        let total: i64 = cells.iter().map(|c| parse_road_cell_stone(&c.required_json)).sum();
        assert_eq!(total, 75, "cell costs sum to exactly the road's total, no rounding drift");
        assert!(cells.iter().all(|c| parse_road_cell_stone(&c.required_json) >= 1), "no cell prices at zero");
    }

    #[test]
    fn cut_road_cells_handles_a_sub_cell_stub_as_one_whole_cell() {
        // A 4m stub is shorter than one 5m cell — the whole stub is cell 0,
        // pricing the ROAD_MIN_STONE floor (matches
        // `road_plan_validates_geometry_and_floors_the_cost`'s stub case).
        let points = vec![(12800, 12800), (12804, 12800)];
        let cells = cut_road_cells(&points, ROAD_MIN_STONE);
        assert_eq!(cells.len(), 1);
        assert_eq!((cells[0].x0, cells[0].y0, cells[0].x1, cells[0].y1), (12800, 12800, 12804, 12800));
        assert_eq!(parse_road_cell_stone(&cells[0].required_json), ROAD_MIN_STONE);
    }

    #[test]
    fn cut_road_cells_folds_the_remainder_length_into_a_short_last_cell() {
        // 12m: two full 5m cells plus a 2m remainder cell, still summing to
        // exactly the road's total cost.
        let points = vec![(12800, 12800), (12812, 12800)];
        let cells = cut_road_cells(&points, 5);
        assert_eq!(cells.len(), 3);
        assert_eq!((cells[2].x0, cells[2].x1), (12810, 12812), "the short remainder cell is 2m");
        let total: i64 = cells.iter().map(|c| parse_road_cell_stone(&c.required_json)).sum();
        assert_eq!(total, 5);
    }

    #[test]
    fn cut_road_cells_crosses_a_corner_with_a_straight_chord() {
        // 3m east then 5m south = 8m total, so the 5m cut lands 2m into the
        // second leg — the first cell (0-5m) straddles the corner and
        // becomes a diagonal chord between its own endpoints, not the
        // kinked L the original path took.
        let points = vec![(12800, 12800), (12803, 12800), (12803, 12805)];
        let cells = cut_road_cells(&points, 5);
        assert_eq!(cells.len(), 2);
        assert_eq!((cells[0].x0, cells[0].y0), (12800, 12800));
        assert_eq!((cells[0].x1, cells[0].y1), (12803, 12802), "chord ends 2m into the second leg");
        assert_ne!(cells[0].x0, cells[0].x1, "a corner-straddling cell isn't axis-aligned like either original run");
        assert_ne!(cells[0].y0, cells[0].y1);
        assert_eq!((cells[1].x1, cells[1].y1), (12803, 12805), "second cell ends exactly on the path's end");
        let total: i64 = cells.iter().map(|c| parse_road_cell_stone(&c.required_json)).sum();
        assert_eq!(total, 5, "cell costs still sum to the road's total across a corner");
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
        p.entity_state.lock().unwrap().insert("p1".into(), EntityCache { x: 650, y: 300, hp: 100, zone: "zone_a".into() });

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
            EntityCache { x: 4242, y: 1337, hp: 55, zone: "zone_a".into() },
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
            EntityCache { x: 1, y: 2, hp: 100, zone: "zone_a".into() },
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

    /// Park a player's cached position. Must be called right BEFORE each
    /// proximity-gated command: login inserts the character's saved position
    /// into the same cache *after* `welcome` is sent, so a one-time setup at
    /// the top of a test gets silently overwritten.
    fn stand_at(proxy: &Arc<Proxy>, pid: &str, x: i32, y: i32) {
        proxy.entity_state.lock().unwrap().insert(pid.to_string(), EntityCache { x, y, hp: 100, zone: "zone_a".into() });
    }

    /// Poll a road order's aggregate `progress_json` until it matches
    /// `want` or 2 seconds pass. A fixed sleep isn't enough here: these
    /// tests fire a burst of independent `build_contribute` sends without
    /// waiting on each one's own response, and how long the proxy's actor
    /// loop takes to actually work through that burst is load-dependent —
    /// under the full suite's parallel test load a flat `sleep(300ms)`
    /// occasionally lands before the last one lands (#133's road-cell
    /// tests hit exactly this).
    async fn poll_progress_json(db: &Db, order_id: &str, want: &str) {
        for _ in 0..40 {
            if db.build_order_by_id(order_id).await.unwrap().unwrap().progress_json == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "progress_json never reached {want} for {order_id}, got {:?}",
            db.build_order_by_id(order_id).await.unwrap().map(|o| o.progress_json)
        );
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
            EntityCache { x: tcx, y: tcy - 40, hp: 100, zone: "zone_a".into() },
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
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: x0, y: y0, hp: 100, zone: "zone_a".into() });

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

    /// Pickaxe is a normal recipe (mining/abilities epic #123, #116): same
    /// station+ingredients gate as any other craft.
    #[tokio::test]
    async fn pickaxe_recipe_crafts_at_a_station() {
        let (proxy, dbf, mut zone) = proxy_with_db().await;
        let db = Db::connect(dbf.url()).await.unwrap();
        db.seed_capital(&mmo::world::capital(), 0).await.unwrap();

        let mut ws = dial(&proxy).await;
        let (pid, bounds) = registered_with_plot(&proxy, &mut zone, &mut ws, "Smith").await;
        let (bx, by) = (bounds["x"].as_i64().unwrap() as i32, bounds["y"].as_i64().unwrap() as i32);

        // Stock exactly what the recipe asks for, read from the registry — a
        // balance pass (#129) retunes the costs without turning this test into a
        // false failure about ingredient amounts it isn't testing.
        let recipe = mmo::world::recipes()
            .into_iter()
            .find(|r| r.output_item == "pickaxe")
            .expect("the pickaxe recipe");
        for (item, qty) in recipe.inputs {
            zone.to_proxy.send(Message::Text(json!({
                "type": "gather_yield", "player_id": pid,
                "item_id": item, "qty": qty, "skill": "gathering", "xp": 1,
            }).to_string())).unwrap();
        }
        loop {
            let v = recv_frame(&mut ws).await.expect("expected the stocking pushes");
            if v["type"] == "inv.update" {
                let items = v["items"].as_array().cloned().unwrap_or_default();
                let stocked = recipe.inputs.iter().all(|(item, qty)| {
                    items.iter().any(|it| it["item_id"] == *item && it["qty"] == *qty)
                });
                if stocked {
                    break;
                }
            }
        }

        place_home_structure(&mut zone, &mut ws, &pid, "crafting", bx + 5, by + 5).await;

        zone.to_proxy.send(Message::Text(json!({
            "type": "craft_make", "player_id": pid, "recipe_id": "pickaxe",
        }).to_string())).unwrap();
        let made = loop {
            let v = recv_frame(&mut ws).await.expect("expected craft.made");
            if v["type"] == "craft.made" { break v; }
        };
        assert_eq!(made["item_id"], "pickaxe");
        assert_eq!(made["qty"], 1);

        drop(ws);
    }

    /// Equip/unequip (mining/abilities epic #123, #116; instanced in #128):
    /// arming a tool requires actually owning that SPECIFIC instance,
    /// grants the item's abilities on `equip.update` with a cooldown
    /// already scaled by the governing skill's level plus the instance's
    /// live durability, and the armed instance survives a reconnect (it's
    /// durable, not session state).
    #[tokio::test]
    async fn equip_requires_ownership_grants_abilities_and_persists() {
        let (proxy, _dbf, zone) = proxy_with_db().await;
        let email = format!("eq_{}@t.test", Uuid::new_v4().simple());

        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Miner"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();

        // No pickaxe yet: equipping some made-up instance id is rejected.
        ws.send(Message::Text(json!({"type": "equip", "instance_id": "nonexistent"}).to_string()))
            .await
            .unwrap();
        let err = recv_until(&mut ws, "equip_error").await;
        assert!(err["message"].as_str().unwrap().contains("don't have"), "unexpected: {err}");

        // Grant one mining level's worth of xp (2500 -> level 5) and a pickaxe,
        // as a foreman hand-out + some practice would. Drain both the
        // inventory AND skill pushes (not just the first) before the next
        // write — same idiom as the crafting test: firing straight into a
        // transaction that's still committing races SQLite into "database
        // is locked" rather than actually reordering anything. Capture the
        // granted instance's own id (#128) — equipping now targets it
        // specifically, since "the pickaxe" stops being unambiguous the
        // moment you could own more than one.
        zone.to_proxy.send(Message::Text(json!({
            "type": "gather_yield", "player_id": pid,
            "item_id": "pickaxe", "qty": 1, "skill": "mining", "xp": 2500,
        }).to_string())).unwrap();
        let (mut saw_inv, mut saw_skill, mut instance_id) = (false, false, String::new());
        while !(saw_inv && saw_skill) {
            let frame = recv_frame(&mut ws).await.expect("expected the grant's pushes");
            match frame["type"].as_str() {
                Some("inv.update") => {
                    saw_inv = true;
                    let items = frame["items"].as_array().unwrap();
                    let pick = items.iter().find(|i| i["item_id"] == "pickaxe").expect("granted pickaxe");
                    let fresh = mmo::world::tool_max_durability("pickaxe").unwrap();
                    assert_eq!(pick["durability"], fresh, "fresh instance starts at max durability");
                    assert_eq!(pick["max_durability"], fresh);
                    instance_id = pick["id"].as_str().unwrap().to_string();
                }
                Some("skill.update") => saw_skill = true,
                _ => {}
            }
        }

        ws.send(Message::Text(json!({"type": "equip", "instance_id": instance_id}).to_string()))
            .await
            .unwrap();
        let update = recv_until(&mut ws, "equip.update").await;
        assert_eq!(update["tool"], "pickaxe");
        let fresh = mmo::world::tool_max_durability("pickaxe").unwrap();
        assert_eq!(update["durability"], fresh);
        assert_eq!(update["max_durability"], fresh);
        let abilities = update["abilities"].as_array().unwrap();
        assert_eq!(abilities.len(), 1);
        assert_eq!(abilities[0]["id"], "pick");
        assert_eq!(
            abilities[0]["cooldown_ms"],
            mmo::world::ability_cooldown_ms("pick", 5),
            "level 5 shaves the swing cooldown"
        );
        assert!(
            mmo::world::ability_cooldown_ms("pick", 5) < mmo::world::ability_cooldown_ms("pick", 0),
            "and levelling must actually make it faster"
        );

        // Unequip clears the slot and the granted abilities with it.
        ws.send(Message::Text(json!({"type": "unequip"}).to_string())).await.unwrap();
        let cleared = recv_until(&mut ws, "equip.update").await;
        assert!(cleared["tool"].is_null());
        assert!(cleared["abilities"].as_array().unwrap().is_empty());

        // Re-arm, then reconnect: the tool is durable state, so login
        // hydration re-sends the same equip.update without re-equipping.
        ws.send(Message::Text(json!({"type": "equip", "instance_id": instance_id}).to_string()))
            .await
            .unwrap();
        recv_until(&mut ws, "equip.update").await;
        drop(ws);

        let mut ws2 = dial(&proxy).await;
        ws2.send(Message::Text(
            json!({"type": "login", "email": email, "password": "pw12"}).to_string(),
        ))
        .await
        .unwrap();
        recv_until(&mut ws2, "welcome").await;
        let rehydrated = recv_until(&mut ws2, "equip.update").await;
        assert_eq!(rehydrated["tool"], "pickaxe", "the armed tool survives a reconnect");
        drop(ws2);
    }

    /// A tool breaking (#128) auto-unequips through the FULL gateway path:
    /// the zone's `gather_yield` (carrying `ability_id`, exactly as a real
    /// swing sends it) drives `apply_gather_yield`'s wear-down, which pushes
    /// both an `inv.update` showing the broken husk and an `equip.update`
    /// with the ability gone. Wears it down directly against the shared DB
    /// first (all but one) so the test doesn't need a full tool's worth of real
    /// round trips — only the LAST, breaking swing goes through the real wire.
    #[tokio::test]
    async fn a_tool_swing_wears_it_down_and_breaking_auto_unequips() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": format!("wear_{}@t.test", Uuid::new_v4().simple()),
                   "password": "pw12", "name": "Worn"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();

        zone.to_proxy.send(Message::Text(json!({
            "type": "gather_yield", "player_id": pid,
            "item_id": "pickaxe", "qty": 1, "skill": "mining", "xp": 0,
        }).to_string())).unwrap();
        let instance_id = loop {
            let v = recv_frame(&mut ws).await.expect("expected the grant's inv.update");
            if v["type"] == "inv.update" {
                if let Some(pick) = v["items"].as_array().unwrap().iter().find(|i| i["item_id"] == "pickaxe") {
                    break pick["id"].as_str().unwrap().to_string();
                }
            }
        };
        ws.send(Message::Text(json!({"type": "equip", "instance_id": instance_id}).to_string()))
            .await
            .unwrap();
        // Login hydration may still have its own (tool: null) equip.update
        // in flight — loop past it to the one this equip actually produced.
        loop {
            if recv_until(&mut ws, "equip.update").await["tool"] == "pickaxe" {
                break;
            }
        }

        // All-but-one direct wears (bypassing the wire — this is gateway-side
        // plumbing, not re-proving the swing pipeline the earlier ability tests
        // cover). Count derived from the registry so a balance pass can retune
        // durability without breaking this.
        let fresh = mmo::world::tool_max_durability("pickaxe").unwrap();
        for _ in 0..(fresh - 1) {
            db.wear_equipped_tool(&pid, "tool", 1).await.unwrap();
        }
        assert_eq!(db.equipped(&pid, "tool").await.unwrap().as_deref(), Some("pickaxe"), "not broken yet");

        // The last, breaking swing — through the real internal message a
        // zone's apply_ability_swing actually sends.
        zone.to_proxy.send(Message::Text(json!({
            "type": "gather_yield", "player_id": pid,
            "item_id": "stone", "qty": 1, "skill": "mining", "xp": 12, "ability_id": "pick",
        }).to_string())).unwrap();

        let (mut saw_husk, mut saw_unequip) = (false, false);
        for _ in 0..40 {
            if saw_husk && saw_unequip { break; }
            let Some(v) = recv_frame(&mut ws).await else { break };
            match v["type"].as_str() {
                Some("inv.update") => {
                    if let Some(pick) = v["items"].as_array().unwrap().iter().find(|i| i["item_id"] == "pickaxe") {
                        if pick["durability"] == 0 {
                            saw_husk = true;
                        }
                    }
                }
                Some("equip.update") => {
                    if v["tool"].is_null() {
                        saw_unequip = true;
                    }
                }
                _ => {}
            }
        }
        assert!(saw_husk, "expected an inv.update showing the pickaxe at 0 durability");
        assert!(saw_unequip, "expected an equip.update with the tool cleared");
        assert_eq!(db.equipped(&pid, "tool").await.unwrap(), None);
        let items = db.inventory_for_character(&pid).await.unwrap();
        assert!(items.iter().any(|i| i.id == instance_id && i.durability == Some(0)),
            "the broken instance must still exist as a repairable husk");

        drop(ws);
    }

    /// Repairing over the real wire, at an owned crafting station, restores
    /// durability and consumes the (scaled) ingredient cost.
    #[tokio::test]
    async fn repair_wire_message_restores_durability_at_a_station() {
        let (proxy, db, dbf, mut zone) = proxy_with_shared_db().await;
        let db2 = Db::connect(dbf.url()).await.unwrap();
        db2.seed_capital(&mmo::world::capital(), 0).await.unwrap();
        let mut ws = dial(&proxy).await;
        let (pid, bounds) = registered_with_plot(&proxy, &mut zone, &mut ws, "Repairer").await;
        let (px, py) = (bounds["x"].as_i64().unwrap() as i32 + 2, bounds["y"].as_i64().unwrap() as i32 + 2);
        place_home_structure(&mut zone, &mut ws, &pid, "crafting", px, py).await;

        db.add_to_inventory(&pid, "pickaxe", 1).await.unwrap();
        let instance_id = db.inventory_for_character(&pid).await.unwrap()
            .into_iter().find(|i| i.item_id == "pickaxe").unwrap().id;
        db.equip_instance(&pid, &instance_id).await.unwrap();
        // Wear and cost both derived from the registry, so a balance pass
        // (#129) retunes durability and recipes without turning this into a
        // false failure about numbers it isn't testing.
        let fresh = mmo::world::tool_max_durability("pickaxe").unwrap();
        let missing = fresh * 2 / 3;
        for _ in 0..missing {
            db.wear_equipped_tool(&pid, "tool", 1).await.unwrap();
        }
        let cost = mmo::world::repair_cost("pickaxe", missing, fresh).unwrap();
        for (item, qty) in &cost {
            db.add_to_inventory(&pid, item, *qty).await.unwrap();
        }
        let want_wood = cost.iter().find(|(i, _)| *i == "wood").map(|(_, q)| *q).unwrap_or(0);
        while recv_frame(&mut ws).await.is_some() {
            // drain hydration/setup housekeeping — nothing here is asserted on.
            if db.inventory_qty(&pid, "wood").await.unwrap() == want_wood { break; }
        }

        ws.send(Message::Text(json!({"type": "repair", "instance_id": instance_id}).to_string()))
            .await
            .unwrap();
        let done = recv_until(&mut ws, "repair.done").await;
        assert_eq!(done["item_id"], "pickaxe");
        for (item, qty) in &cost {
            assert_eq!(done["cost"][item], *qty, "repair cost for {item}");
        }
        assert_eq!(db.inventory_qty(&pid, "wood").await.unwrap(), 0);
        assert_eq!(db.inventory_qty(&pid, "stone").await.unwrap(), 0);
        let repaired = db.inventory_for_character(&pid).await.unwrap()
            .into_iter().find(|i| i.id == instance_id).unwrap();
        assert_eq!(repaired.durability, Some(fresh));

        drop(ws);
    }

    /// Tools are blocked from storage entirely (#128) — a deposit attempt
    /// (even one the zone would otherwise have approved on proximity)
    /// leaves the instance exactly where it was.
    #[tokio::test]
    async fn store_deposit_rejects_tools() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": format!("nolaunder_{}@t.test", Uuid::new_v4().simple()),
                   "password": "pw12", "name": "NoLaunder"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();

        zone.to_proxy.send(Message::Text(json!({
            "type": "gather_yield", "player_id": pid,
            "item_id": "pickaxe", "qty": 1, "skill": "mining", "xp": 0,
        }).to_string())).unwrap();
        recv_until(&mut ws, "inv.update").await;

        zone.to_proxy.send(Message::Text(json!({
            "type": "store_op", "player_id": pid,
            "op": "deposit", "item_id": "pickaxe", "qty": 1,
        }).to_string())).unwrap();

        // Nothing should move — no store.update/inv.update reflecting a
        // deposit ever arrives; confirm directly against the DB instead of
        // racing an absence over the wire.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(db.inventory_qty(&pid, "pickaxe").await.unwrap(), 1, "still carried, not deposited");
        assert!(db.storage_for_character(&pid).await.unwrap().is_empty(), "nothing reached storage");

        drop(ws);
    }

    /// Read `from_proxy` values until one of type `ty` arrives, discarding
    /// anything else (registration/hydration housekeeping the zone doesn't
    /// care about in these tests) — same "assert in wire order, skip the
    /// rest" idiom as the client-side `recv_until`.
    async fn recv_value_until(rx: &mut mpsc::UnboundedReceiver<Value>, ty: &str) -> Value {
        loop {
            let v = recv_value(rx).await;
            if v.get("type").and_then(|x| x.as_str()) == Some(ty) {
                return v;
            }
        }
    }

    /// `ability.use` with no tool equipped (mining/abilities epic #123,
    /// #117) is rejected at the gateway — before the zone is ever involved,
    /// since only the gateway knows what's equipped.
    #[tokio::test]
    async fn ability_use_without_a_tool_is_rejected() {
        let (proxy, _dbf, mut zone) = proxy_with_db().await;
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": format!("ab_{}@t.test", Uuid::new_v4().simple()),
                   "password": "pw12", "name": "Barehands"}).to_string(),
        ))
        .await
        .unwrap();
        recv_until(&mut ws, "welcome").await;

        ws.send(Message::Text(
            json!({"type": "ability.use", "id": "pick", "node_id": "node_civic_rock_0"}).to_string(),
        ))
        .await
        .unwrap();
        let result = recv_until(&mut ws, "ability.result").await;
        assert_eq!(result["ok"], false);
        assert_eq!(result["reason"], "no_tool");

        // Nothing was forwarded to the zone — the gateway never got that far.
        while zone.from_proxy.try_recv().is_ok() {} // drain registration housekeeping
        assert!(zone.from_proxy.try_recv().is_err(), "no ability_swing should have been forwarded");
        drop(ws);
    }

    /// With a pickaxe armed, `ability.use` is accepted, the forwarded
    /// `ability_swing` carries a cooldown already scaled by the wielder's
    /// mining level, and firing again before that cooldown elapses is
    /// rejected server-side without a second forward — the client's hotbar
    /// sweep is a prediction, this is the enforcement.
    #[tokio::test]
    async fn ability_use_forwards_a_level_scaled_swing_and_enforces_cooldown() {
        let (proxy, _dbf, mut zone) = proxy_with_db().await;
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": format!("ab_{}@t.test", Uuid::new_v4().simple()),
                   "password": "pw12", "name": "Swinger"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();

        // Level 5 (2500 xp): cooldown should come out to 2000 - 80*5 = 1600ms.
        zone.to_proxy.send(Message::Text(json!({
            "type": "gather_yield", "player_id": pid,
            "item_id": "pickaxe", "qty": 1, "skill": "mining", "xp": 2500,
        }).to_string())).unwrap();
        let (mut saw_inv, mut saw_skill, mut instance_id) = (false, false, String::new());
        while !(saw_inv && saw_skill) {
            let frame = recv_frame(&mut ws).await.expect("expected the grant's pushes");
            match frame["type"].as_str() {
                // Login hydration may still push its own (empty) inv.update
                // around this point — only the one that actually carries the
                // pickaxe counts.
                Some("inv.update") => {
                    if let Some(pick) = frame["items"].as_array().unwrap().iter().find(|i| i["item_id"] == "pickaxe") {
                        saw_inv = true;
                        instance_id = pick["id"].as_str().unwrap().to_string();
                    }
                }
                Some("skill.update") => saw_skill = true,
                _ => {}
            }
        }
        ws.send(Message::Text(json!({"type": "equip", "instance_id": instance_id}).to_string()))
            .await
            .unwrap();
        recv_until(&mut ws, "equip.update").await;

        while zone.from_proxy.try_recv().is_ok() {} // drain registration/equip housekeeping

        ws.send(Message::Text(
            json!({"type": "ability.use", "id": "pick", "node_id": "node_civic_rock_0"}).to_string(),
        ))
        .await
        .unwrap();
        let swing = recv_value_until(&mut zone.from_proxy, "ability_swing").await;
        assert_eq!(swing["id"], "pick");
        assert_eq!(swing["node_id"], "node_civic_rock_0");
        assert_eq!(swing["player_id"], pid, "spoofable field must be the real id");
        assert_eq!(swing["cooldown_ms"], 2000 - 80 * 5, "level 5 shaves the cooldown");

        // Fire again immediately: rejected server-side, nothing forwarded.
        ws.send(Message::Text(
            json!({"type": "ability.use", "id": "pick", "node_id": "node_civic_rock_0"}).to_string(),
        ))
        .await
        .unwrap();
        let result = recv_until(&mut ws, "ability.result").await;
        assert_eq!(result["ok"], false);
        assert_eq!(result["reason"], "cooldown");
        assert!(zone.from_proxy.try_recv().is_err(), "a cooling-down swing must not forward");

        drop(ws);
    }

    /// The quarry foreman (mining/abilities epic #123, #118) hands over
    /// exactly one pickaxe the first time a character has none at all —
    /// then stops, whether they're holding it in their bags or in hand.
    #[tokio::test]
    async fn npc_interact_grants_a_pickaxe_only_while_the_character_has_none() {
        // Shared DB handle so "no second grant" is a deterministic query
        // against durable state, not a race against however many hydration
        // frames (equip.update, build.list, rent.status, ...) happen to
        // still be in flight when the assertion runs.
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": format!("nf_{}@t.test", Uuid::new_v4().simple()),
                   "password": "pw12", "name": "Newcomer"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();

        // First talk: no pickaxe anywhere -> granted, with the hand-out line.
        zone.to_proxy.send(Message::Text(json!({
            "type": "npc_interact", "player_id": pid, "npc_id": "npc_quarry_foreman",
        }).to_string())).unwrap();
        let dialogue = recv_until(&mut ws, "npc.dialogue").await;
        assert_eq!(dialogue["name"], "Sten");
        assert_eq!(dialogue["granted"], true);
        assert_eq!(db.inventory_qty(&pid, "pickaxe").await.unwrap(), 1);

        // Talk again, still carrying it: no second grant, and the mentoring
        // line (not the hand-out one) plays instead.
        zone.to_proxy.send(Message::Text(json!({
            "type": "npc_interact", "player_id": pid, "npc_id": "npc_quarry_foreman",
        }).to_string())).unwrap();
        let again = recv_until(&mut ws, "npc.dialogue").await;
        assert_eq!(again["granted"], false);
        assert_ne!(again["lines"], dialogue["lines"], "a returning visitor hears different lines");
        assert_eq!(db.inventory_qty(&pid, "pickaxe").await.unwrap(), 1, "no second grant");

        // Equipping is a pointer, not a move (#116) — the pickaxe stays
        // counted in the bag too. A third talk still grants nothing either
        // way, whether the "owned" or the "equipped" half of the check fires.
        // The shared DB handle gives the granted instance's id directly —
        // simpler than threading it through the inv.update the grant
        // already pushed (#128: equip now targets a specific instance).
        let instance_id = db.inventory_for_character(&pid).await.unwrap()
            .into_iter().find(|i| i.item_id == "pickaxe").unwrap().id;
        ws.send(Message::Text(json!({"type": "equip", "instance_id": instance_id}).to_string()))
            .await
            .unwrap();
        // Login hydration also pushes an `equip.update` (tool: null) — it
        // races the npc_interact replies above over a different async path
        // (the zone-listener task, not this client's), so it can still be
        // sitting unread here. Loop past it to the one this equip actually
        // produced (tool: "pickaxe").
        loop {
            if recv_until(&mut ws, "equip.update").await["tool"] == "pickaxe" {
                break;
            }
        }
        assert_eq!(db.equipped(&pid, "tool").await.unwrap().as_deref(), Some("pickaxe"));
        assert_eq!(db.inventory_qty(&pid, "pickaxe").await.unwrap(), 1, "equip doesn't move the item");
        zone.to_proxy.send(Message::Text(json!({
            "type": "npc_interact", "player_id": pid, "npc_id": "npc_quarry_foreman",
        }).to_string())).unwrap();
        let equipped_talk = recv_until(&mut ws, "npc.dialogue").await;
        assert_eq!(equipped_talk["granted"], false, "an equipped pick still counts as owned");
        assert_eq!(db.inventory_qty(&pid, "pickaxe").await.unwrap(), 1, "still no second grant");

        drop(ws);
    }

    /// The logging foreman (#126) grants exactly like Sten does — same rule,
    /// different NPC/item, proving `apply_npc_interact`'s generalization off
    /// `NpcSpawn.grants_item` actually works for a second NPC, not just the
    /// one it was extracted from.
    #[tokio::test]
    async fn npc_interact_grants_an_axe_from_the_logging_foreman_only_while_none() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": format!("lf_{}@t.test", Uuid::new_v4().simple()),
                   "password": "pw12", "name": "Lumberjack"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();

        zone.to_proxy.send(Message::Text(json!({
            "type": "npc_interact", "player_id": pid, "npc_id": "npc_logging_foreman",
        }).to_string())).unwrap();
        let dialogue = recv_until(&mut ws, "npc.dialogue").await;
        assert_eq!(dialogue["name"], "Elke");
        assert_eq!(dialogue["granted"], true);
        assert_eq!(db.inventory_qty(&pid, "axe").await.unwrap(), 1);

        zone.to_proxy.send(Message::Text(json!({
            "type": "npc_interact", "player_id": pid, "npc_id": "npc_logging_foreman",
        }).to_string())).unwrap();
        let again = recv_until(&mut ws, "npc.dialogue").await;
        assert_eq!(again["granted"], false);
        assert_ne!(again["lines"], dialogue["lines"], "a returning visitor hears different lines");
        assert_eq!(db.inventory_qty(&pid, "axe").await.unwrap(), 1, "no second grant");

        drop(ws);
    }

    // --- Foreman Marlow and the tutorial track (#169) -----------------------

    /// A proxy running a small, explicit tutorial rather than the shipped one,
    /// so these tests pin behaviour instead of today's authored numbers.
    async fn proxy_with_tutorial(toml: &str) -> (Arc<Proxy>, Arc<Db>, TestDb, FakeZone) {
        let dbf = TestDb::new();
        let db = Arc::new(Db::connect(dbf.url()).await.unwrap());
        let cfg = mmo::tutorial_config::TutorialConfig::parse(toml).expect("valid tutorial");
        // Installed BEFORE the Arc is shared. `register_zone` clones it, so
        // there is no later moment at which `Arc::get_mut` would succeed.
        let mut proxy = Proxy::new_with_market_config(
            "127.0.0.1", 0, 0, 0, Some(db.clone()),
            mmo::market_config::MarketConfigSet::default(),
        );
        {
            let m = Arc::get_mut(&mut proxy).expect("not yet shared");
            m.tutorial_counted = cfg.counted_items();
            m.tutorial_made = cfg.made_items();
            m.tutorial_cfg = cfg;
        }
        let zone = spawn_fake_zone().await;
        proxy
            .register_zone("zone_a".to_string(), zone.uri.clone(), 1, String::new(), Region::whole_world())
            .await;
        (proxy, db, dbf, zone)
    }

    const VALVE: &str = r#"
        [[handout]]
        npc = "npc_mine_foreman"
        item = "pickaxe"
        cooldown_secs = 600
        when = ["no_item pickaxe", "inventory_below iron_ore 1"]
        line = "Here. Try not to lose this one."

        [[handout]]
        npc = "npc_mine_foreman"
        item = "charcoal"
        qty = 10
        once = true
        when = ["gained iron_ore 2"]
        line = "Charcoal. The fire doesn't light itself."

        [[step]]
        id = "take_pickaxe"
        text = "Get a pickaxe"
        when = "has_item pickaxe"

        [[step]]
        id = "mine_clay"
        text = "Mine 4 clay"
        when = "gained clay_lump 4"

        [[reward]]
        item = "clay_lump"
        qty = 6
    "#;

    async fn a_player(
        proxy: &Arc<Proxy>,
    ) -> (WebSocketStream<MaybeTlsStream<TcpStream>>, String) {
        let mut ws = dial(proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": format!("tut_{}@t.test", Uuid::new_v4().simple()),
                   "password": "pw12", "name": "Learner"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();
        (ws, pid)
    }

    /// The valve opens for exactly the person it exists for: no pickaxe, no ore.
    /// Anyone else gets nothing, and that is the whole design — a handout that
    /// fired for someone with a pickaxe would be a tool tap.
    #[tokio::test]
    async fn the_pickaxe_valve_opens_only_with_no_pickaxe_and_no_ore() {
        let (proxy, db, _dbf, _zone) = proxy_with_tutorial(VALVE).await;
        let (_ws, pid) = a_player(&proxy).await;

        // Nothing in hand: it opens.
        proxy.apply_npc_interact(&pid, "npc_mine_foreman").await;
        assert_eq!(db.inventory_qty(&pid, "pickaxe").await.unwrap(), 1, "the valve opened");

        // EACH REFUSAL GETS ITS OWN FRESH PLAYER, deliberately. Reusing the one
        // above would let the 600-second cooldown be the reason nothing is
        // granted, and the test would pass with the conditions deleted — it did
        // exactly that until a sabotage run caught it.
        let (_w2, has_pick) = a_player(&proxy).await;
        db.add_to_inventory(&has_pick, "pickaxe", 1).await.unwrap();
        proxy.apply_npc_interact(&has_pick, "npc_mine_foreman").await;
        assert_eq!(
            db.inventory_qty(&has_pick, "pickaxe").await.unwrap(),
            1,
            "already has one, on a cooldown that has never fired: still no second"
        );

        // Ore but no pickaxe: STILL shut. Without this a player could mine a
        // stack, drop the pick and collect a fresh one forever.
        let (_w3, has_ore) = a_player(&proxy).await;
        db.add_to_inventory(&has_ore, "iron_ore", 3).await.unwrap();
        proxy.apply_npc_interact(&has_ore, "npc_mine_foreman").await;
        assert_eq!(
            db.inventory_qty(&has_ore, "pickaxe").await.unwrap(),
            0,
            "having ore means they had a pick recently — the valve stays shut"
        );
    }

    /// Arming your only pickaxe does not reopen the valve.
    ///
    /// Note WHAT MAKES THIS TRUE, because it is not the equipment lookup in
    /// `holds_equipped`: `equipment.instance_id` is a foreign key onto
    /// `inventory_item`, so an armed pickaxe necessarily still has a row and
    /// `no_item` fails on the count alone. Deleting the row to isolate the
    /// equipment check is impossible — the FK refuses.
    ///
    /// `holds_equipped` is therefore belt-and-braces rather than load-bearing
    /// today, and is kept for the case where equipping stops implying
    /// ownership. This test asserts the property players care about; it does
    /// not claim to prove which of the two mechanisms delivered it.
    #[tokio::test]
    async fn a_pickaxe_in_hand_still_closes_the_valve() {
        let (proxy, db, _dbf, _zone) = proxy_with_tutorial(VALVE).await;
        let (_ws, pid) = a_player(&proxy).await;
        db.add_to_inventory(&pid, "pickaxe", 1).await.unwrap();
        let inst = db.inventory_for_character(&pid).await.unwrap()
            .into_iter().find(|i| i.item_id == "pickaxe").unwrap();
        db.equip_instance(&pid, &inst.id).await.unwrap();
        assert_eq!(db.inventory_qty(&pid, "pickaxe").await.unwrap(), 1, "arming keeps the row");

        proxy.apply_npc_interact(&pid, "npc_mine_foreman").await;
        assert_eq!(
            db.inventory_qty(&pid, "pickaxe").await.unwrap(),
            1,
            "an armed pickaxe is still a pickaxe owned — no second one"
        );
    }

    /// Even at rock bottom, not on demand. The cooldown is the last line
    /// against a player who genuinely has nothing farming the safety net.
    #[tokio::test]
    async fn the_valve_respects_its_cooldown() {
        let (proxy, db, _dbf, _zone) = proxy_with_tutorial(VALVE).await;
        let (_ws, pid) = a_player(&proxy).await;

        proxy.apply_npc_interact(&pid, "npc_mine_foreman").await;
        assert_eq!(db.inventory_qty(&pid, "pickaxe").await.unwrap(), 1);
        db.remove_from_inventory(&pid, "pickaxe", 1).await.unwrap();

        // Conditions hold again — no pickaxe, no ore — but it is minutes early.
        proxy.apply_npc_interact(&pid, "npc_mine_foreman").await;
        assert_eq!(
            db.inventory_qty(&pid, "pickaxe").await.unwrap(),
            0,
            "inside the cooldown, deserving or not"
        );
    }

    /// The charcoal bundle is genuinely once, forever — not once per cooldown.
    #[tokio::test]
    async fn the_charcoal_bundle_is_one_time_ever() {
        let (proxy, db, _dbf, _zone) = proxy_with_tutorial(VALVE).await;
        let (_ws, pid) = a_player(&proxy).await;

        // Its condition is `gained iron_ore 2`, which is HISTORY: spending the
        // ore afterwards must not un-earn it.
        db.note_tutorial_event(&pid, "gained:iron_ore", 2, 1_000).await.unwrap();
        proxy.apply_npc_interact(&pid, "npc_mine_foreman").await;
        assert_eq!(db.inventory_qty(&pid, "charcoal").await.unwrap(), 10);

        for _ in 0..3 {
            proxy.apply_npc_interact(&pid, "npc_mine_foreman").await;
        }
        assert_eq!(db.inventory_qty(&pid, "charcoal").await.unwrap(), 10, "once, ever");
    }

    /// A step completed before ever meeting Marlow is already ticked when the
    /// track first arrives. This is the property the whole counter design
    /// exists for — there is no "tutorial started" event to have missed.
    #[tokio::test]
    async fn steps_done_before_meeting_marlow_arrive_already_ticked() {
        let (proxy, db, _dbf, _zone) = proxy_with_tutorial(VALVE).await;
        let (mut ws, pid) = a_player(&proxy).await;

        // Mine clay and buy a pickaxe without ever talking to anyone.
        db.add_to_inventory(&pid, "pickaxe", 1).await.unwrap();
        proxy.note_tutorial(&pid, "gained:clay_lump", 4).await;

        proxy.send_tutorial_state(&pid).await;
        let state = recv_until(&mut ws, "tutorial.state").await;
        let steps = state["steps"].as_array().unwrap();
        assert!(steps.iter().all(|s| s["done"].as_bool().unwrap()),
            "both steps should already be ticked: {steps:?}");
        assert_eq!(state["done"], json!(2));
    }

    /// Finishing pays out once, and only once.
    #[tokio::test]
    async fn finishing_the_track_rewards_exactly_once() {
        let (proxy, db, _dbf, _zone) = proxy_with_tutorial(VALVE).await;
        let (_ws, pid) = a_player(&proxy).await;
        db.add_to_inventory(&pid, "pickaxe", 1).await.unwrap();
        proxy.note_tutorial(&pid, "gained:clay_lump", 4).await;

        for _ in 0..4 {
            proxy.send_tutorial_state(&pid).await;
        }
        assert_eq!(
            db.inventory_qty(&pid, "clay_lump").await.unwrap(),
            6,
            "the reward lands once however often the track is re-evaluated"
        );
    }

    /// Guests get no durable progress and nothing panics. They have no
    /// inventory to hand anything into, so the handouts must simply not fire.
    #[tokio::test]
    async fn guests_get_nothing_and_nothing_breaks() {
        let (proxy, db, _dbf, _zone) = proxy_with_tutorial(VALVE).await;
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(json!({"type": "guest"}).to_string())).await.unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();

        proxy.apply_npc_interact(&pid, "npc_mine_foreman").await;
        proxy.note_tutorial(&pid, "gained:clay_lump", 4).await;
        proxy.send_tutorial_state(&pid).await;

        assert!(db.tutorial_counters(&pid).await.unwrap().is_empty(),
            "a guest leaves no durable progress");
        assert_eq!(db.inventory_qty(&pid, "pickaxe").await.unwrap(), 0);
    }

    /// The track can be skipped entirely with nothing locked behind it. The
    /// only thing a player who ignores Marlow misses is the reward.
    #[tokio::test]
    async fn ignoring_the_track_locks_nothing() {
        let (proxy, db, _dbf, _zone) = proxy_with_tutorial(VALVE).await;
        let (_ws, pid) = a_player(&proxy).await;

        // Never talk to Marlow, never look at the track — just play.
        db.add_to_inventory(&pid, "pickaxe", 1).await.unwrap();
        db.add_to_inventory(&pid, "iron_ore", 4).await.unwrap();
        db.add_to_inventory(&pid, "charcoal", 2).await.unwrap();
        db.load_station_fuel("f1", &pid, "charcoal", 1, 2, 1_000).await.unwrap();
        let mut recipe = mmo::crafting_config::StationRecipe {
            display_name: "Smelt".into(), tags: vec!["smelting".into()],
            skill: "smelting".into(), required_level: 0,
            inputs: vec![mmo::crafting_config::Ingredient { item: "iron_ore".into(), qty: 2 }],
            output_item: "iron_ingot".into(), output_qty: 1, fuel_units: 2,
            duration_ms: 1_000, xp: 10, failure_chance: 0.0,
            failure_xp_fraction: 0.5, catalyst: None, fee_multiplier: 1,
        };
        recipe.fuel_units = 2;
        let job = db
            .start_station_job("f1", &pid, 0, "iron_ingot", &recipe, 0, 1_000, false, None, 1_000)
            .await
            .unwrap()
            .expect("smelting works without the tutorial");
        db.ripen_station_jobs(2_000).await.unwrap();
        let got = db.collect_station_job(&job.id, &pid, 0, 0, 2_000).await.unwrap().unwrap();
        assert_eq!(got.payout, vec![("iron_ingot".to_string(), 1)],
            "the whole economy works for someone who never met Marlow");
    }

    /// Marlow stands where he can see what he is talking about, and outside
    /// the portal's reach so talking to him is never the same gesture as
    /// walking into the mine.
    #[test]
    fn marlow_stands_in_the_yard_but_clear_of_the_adit() {
        let marlow = mmo::world::npc("npc_mine_foreman").expect("Marlow should exist");
        let zones = load_zone_config();
        let mine = zones.interior("mine_starter").unwrap();
        let adit = mine.portals.first().unwrap();
        let d = (((marlow.x - adit.world.0).pow(2) + (marlow.y - adit.world.1).pow(2)) as f64).sqrt();
        assert!(d > 50.0, "inside the portal's reach ({d:.0})");
        assert!(d < 250.0, "too far from the yard he is meant to explain ({d:.0})");

        // He hands nothing over unconditionally — every grant of his is a valve
        // in tutorial.toml, and a static one here would bypass all of it.
        assert!(marlow.grants_item.is_none(), "Marlow's grants must all be conditional");
    }

    /// Each NPC's grant is scoped to its OWN item (#126) — talking to one
    /// never satisfies the other, and talking to both grants both.
    #[tokio::test]
    async fn npc_grants_are_scoped_to_each_npcs_own_item() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": format!("both_{}@t.test", Uuid::new_v4().simple()),
                   "password": "pw12", "name": "BothTools"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();

        // Talking to Sten first must not pre-empt Elke's axe grant, or vice versa.
        zone.to_proxy.send(Message::Text(json!({
            "type": "npc_interact", "player_id": pid, "npc_id": "npc_quarry_foreman",
        }).to_string())).unwrap();
        assert_eq!(recv_until(&mut ws, "npc.dialogue").await["granted"], true);

        zone.to_proxy.send(Message::Text(json!({
            "type": "npc_interact", "player_id": pid, "npc_id": "npc_logging_foreman",
        }).to_string())).unwrap();
        assert_eq!(recv_until(&mut ws, "npc.dialogue").await["granted"], true, "owning a pick must not block the axe grant");

        assert_eq!(db.inventory_qty(&pid, "pickaxe").await.unwrap(), 1);
        assert_eq!(db.inventory_qty(&pid, "axe").await.unwrap(), 1);
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
            EntityCache { x: 5000, y: 3000, hp: 100, zone: "zone_a".into() },
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
            EntityCache { x: 5200, y: 3000, hp: 100, zone: "zone_a".into() },
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
            EntityCache { x: 12800, y: 12800, hp: 100, zone: "zone_a".into() },
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
        proxy_with_market_config(mmo::market_config::MarketConfigSet::default()).await
    }

    /// `proxy_with_shared_db` with market tuning of the caller's choosing (#152).
    async fn proxy_with_market_config(
        cfg: mmo::market_config::MarketConfigSet,
    ) -> (Arc<Proxy>, Arc<Db>, TestDb, FakeZone) {
        let dbf = TestDb::new();
        let db = Arc::new(Db::connect(dbf.url()).await.unwrap());
        let proxy =
            Proxy::new_with_market_config("127.0.0.1", 0, 0, 0, Some(db.clone()), cfg);
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
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: dx, y: dy, hp: 100, zone: "zone_a".into() });
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
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: px as i32, y: py as i32, hp: 100, zone: "zone_a".into() });
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
    /// placement — and rejects them away from the path entirely. Since #133,
    /// a contribution lands on the single nearest CELL, not the pooled
    /// total, so finishing a road longer than one `BOARD_RANGE` reach takes
    /// contributing from more than one spot — proven here by walking from
    /// the far end back to the start. Completing the order broadcasts a
    /// structure that carries the full path (#96).
    #[tokio::test]
    async fn road_contributions_work_along_the_path_and_completion_carries_it() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let mut editor_ws = dial_editor(&proxy, &db).await;

        // 50m east then 100m south = 150m -> 37 stone across 30 5m cells —
        // deliberately under the 50-item carry cap so one hauler can carry
        // enough, but no single position reaches every cell.
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
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: 12850, y: 13250, hp: 100, zone: "zone_a".into() });
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
        // ~100m from the civic board): accepted, but lands on ONE cell —
        // the order's aggregate moves by exactly this contribution, not the
        // whole road's cost.
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: 12855, y: 12890, hp: 100, zone: "zone_a".into() });
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "build_contribute", "player_id": pid,
                "order_id": order_id, "item_id": "stone", "qty": 1,
            }).to_string()))
            .unwrap();
        let cell_msg = recv_until(&mut ws, "road.cell_progress").await;
        assert!(cell_msg["cell_index"].as_i64().unwrap() >= 10, "lands in run 2 (cells 10..30), not run 1");
        let progress = recv_until(&mut ws, "build.progress").await;
        assert_eq!(progress["progress"]["stone"].as_i64(), Some(1), "one cell's worth moved, not the whole road");

        // Keep feeding from the same spot: fills every cell reachable from
        // here (the rest of run 2), but run 1's cells — far from this
        // position — stay untouched, so the road isn't done yet.
        for _ in 0..40 {
            zone.to_proxy
                .send(Message::Text(json!({
                    "type": "build_contribute", "player_id": pid,
                    "order_id": order_id, "item_id": "stone", "qty": 3,
                }).to_string()))
                .unwrap();
        }
        // Run 2 has 20 cells (10..30); one (28) was already filled above,
        // 12 more cost 1 stone each and the last (29) absorbs the 8-stone
        // remainder — 1 + 12 + 8 = 21 once the burst above has landed.
        poll_progress_json(&db, &order_id, r#"{"stone":21}"#).await;
        assert_eq!(
            db.build_order_by_id(&order_id).await.unwrap().unwrap().state, "open",
            "run 1's cells are still out of reach from the far end"
        );

        // Walk to the path's START: the untouched cells near run 1 are now
        // in range, and finishing them completes the whole road.
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: 12800, y: 12800, hp: 100, zone: "zone_a".into() });
        for _ in 0..40 {
            zone.to_proxy
                .send(Message::Text(json!({
                    "type": "build_contribute", "player_id": pid,
                    "order_id": order_id, "item_id": "stone", "qty": 3,
                }).to_string()))
                .unwrap();
        }
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
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: 600, y: 600, hp: 100, zone: "zone_a".into() });
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

        // Contribute 10 of the 50 first — the move must carry it. Since
        // #133 a single contribution only lands on one ~5m cell (most of
        // this road's 40 cells cost 1 stone each), so reaching 10 takes 10
        // separate contributions — each one advances to whichever nearby
        // cell is still incomplete, same as a player walking a few metres
        // between drops in practice.
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "gather_yield", "player_id": pid,
                "item_id": "stone", "qty": 10, "skill": "gathering", "xp": 1,
            }).to_string()))
            .unwrap();
        recv_until(&mut player_ws, "inv.update").await;
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: 13050, y: 13000, hp: 100, zone: "zone_a".into() });
        for _ in 0..10 {
            zone.to_proxy
                .send(Message::Text(json!({
                    "type": "build_contribute", "player_id": pid,
                    "order_id": order_id, "item_id": "stone", "qty": 1,
                }).to_string()))
                .unwrap();
        }
        poll_progress_json(&db, &order_id, r#"{"stone":10}"#).await;

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
        // The 40m stub is only 8 cells, all within BOARD_RANGE of its
        // midpoint (#131/#132/#133) — but each contribution still only
        // lands on one cell at a time, so finishing it takes several.
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: 13420, y: 12600, hp: 100, zone: "zone_a".into() });
        for _ in 0..10 {
            zone.to_proxy
                .send(Message::Text(json!({
                    "type": "build_contribute", "player_id": pid,
                    "order_id": road_order, "item_id": "stone", "qty": 3,
                }).to_string()))
                .unwrap();
        }
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
        // Login hydration also pushes an (empty) `store.update` on connect —
        // loop past it rather than trusting the first one to be the refund.
        let items = loop {
            let storage = recv_until(&mut ws, "store.update").await;
            let items = storage["items"].as_array().cloned().unwrap_or_default();
            if items.iter().any(|it| it["item_id"] == "stone") {
                break items;
            }
        };
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
        // Each contribution lands on one cell (#131/#132/#133), so reaching
        // 12 stone takes 12 separate deposits from this spot.
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: 13500, y: 12500, hp: 100, zone: "zone_a".into() });
        for _ in 0..12 {
            zone.to_proxy
                .send(Message::Text(json!({
                    "type": "build_contribute", "player_id": pid,
                    "order_id": road_order, "item_id": "stone", "qty": 1,
                }).to_string()))
                .unwrap();
        }
        poll_progress_json(&db, &road_order, r#"{"stone":12}"#).await;

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
        // Wire order: refund first, then the completion announcements. Login
        // hydration also pushes an (empty) `store.update` on connect — loop
        // past it rather than trusting the first one to be the refund.
        let items = loop {
            let storage = recv_until(&mut ws, "store.update").await;
            let items = storage["items"].as_array().cloned().unwrap_or_default();
            if items.iter().any(|it| it["item_id"] == "stone") {
                break items;
            }
        };
        assert!(
            items.iter().any(|it| it["item_id"] == "stone" && it["qty"].as_i64() == Some(12)),
            "a part-built plan refunds its contributed progress (got {items:?})"
        );
        recv_until(&mut ws, "build.completed").await;
        assert!(db.build_order_by_id(&road_order).await.unwrap().is_none());

        drop(editor_ws);
        drop(ws);
    }

    /// A road order planned before #132 shipped (kind `road_*`, `path_json`
    /// set, but never given any `road_cell` rows — exactly what a road
    /// planned on an older build looks like) falls back to the ordinary
    /// pooled `build.contribute` path instead of silently refusing every
    /// contribution forever just because it predates per-cell tracking.
    #[tokio::test]
    async fn a_road_order_with_no_cells_falls_back_to_pooled_contribution() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let order = db
            .insert_build_order(
                "civic", "road_legacy", r#"{"stone":5}"#, "open", 0, None, 0,
                Some(mmo::persistence::BuildPlacement {
                    structure_kind: "dirt_road".to_string(), x: 100, y: 100, x1: Some(110), y1: Some(100),
                }),
                Some("[[100,100],[110,100]]"),
            )
            .await
            .unwrap();
        assert!(db.road_cells_for_order(&order.id).await.unwrap().is_empty(), "no cells, as a pre-#132 road would have");

        let email = format!("legacy_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Legacy"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "gather_yield", "player_id": pid,
                "item_id": "stone", "qty": 5, "skill": "gathering", "xp": 1,
            }).to_string()))
            .unwrap();
        recv_until(&mut ws, "inv.update").await;
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: 105, y: 100, hp: 100, zone: "zone_a".into() });

        // One pooled contribution of the whole 5 completes it outright —
        // proving this landed on the legacy `db.contribute` path, not the
        // per-cell one (which would have capped a single call far lower).
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "build_contribute", "player_id": pid,
                "order_id": order.id, "item_id": "stone", "qty": 5,
            }).to_string()))
            .unwrap();
        recv_until(&mut ws, "build.completed").await;
        assert_eq!(db.build_order_by_id(&order.id).await.unwrap().unwrap().state, "completed");

        drop(ws);
    }

    /// The Market (#137): it must be BUILT before it can be traded at, the
    /// range gate is enforced server-side (not merely hidden client-side), and
    /// the market id is the completed order's own — never something the client
    /// names, so it can't claim to be at a market it isn't at.
    #[tokio::test]
    async fn market_open_requires_a_built_market_in_range() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        // The real gateway seeds the authored capital at boot; the test
        // harness doesn't, so do it here to get the authored market order.
        db.seed_capital(&mmo::world::capital(), 0).await.unwrap();

        // The authored market order seeds `open` (unbuilt) at boot.
        let orders = db.build_orders_for_district("civic").await.unwrap();
        let market = orders.iter().find(|o| o.kind == "market").expect("the market is authored");
        assert_eq!(market.state, "open", "a fresh capital's market is unbuilt");
        let (mx, my) = (market.x.unwrap(), market.y.unwrap());

        let email = format!("trader_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Trader"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();

        // Standing right on the site, but nothing is built there yet: refused.
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: mx as i32, y: my as i32, hp: 100, zone: "zone_a".into() });
        ws.send(Message::Text(json!({"type": "market.open"}).to_string())).await.unwrap();
        let err = recv_until(&mut ws, "market.error").await;
        assert_eq!(err["code"].as_str().unwrap(), "out_of_range", "an unbuilt market can't be traded at");

        // Build it for real, through the ordinary contribution flow. Its
        // 80-unit cost exceeds `MAX_CARRY` (50), so it takes two trips — a
        // deliberate property of a civic build this size, not a test quirk.
        for (item, qty) in [("wood", 50), ("stone", 30)] {
            zone.to_proxy
                .send(Message::Text(json!({
                    "type": "gather_yield", "player_id": pid,
                    "item_id": item, "qty": qty, "skill": "gathering", "xp": 1,
                }).to_string()))
                .unwrap();
            recv_until(&mut ws, "inv.update").await;
            zone.to_proxy
                .send(Message::Text(json!({
                    "type": "build_contribute", "player_id": pid,
                    "order_id": market.id, "item_id": item, "qty": qty,
                }).to_string()))
                .unwrap();
        }
        recv_until(&mut ws, "build.completed").await;

        // Now in range of a BUILT market: accepted, and the id is the order's.
        ws.send(Message::Text(json!({"type": "market.open"}).to_string())).await.unwrap();
        let opened = recv_until(&mut ws, "market.opened").await;
        assert_eq!(opened["market_id"].as_str().unwrap(), market.id, "market id is the order's own id");
        assert_eq!((opened["x"].as_i64(), opened["y"].as_i64()), (Some(mx), Some(my)));

        // Walk out of range: refused again, even though it's built. Range is
        // enforced here, not merely used to hide the panel.
        proxy.entity_state.lock().unwrap().insert(
            pid.clone(), EntityCache { x: mx as i32 + test_market_cfg().range + 5, y: my as i32, hp: 100, zone: "zone_a".into() },
        );
        ws.send(Message::Text(json!({"type": "market.open"}).to_string())).await.unwrap();
        let err = recv_until(&mut ws, "market.error").await;
        assert_eq!(err["code"].as_str().unwrap(), "out_of_range");

        drop(ws);
    }

    /// `market.opened` carries the rates in force, and a `[districts.<id>]`
    /// override is actually CHARGED — not merely reported (#152).
    ///
    /// This is the test the whole issue exists for. The client previews a fee
    /// before you commit; while the rates were compile-time consts, a mirrored
    /// copy in `Protocol.gd` was sound. The moment they became per-district data
    /// that mirror became a LIE, and a quiet one — the panel would quote 3%
    /// while the server took 10%, and the player would only find out by being
    /// short-changed. So it is not enough for the wire to carry *some* numbers:
    /// what it carries has to be what the ledger records.
    #[tokio::test]
    async fn market_opened_carries_the_rates_that_are_actually_charged() {
        // A sale tax an order of magnitude off the default, so a stale mirror
        // couldn't coincidentally agree with it.
        let cfg = mmo::market_config::MarketConfigSet::parse(
            "[districts.civic]
sale_tax_num = 10
listing_fee_min_gold = 4
",
        )
        .unwrap();
        let (proxy, db, _dbf, zone) = proxy_with_market_config(cfg).await;

        let market = db
            .insert_build_order(
                "civic", "market", r#"{"wood":1}"#, "completed", 0, None, 0,
                Some(mmo::persistence::BuildPlacement {
                    structure_kind: "market".to_string(), x: 12800, y: 12800, x1: None, y1: None,
                }),
                None,
            )
            .await
            .unwrap();

        let email = format!("taxed_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Taxed"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();
        stand_at(&proxy, &pid, 12800, 12800);

        // 1. The wire reports the OVERRIDE, not the shipped default.
        ws.send(Message::Text(json!({"type": "market.open"}).to_string())).await.unwrap();
        let opened = recv_until(&mut ws, "market.opened").await;
        assert_eq!(opened["district"].as_str(), Some("civic"));
        let rules = &opened["rules"];
        assert_eq!(rules["sale_tax_num"].as_i64(), Some(10), "the override must be on the wire");
        assert_eq!(rules["sale_tax_den"].as_i64(), Some(100));
        assert_eq!(rules["listing_fee_min_gold"].as_i64(), Some(4));
        // Unstated keys still come through, from the resolved defaults.
        assert_eq!(rules["price_tick_gold"].as_i64(), Some(1));
        assert_eq!(rules["max_open_orders"].as_i64(), Some(40));

        // 2. And the ledger agrees. A seller rests 10 wood at 10g; a buyer
        //    crosses it. Tax is 10% of the 100g fill, and the listing fee floor
        //    is 4g, both per the override.
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "gather_yield", "player_id": pid,
                "item_id": "wood", "qty": 10, "skill": "gathering", "xp": 1,
            }).to_string()))
            .unwrap();
        // Loop until the payload reflects what WE caused: login pushes an
        // empty inv.update of its own, and grabbing that one instead is a
        // hydration race this suite has been bitten by repeatedly.
        loop {
            recv_until(&mut ws, "inv.update").await;
            if qty_of_inventory(&db, &pid, "wood").await >= 10 { break; }
        }
        stand_at(&proxy, &pid, 12800, 12800);
        ws.send(Message::Text(
            json!({"type": "warehouse.deposit", "item_id": "wood", "qty": 10}).to_string(),
        ))
        .await
        .unwrap();
        loop {
            let st = recv_until(&mut ws, "warehouse.state").await;
            if st["items"].as_array().map(|a| !a.is_empty()).unwrap_or(false) { break; }
        }

        let seller_before = db.character_gold(&pid).await.unwrap();
        stand_at(&proxy, &pid, 12800, 12800);
        ws.send(Message::Text(json!({
            "type": "market.sell", "item_id": "wood", "unit_price": 10, "qty": 10,
            "command_id": "cfg-sell-1",
        }).to_string()))
        .await
        .unwrap();
        recv_until(&mut ws, "market.fees").await;

        // Listing fee: 1% of 100 = 1, floored UP to the override's 4.
        let after_listing = db.character_gold(&pid).await.unwrap();
        assert_eq!(
            seller_before - after_listing, 4,
            "listing fee should be the override's 4g floor, not the default 1g"
        );

        // A second character crosses the ask.
        let buyer_email = format!("buyer_{}@t.test", Uuid::new_v4().simple());
        let mut bws = dial(&proxy).await;
        bws.send(Message::Text(
            json!({"type": "register", "email": buyer_email, "password": "pw12", "name": "Buyer"}).to_string(),
        ))
        .await
        .unwrap();
        let bid = recv_until(&mut bws, "welcome").await["player_id"].as_str().unwrap().to_string();
        stand_at(&proxy, &bid, 12800, 12800);
        bws.send(Message::Text(json!({
            "type": "market.buy", "item_id": "wood", "unit_price": 10, "qty": 10,
            "command_id": "cfg-buy-1",
        }).to_string()))
        .await
        .unwrap();
        recv_until(&mut bws, "market.fees").await;

        // The trade ledger is the authority: 10% of the 100g fill, not 3%.
        let trades = db.recent_trades(&market.id, "wood", 10).await.unwrap();
        let t = trades.first().expect("the cross should have traded");
        assert_eq!(t.unit_price * t.qty, 100);
        assert_eq!(
            t.sale_tax_gold, 10,
            "the ledger must record the override's 10% tax, not the shipped 3%"
        );
        // Seller nets the fill less that same tax.
        let seller_end = db.character_gold(&pid).await.unwrap();
        assert_eq!(seller_end, after_listing + 100 - 10);

        drop(ws);
        drop(bws);
    }

    /// Finishing the capital's market unlocks the Market District's (#153), and
    /// the announcement reaches the district where the new order actually
    /// APPEARED — not just the one where the build finished.
    ///
    /// This is a cross-district prereq, the first in the game: every earlier
    /// dependent lived in the same district as its prerequisite, so
    /// `build.completed` announcing only to the completing order's district was
    /// indistinguishable from correct. Here it isn't — a player standing 8.6km
    /// east would have been left staring at a board that never mentioned the
    /// order that had just opened in front of them.
    #[tokio::test]
    async fn completing_the_capital_market_unlocks_the_market_district_one() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        db.seed_capital(&mmo::world::capital(), 0).await.unwrap();

        // The authored pair: the capital's opens, the Market District's is
        // locked behind it.
        let capital = db
            .build_orders_for_district("civic")
            .await
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "market")
            .expect("the capital market is authored");
        assert_eq!(capital.state, "open");
        let remote = db
            .build_orders_for_district("market")
            .await
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "market_east")
            .expect("the Market District's market is authored");
        assert_eq!(remote.state, "locked", "the second market starts gated behind the first");

        let email = format!("unlocker_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Unlocker"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();

        // Build the capital's market through the ordinary contribution flow.
        for (item, qty) in [("wood", 50), ("stone", 30)] {
            zone.to_proxy
                .send(Message::Text(json!({
                    "type": "gather_yield", "player_id": pid,
                    "item_id": item, "qty": qty, "skill": "gathering", "xp": 1,
                }).to_string()))
                .unwrap();
            recv_until(&mut ws, "inv.update").await;
            zone.to_proxy
                .send(Message::Text(json!({
                    "type": "build_contribute", "player_id": pid,
                    "order_id": capital.id, "item_id": item, "qty": qty,
                }).to_string()))
                .unwrap();
        }
        recv_until(&mut ws, "build.completed").await;

        // Durably unlocked.
        let remote_now = db
            .build_orders_for_district("market")
            .await
            .unwrap()
            .into_iter()
            .find(|o| o.kind == "market_east")
            .unwrap();
        assert_eq!(remote_now.state, "open", "finishing the first market should open the second");

        // And announced where it appeared: a board push carrying the Market
        // District's order. Under the pre-#153 code only the civic board was
        // refreshed, and civic's list never contains `market_east`.
        let mut saw_remote_board = false;
        for _ in 0..40 {
            let msg = recv_until(&mut ws, "build.list").await;
            if msg["orders"]
                .as_array()
                .map(|a| a.iter().any(|o| o["kind"].as_str() == Some("market_east")))
                .unwrap_or(false)
            {
                saw_remote_board = true;
                break;
            }
        }
        assert!(
            saw_remote_board,
            "the Market District's board was never refreshed — its players would not see the \
             order that just opened there"
        );

        drop(ws);
    }

    /// The bounty over the wire (#161): range-gated server-side, paid, and
    /// reported either way.
    #[tokio::test]
    async fn the_bounty_is_range_gated_and_pays_over_the_wire() {
        let (proxy, db, _dbf, _zone) = proxy_with_shared_db().await;
        let email = format!("bounty_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Claimer"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();
        let cfg = mmo::market_config::BountyConfig::default();
        db.add_to_inventory(&pid, &cfg.item_id, cfg.required).await.unwrap();

        // Standing anywhere else: refused server-side, and nothing is consumed.
        stand_at(&proxy, &pid, 12800, 12800);
        ws.send(Message::Text(
            json!({"type": "bounty.turn_in", "command_id": "far"}).to_string(),
        ))
        .await
        .unwrap();
        let err = recv_until(&mut ws, "bounty.error").await;
        assert_eq!(err["code"], "out_of_range");
        assert_eq!(
            qty_of_inventory(&db, &pid, &cfg.item_id).await,
            cfg.required,
            "a refused turn-in took the trophies anyway"
        );

        // Standing with him: paid.
        let (wx, wy) = mmo::world::WEAPON_MASTER_AT;
        stand_at(&proxy, &pid, wx, wy);
        let purse = db.character_gold(&pid).await.unwrap();
        ws.send(Message::Text(
            json!({"type": "bounty.turn_in", "command_id": "near"}).to_string(),
        ))
        .await
        .unwrap();
        let state = recv_until(&mut ws, "bounty.state").await;
        assert_eq!(state["paid"], cfg.gold);
        assert_eq!(state["held"], 0);
        assert_eq!(state["required"], cfg.required);
        assert_eq!(db.character_gold(&pid).await.unwrap(), purse + cfg.gold);
        assert_eq!(qty_of_inventory(&db, &pid, &cfg.item_id).await, 0);
        drop(ws);
    }

    /// Talking to the weapon master REPORTS the bounty; it never claims it.
    /// Conflating the two would mean walking up to him silently spends ten pelts
    /// the moment you have them.
    #[tokio::test]
    async fn talking_to_the_weapon_master_reports_but_never_claims() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let email = format!("chat_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Chatter"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();
        let cfg = mmo::market_config::BountyConfig::default();
        // Deliberately holding MORE than enough: if talking claimed, it would.
        db.add_to_inventory(&pid, &cfg.item_id, cfg.required).await.unwrap();
        let purse = db.character_gold(&pid).await.unwrap();

        zone.to_proxy
            .send(Message::Text(json!({
                "type": "npc_interact", "player_id": pid, "npc_id": "npc_weapon_master",
            }).to_string()))
            .unwrap();

        let state = recv_until(&mut ws, "bounty.state").await;
        assert_eq!(state["held"], cfg.required, "the offer should show what they hold");
        assert_eq!(state["paid"], 0, "talking must not pay");
        assert_eq!(db.character_gold(&pid).await.unwrap(), purse, "talking minted gold");
        assert_eq!(
            qty_of_inventory(&db, &pid, &cfg.item_id).await,
            cfg.required,
            "talking consumed trophies"
        );
        drop(ws);
    }

    /// An ordinary NPC advertises no bounty at all.
    #[tokio::test]
    async fn an_ordinary_npc_offers_no_bounty() {
        let (proxy, _db, _dbf, zone) = proxy_with_shared_db().await;
        let email = format!("plain_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Plain"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();

        zone.to_proxy
            .send(Message::Text(json!({
                "type": "npc_interact", "player_id": pid, "npc_id": "npc_logging_foreman",
            }).to_string()))
            .unwrap();
        recv_until(&mut ws, "npc.dialogue").await;
        for _ in 0..15 {
            let Some(v) = recv_frame(&mut ws).await else { break };
            assert_ne!(v["type"], "bounty.state", "the logging foreman offered a dog bounty");
        }
        drop(ws);
    }

    /// Both slots on the wire at once (#160). Two slots means `equip.update` has
    /// to carry both without either clobbering the other — a client reading
    /// `tool` and one reading `weapon` are looking at different equipment.
    #[tokio::test]
    async fn a_tool_and_a_weapon_occupy_separate_slots() {
        let (proxy, db, _dbf, _zone) = proxy_with_shared_db().await;
        let email = format!("armed_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Armed"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();

        db.add_to_inventory(&pid, "pickaxe", 1).await.unwrap();
        db.add_to_inventory(&pid, "sword", 1).await.unwrap();
        let inv = db.inventory_for_character(&pid).await.unwrap();
        let pick = inv.iter().find(|i| i.item_id == "pickaxe").unwrap().id.clone();
        let sword = inv.iter().find(|i| i.item_id == "sword").unwrap().id.clone();

        // Equipping one must not disturb the other, in either order.
        db.equip_instance(&pid, &pick).await.unwrap();
        db.equip_instance(&pid, &sword).await.unwrap();
        assert_eq!(db.equipped(&pid, "tool").await.unwrap().as_deref(), Some("pickaxe"));
        assert_eq!(db.equipped(&pid, "weapon").await.unwrap().as_deref(), Some("sword"));

        ws.send(Message::Text(json!({"type": "equip", "instance_id": sword}).to_string()))
            .await
            .unwrap();
        let update = loop {
            let v = recv_until(&mut ws, "equip.update").await;
            if v["weapon"] == "sword" {
                break v;
            }
        };
        assert_eq!(update["tool"], "pickaxe", "arming a sword unequipped the pickaxe");
        assert_eq!(update["weapon"], "sword");
        let fresh = mmo::world::tool_max_durability("sword").unwrap();
        assert_eq!(update["weapon_durability"], fresh);
        assert_eq!(update["weapon_max_durability"], fresh);
        assert_eq!(update["melee_damage"], mmo::world::melee_damage(Some("sword")));
        // The tool's own ability is untouched by the weapon in the other hand.
        assert_eq!(update["abilities"][0]["id"], "pick");
        drop(ws);
    }

    /// A connecting swing wears the blade, and breaking it auto-unequips (#128's
    /// contract) AND drops the swing back to bare-handed damage — a broken sword
    /// must stop hitting like one immediately, not at the next periodic sweep.
    #[tokio::test]
    async fn a_sword_wears_on_connecting_swings_and_breaking_disarms_you() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let email = format!("blunt_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Blunt"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();
        db.add_to_inventory(&pid, "sword", 1).await.unwrap();
        let sword = db
            .inventory_for_character(&pid)
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.item_id == "sword")
            .unwrap()
            .id;
        db.equip_instance(&pid, &sword).await.unwrap();
        let fresh = mmo::world::tool_max_durability("sword").unwrap();

        // One connecting swing, as the zone reports it.
        zone.to_proxy
            .send(Message::Text(
                json!({"type": "weapon_used", "player_id": pid}).to_string(),
            ))
            .unwrap();
        loop {
            let v = recv_until(&mut ws, "equip.update").await;
            if v["weapon_durability"] == fresh - 1 {
                break;
            }
        }

        // Wear it to the brink directly, then break it over the wire.
        for _ in 0..(fresh - 2) {
            db.wear_equipped_tool(&pid, "weapon", 1).await.unwrap();
        }
        assert_eq!(
            db.equipped(&pid, "weapon").await.unwrap().as_deref(),
            Some("sword"),
            "not broken yet"
        );
        zone.to_proxy
            .send(Message::Text(
                json!({"type": "weapon_used", "player_id": pid}).to_string(),
            ))
            .unwrap();

        let broken = loop {
            let v = recv_until(&mut ws, "equip.update").await;
            if v["weapon"].is_null() {
                break v;
            }
        };
        assert_eq!(db.equipped(&pid, "weapon").await.unwrap(), None, "auto-unequipped");
        assert_eq!(
            broken["melee_damage"],
            mmo::world::MELEE_DAMAGE_BARE,
            "a broken sword must stop hitting like a sword at once"
        );
        // The husk survives, repairable, exactly as a broken tool does (#128).
        let husk = db
            .inventory_for_character(&pid)
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.id == sword)
            .expect("the broken sword should still exist");
        assert_eq!(husk.durability, Some(0));
        drop(ws);
    }

    /// Bare-handed swings wear nothing. Fists don't blunt, and a player between
    /// swords must not be silently punished for having none.
    #[tokio::test]
    async fn a_bare_handed_swing_wears_nothing() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let email = format!("fists_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Fists"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();
        // A sword in the pack but NOT in hand: wearing must key on the slot, not
        // on ownership.
        db.add_to_inventory(&pid, "sword", 1).await.unwrap();

        for _ in 0..5 {
            zone.to_proxy
                .send(Message::Text(
                    json!({"type": "weapon_used", "player_id": pid}).to_string(),
                ))
                .unwrap();
        }
        // Give the gateway a chance to do the wrong thing.
        for _ in 0..10 {
            let _ = recv_frame(&mut ws).await;
        }
        let carried = db
            .inventory_for_character(&pid)
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.item_id == "sword")
            .unwrap();
        assert_eq!(
            carried.durability,
            mmo::world::tool_max_durability("sword"),
            "an unequipped sword was worn down by punching"
        );
        drop(ws);
    }

    /// The weapon master hands over a blade only when you have none at all —
    /// the same "safety net, not a farm" contract the foremen have, so losing
    /// your sword is never a dead end and dropping one is never a farm.
    #[tokio::test]
    async fn the_weapon_master_arms_you_only_when_you_have_nothing() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let email = format!("bram_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Pupil"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();
        let (wx, wy) = mmo::world::WEAPON_MASTER_AT;
        stand_at(&proxy, &pid, wx, wy);

        // The ZONE range-gates `npc.talk` and forwards `npc_interact`; the fake
        // zone here can't, so drive the internal message the real one sends.
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "npc_interact", "player_id": pid, "npc_id": "npc_weapon_master",
            }).to_string()))
            .unwrap();
        loop {
            recv_until(&mut ws, "npc.dialogue").await;
            if qty_of_inventory(&db, &pid, "sword").await >= 1 {
                break;
            }
        }
        assert_eq!(qty_of_inventory(&db, &pid, "sword").await, 1);

        // Talking again while armed hands over nothing.
        stand_at(&proxy, &pid, wx, wy);
        // The ZONE range-gates `npc.talk` and forwards `npc_interact`; the fake
        // zone here can't, so drive the internal message the real one sends.
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "npc_interact", "player_id": pid, "npc_id": "npc_weapon_master",
            }).to_string()))
            .unwrap();
        recv_until(&mut ws, "npc.dialogue").await;
        for _ in 0..10 {
            let _ = recv_frame(&mut ws).await;
        }
        assert_eq!(
            qty_of_inventory(&db, &pid, "sword").await,
            1,
            "the weapon master is a safety net, not a sword dispenser"
        );
        drop(ws);
    }

    // --- interior zones (#165) ----------------------------------------------

    fn a_mine_config() -> mmo::zone_config::ZoneConfig {
        mmo::zone_config::ZoneConfig::parse(
            r#"
            [interior.mine_test]
            display_name = "Test Cut"
            spawn_anchor = [20, 30]
            [[interior.mine_test.volumes]]
            x0 = 0
            y0 = 0
            x1 = 80
            y1 = 60
            [[interior.mine_test.portals]]
            id = "adit"
            world = [12800, 13500]
            inside = [20, 30]
            radius = 40
            "#,
        )
        .unwrap()
    }

    // --- Stations: the gateway-side range gate (#167) -----------------------

    /// A surface furnace and an interior one at the SAME local coordinates.
    /// That collision is the point: since #165 a position without a zone means
    /// nothing, and these tests exist to prove the gate knows the difference.
    fn a_station_world() -> (mmo::zone_config::ZoneConfig, mmo::crafting_config::CraftingConfig) {
        let zones = mmo::zone_config::ZoneConfig::parse(
            r#"
            [interior.mine_test]
            display_name = "Test Cut"
            spawn_anchor = [20, 30]
            [[interior.mine_test.volumes]]
            x0 = 0
            y0 = 0
            x1 = 200
            y1 = 200
            [[interior.mine_test.portals]]
            id = "adit"
            world = [12800, 13500]
            inside = [20, 30]
            radius = 40

            [[station]]
            id = "yard_furnace"
            type = "furnace"
            pos = [100, 100]

            [[station]]
            id = "deep_wheel"
            type = "wheel"
            pos = [100, 100]
            interior = "mine_test"
            "#,
        )
        .unwrap();
        let crafting = mmo::crafting_config::CraftingConfig::parse(
            r#"
            [station.furnace]
            display_name = "Furnace"
            kind = "heat"
            recipe_tags = ["smelting"]
            radius = 40
            [station.furnace.fuels]
            charcoal = 2

            [station.wheel]
            display_name = "Potter's Wheel"
            kind = "shaping"
            recipe_tags = ["pottery"]
            radius = 40

            [recipe.iron_ingot]
            display_name = "Smelt Iron Ingot"
            tags = ["smelting"]
            skill = "smelting"
            output_item = "iron_ingot"
            fuel_units = 2
            duration_ms = 12000
            [[recipe.iron_ingot.inputs]]
            item = "iron_ore"
            qty = 2

            [recipe.clay_pot]
            display_name = "Clay Pot"
            tags = ["pottery"]
            skill = "pottery"
            output_item = "stone"
            duration_ms = 5000
            [[recipe.clay_pot.inputs]]
            item = "clay_lump"
            qty = 1
            "#,
        )
        .unwrap();
        (zones, crafting)
    }

    fn test_proxy_with_stations() -> Arc<Proxy> {
        let (zones, crafting) = a_station_world();
        let mut p = Proxy::new("127.0.0.1", 0, 0, 0, None);
        {
            let m = Arc::get_mut(&mut p).expect("not yet shared");
            m.zone_cfg = zones;
            m.crafting_cfg = crafting;
        }
        add_zone_region(&p, "zone_a", Region { x0: 0, y0: 0, x1: 25600, y1: 25600 });
        add_zone_region(&p, "mine_test", Region { x0: 0, y0: 0, x1: 200, y1: 200 });
        p.zones.lock().unwrap().get_mut("mine_test").unwrap().interior = true;
        p
    }

    fn stand(p: &Proxy, pid: &str, x: i32, y: i32, zone: &str) {
        p.entity_state.lock().unwrap().insert(
            pid.to_string(),
            EntityCache { x, y, hp: 100, zone: zone.to_string() },
        );
    }

    /// The gate is a radius, and it is checked on the gateway's own cache
    /// rather than on anything a client asserts.
    #[test]
    fn a_station_is_only_found_from_inside_its_radius() {
        let p = test_proxy_with_stations();
        stand(&p, "u", 100, 100, "zone_a");
        assert_eq!(p.station_at("u").map(|(s, _)| s.id.clone()), Some("yard_furnace".into()));

        stand(&p, "u", 130, 100, "zone_a"); // 30 away, radius 40
        assert!(p.station_at("u").is_some(), "just inside");

        stand(&p, "u", 150, 100, "zone_a"); // 50 away
        assert!(p.station_at("u").is_none(), "outside the radius");
    }

    /// **The #165 lesson, applied to stations.** Two stations sit at identical
    /// coordinates — one in the yard, one underground. A player at (100, 100) is
    /// standing at exactly one of them, and which one depends entirely on the
    /// zone. Matching on position alone would let someone smelt from inside the
    /// rock, or throw a pot from the surface.
    #[test]
    fn the_same_coordinates_underground_are_a_different_station() {
        let p = test_proxy_with_stations();

        stand(&p, "u", 100, 100, "zone_a");
        assert_eq!(
            p.station_at("u").map(|(s, _)| s.id.clone()),
            Some("yard_furnace".into()),
            "on the surface you are at the furnace"
        );

        stand(&p, "u", 100, 100, "mine_test");
        assert_eq!(
            p.station_at("u").map(|(s, _)| s.id.clone()),
            Some("deep_wheel".into()),
            "the very same coordinates underground are the wheel, not the furnace"
        );
    }

    /// An interior station is unreachable from a DIFFERENT interior, not merely
    /// from the surface — the check is zone identity, not an is-interior flag.
    #[test]
    fn an_interior_station_belongs_to_exactly_one_interior() {
        let p = test_proxy_with_stations();
        add_zone_region(&p, "other_cave", Region { x0: 0, y0: 0, x1: 200, y1: 200 });
        p.zones.lock().unwrap().get_mut("other_cave").unwrap().interior = true;

        stand(&p, "u", 100, 100, "other_cave");
        assert!(
            p.station_at("u").is_none(),
            "the wheel is in mine_test; standing at the same spot in another cave is not being at it"
        );
    }

    /// A station only offers what its tags accept. The furnace makes ingots and
    /// the wheel makes pots, from one recipe table — that filter is the whole
    /// reason there is one station type rather than four.
    #[test]
    fn a_station_offers_only_the_recipes_its_tags_accept() {
        let (_, crafting) = a_station_world();
        let furnace = crafting.station("furnace").unwrap();
        let wheel = crafting.station("wheel").unwrap();

        let at_furnace: Vec<&str> =
            crafting.recipes_for(furnace).iter().map(|(id, _)| id.as_str()).collect();
        let at_wheel: Vec<&str> =
            crafting.recipes_for(wheel).iter().map(|(id, _)| id.as_str()).collect();

        assert_eq!(at_furnace, vec!["iron_ingot"]);
        assert_eq!(at_wheel, vec!["clay_pot"]);
    }

    /// Overlapping stations refuse to boot.
    ///
    /// A player standing in the overlap can only ever reach one of them, and
    /// which one depends on a distance comparison they cannot see. The mine
    /// yard shipped exactly this pair for an afternoon — the live probe walked
    /// to the wheel and was handed the furnace.
    #[test]
    #[should_panic(expected = "overlap")]
    fn two_stations_within_each_others_radius_refuse_to_boot() {
        // Both stations sit at (100, 100), but one is underground — those are
        // different places, so bring the wheel to the surface to collide.
        let mut cfg = a_station_world().0;
        cfg.station[1].interior = None;
        let mut q = Proxy::new("127.0.0.1", 0, 0, 0, None);
        {
            let m = Arc::get_mut(&mut q).expect("not yet shared");
            m.zone_cfg = cfg;
            m.crafting_cfg = a_station_world().1;
        }
        q.check_station_spacing();
    }

    /// ...and the ones we actually ship are spaced properly.
    #[test]
    fn the_shipped_stations_do_not_overlap() {
        let p = Proxy::new("127.0.0.1", 0, 0, 0, None);
        p.check_station_spacing(); // panics if they do
    }

    /// The nearest station wins, not the first in config order. The client
    /// picks the nearest, and a server that picked the first would put the two
    /// into quiet disagreement about where the player is standing.
    #[test]
    fn station_at_picks_the_nearest_not_the_first_listed() {
        let (mut zones, crafting) = a_station_world();
        // Two surface stations, deliberately overlapping so both are in range.
        zones.station[1].interior = None;
        zones.station[1].pos = (140, 100);
        let mut p = Proxy::new("127.0.0.1", 0, 0, 0, None);
        {
            let m = Arc::get_mut(&mut p).expect("not yet shared");
            m.zone_cfg = zones;
            m.crafting_cfg = crafting;
        }
        add_zone_region(&p, "zone_a", Region { x0: 0, y0: 0, x1: 25600, y1: 25600 });

        stand(&p, "u", 110, 100, "zone_a"); // 10 from the furnace, 30 from the wheel
        assert_eq!(p.station_at("u").map(|(s, _)| s.id.clone()), Some("yard_furnace".into()));
        stand(&p, "u", 135, 100, "zone_a"); // 35 from the furnace, 5 from the wheel
        assert_eq!(
            p.station_at("u").map(|(s, _)| s.id.clone()),
            Some("deep_wheel".into()),
            "the nearer one, even though the furnace is listed first"
        );
    }

    /// The shipped configs have to agree with each other: every placed station
    /// must name a type that exists, or the furnace in the yard is scenery.
    #[test]
    fn the_shipped_configs_place_only_stations_that_exist() {
        let zones = load_zone_config();
        let crafting = load_crafting_config();
        assert!(!zones.station.is_empty(), "the mine yard furnace should be placed");
        for st in &zones.station {
            let t = crafting.station(&st.kind).unwrap_or_else(|| {
                panic!("station `{}` is placed as type `{}`, which crafting.toml doesn't define", st.id, st.kind)
            });
            assert!(
                !crafting.recipes_for(t).is_empty(),
                "station `{}` accepts no shipped recipe, so it could never be used",
                st.id
            );
        }
    }

    /// The yard furnace stands outside the adit, close enough to be a beat in
    /// the loop and far enough to be a different place. Asserted against the
    /// file we actually ship, the same way #165 pinned the adit's siting.
    #[test]
    fn the_yard_furnace_is_sited_just_outside_the_adit() {
        let zones = load_zone_config();
        let furnace = zones
            .station
            .iter()
            .find(|s| s.id == "furnace_mine_yard")
            .expect("the mine yard furnace should be placed");
        assert!(furnace.interior.is_none(), "it stands in the yard, not down a gallery");

        let mine = zones.interior("mine_starter").expect("the mine should exist");
        let adit = mine.portals.first().expect("the mine should have a portal");
        let d = (((furnace.pos.0 - adit.world.0).pow(2) + (furnace.pos.1 - adit.world.1).pow(2))
            as f64)
            .sqrt();
        assert!(
            (40.0..200.0).contains(&d),
            "the furnace is {d:.0} units from the adit — a round trip should be a beat, not a haul"
        );
    }

    /// Interiors are invisible to geometry routing (#165). That is the property
    /// that makes an explicit portal the ONLY way in — no amount of walking,
    /// dying or region reshuffling can land somebody underground by accident.
    #[tokio::test]
    async fn geometry_routing_never_finds_an_interior() {
        let proxy = test_proxy();
        let _surface = add_zone_region(&proxy, "zone_a", Region { x0: 0, y0: 0, x1: 25600, y1: 25600 });
        let _mine = add_zone_region(&proxy, "mine_test", Region { x0: 0, y0: 0, x1: 600, y1: 300 });
        proxy.zones.lock().unwrap().get_mut("mine_test").unwrap().interior = true;

        // A point the interior's (meaningless) region also covers still resolves
        // to the surface zone that genuinely owns it.
        assert_eq!(proxy.zone_at(20, 30).as_deref(), Some("zone_a"));
        assert_eq!(proxy.zone_at(12800, 13500).as_deref(), Some("zone_a"));
        assert!(proxy.zone_is_interior("mine_test"));
        assert!(!proxy.zone_is_interior("zone_a"));
    }

    /// A position without a zone is meaningless once interiors exist. Two
    /// players at identical coordinates in different zones are not co-located,
    /// and the surface-only gates have to know it.
    #[tokio::test]
    async fn an_interior_player_is_not_on_the_surface() {
        let proxy = test_proxy();
        let _s = add_zone_region(&proxy, "zone_a", Region { x0: 0, y0: 0, x1: 25600, y1: 25600 });
        let _m = add_zone_region(&proxy, "mine_test", Region { x0: 0, y0: 0, x1: 600, y1: 300 });
        proxy.zones.lock().unwrap().get_mut("mine_test").unwrap().interior = true;

        // Both standing on the weapon master's exact coordinates — one outside,
        // one underground.
        let (wx, wy) = mmo::world::WEAPON_MASTER_AT;
        proxy.entity_state.lock().unwrap().insert(
            "outside".into(),
            EntityCache { x: wx, y: wy, hp: 100, zone: "zone_a".into() },
        );
        proxy.entity_state.lock().unwrap().insert(
            "underground".into(),
            EntityCache { x: wx, y: wy, hp: 100, zone: "mine_test".into() },
        );

        assert!(proxy.on_surface("outside"));
        assert!(!proxy.on_surface("underground"));
        assert!(proxy.at_weapon_master("outside"));
        assert!(
            !proxy.at_weapon_master("underground"),
            "an interior player claimed a surface NPC by sharing its coordinates"
        );
        // An untracked player counts as surface — the pre-#165 world.
        assert!(proxy.on_surface("nobody"));
    }

    /// The whole round trip, through the real handler: in at the mouth, out
    /// again, with the source zone told to let go each time.
    #[tokio::test]
    async fn a_portal_carries_a_player_in_and_back_out() {
        let proxy = test_proxy_with_zone_config(a_mine_config());
        let mut surface = add_zone_region(&proxy, "zone_a", Region { x0: 0, y0: 0, x1: 25600, y1: 25600 });
        let mut mine = add_zone_region(&proxy, "mine_test", Region { x0: 0, y0: 0, x1: 600, y1: 300 });
        proxy.zones.lock().unwrap().get_mut("mine_test").unwrap().interior = true;
        let (info, _rx) = make_client("p1", "zone_a", 32);
        proxy.clients.lock().unwrap().insert("p1".into(), info);

        // Standing at the adit mouth on the surface.
        proxy.entity_state.lock().unwrap().insert(
            "p1".into(),
            EntityCache { x: 12800, y: 13500, hp: 90, zone: "zone_a".into() },
        );
        proxy.apply_portal_enter("p1").await;

        assert_eq!(
            proxy.clients.lock().unwrap().get("p1").unwrap().current_zone,
            "mine_test",
            "the client should now be pointed at the interior"
        );
        let cached = proxy.entity_state.lock().unwrap().get("p1").cloned().unwrap();
        assert_eq!((cached.x, cached.y), (20, 30), "arrived at the portal's inside point");
        assert_eq!(cached.zone, "mine_test");
        assert_eq!(cached.hp, 90, "health carries through a transition");
        // The surface zone was told to let go, and the interior to take them.
        let mut left = false;
        while let Ok(Message::Text(t)) = surface.try_recv() {
            if t.contains("player_leave") { left = true; }
        }
        let mut spawned = false;
        while let Ok(Message::Text(t)) = mine.try_recv() {
            if t.contains("spawn_entity") { spawned = true; }
        }
        assert!(left, "the source zone was never told to let go");
        assert!(spawned, "the destination zone was never told to take them");

        // ...and back out.
        proxy.apply_portal_enter("p1").await;
        assert_eq!(proxy.clients.lock().unwrap().get("p1").unwrap().current_zone, "zone_a");
        let cached = proxy.entity_state.lock().unwrap().get("p1").cloned().unwrap();
        assert_eq!((cached.x, cached.y), (12800, 13500), "back at the mouth, outside");
        assert_eq!(cached.zone, "zone_a");
    }

    /// Standing nowhere near a portal does nothing but say so — the range gate
    /// is server-side, like every other interaction here.
    #[tokio::test]
    async fn a_portal_out_of_reach_is_refused() {
        let proxy = test_proxy_with_zone_config(a_mine_config());
        let _s = add_zone_region(&proxy, "zone_a", Region { x0: 0, y0: 0, x1: 25600, y1: 25600 });
        let _m = add_zone_region(&proxy, "mine_test", Region { x0: 0, y0: 0, x1: 600, y1: 300 });
        proxy.zones.lock().unwrap().get_mut("mine_test").unwrap().interior = true;
        let (info, mut rx) = make_client("p1", "zone_a", 32);
        proxy.clients.lock().unwrap().insert("p1".into(), info);
        proxy.entity_state.lock().unwrap().insert(
            "p1".into(),
            EntityCache { x: 12800, y: 12800, hp: 100, zone: "zone_a".into() },
        );

        proxy.apply_portal_enter("p1").await;

        assert_eq!(
            proxy.clients.lock().unwrap().get("p1").unwrap().current_zone,
            "zone_a",
            "an out-of-range portal moved somebody"
        );
        let mut saw_error = false;
        while let Ok(Message::Text(t)) = rx.try_recv() {
            if t.contains("portal.error") && t.contains("out_of_range") {
                saw_error = true;
            }
        }
        assert!(saw_error, "a refusal must say why");
    }

    /// An authored interior whose process isn't running is closed, not a
    /// silent no-op and certainly not a one-way trip into nothing.
    #[tokio::test]
    async fn a_portal_to_an_unregistered_interior_is_closed() {
        let proxy = test_proxy_with_zone_config(a_mine_config());
        let _s = add_zone_region(&proxy, "zone_a", Region { x0: 0, y0: 0, x1: 25600, y1: 25600 });
        let (info, mut rx) = make_client("p1", "zone_a", 32);
        proxy.clients.lock().unwrap().insert("p1".into(), info);
        proxy.entity_state.lock().unwrap().insert(
            "p1".into(),
            EntityCache { x: 12800, y: 13500, hp: 100, zone: "zone_a".into() },
        );

        proxy.apply_portal_enter("p1").await;

        assert_eq!(proxy.clients.lock().unwrap().get("p1").unwrap().current_zone, "zone_a");
        let mut saw = false;
        while let Ok(Message::Text(t)) = rx.try_recv() {
            if t.contains("portal.error") && t.contains("closed") {
                saw = true;
            }
        }
        assert!(saw, "an unrunning interior should report closed");
    }

    /// The auto-scaler must never consider an interior: it owns no slice of the
    /// world, so splitting it is meaningless and merging it into a surface
    /// neighbour would hand it geometry it cannot represent.
    #[tokio::test]
    async fn the_auto_scaler_ignores_interiors() {
        let proxy = test_proxy();
        let _s = add_zone_region(&proxy, "zone_a", Region { x0: 0, y0: 0, x1: 25600, y1: 25600 });
        let _m = add_zone_region(&proxy, "mine_test", Region { x0: 0, y0: 0, x1: 600, y1: 300 });
        {
            let mut zones = proxy.zones.lock().unwrap();
            zones.get_mut("mine_test").unwrap().interior = true;
            // Wildly over the split threshold, so it would certainly be chosen.
            zones.get_mut("mine_test").unwrap().population = 10_000;
            zones.get_mut("zone_a").unwrap().population = 0;
        }
        let infos: Vec<String> = proxy
            .zones
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, z)| !z.interior)
            .map(|(id, _)| id.clone())
            .collect();
        assert_eq!(infos, vec!["zone_a".to_string()], "the scaler's view includes an interior");
    }

    /// A kill's loot lands in a real inventory, as a real stackable item — the
    /// same shape build orders and the market already deal in. A hidden kill
    /// counter would be the one system in this game where the thing you earned
    /// isn't a thing.
    #[tokio::test]
    async fn a_kills_loot_lands_in_the_inventory_as_a_real_item() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let email = format!("hunter_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Hunter"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();

        zone.to_proxy
            .send(Message::Text(json!({
                "type": "gather_yield", "player_id": pid,
                "item_id": "dog_pelt", "qty": 1, "skill": "", "xp": 0,
                "source": "kill", "species": "wild_dog",
            }).to_string()))
            .unwrap();
        loop {
            recv_until(&mut ws, "inv.update").await;
            if qty_of_inventory(&db, &pid, "dog_pelt").await >= 1 {
                break;
            }
        }
        assert_eq!(qty_of_inventory(&db, &pid, "dog_pelt").await, 1);
        drop(ws);
    }

    /// A full pack must not silently eat a kill's loot. Gathering can afford to
    /// be quiet — you're at the node and can watch the count refuse to move —
    /// but the creature is GONE, and doing the work for nothing with no
    /// explanation is the version of this that reads as broken.
    #[tokio::test]
    async fn a_full_pack_says_so_rather_than_eating_the_loot() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let email = format!("laden_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Laden"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();

        // Fill the pack to MAX_CARRY, in trips (one add is itself capped).
        while qty_of_inventory(&db, &pid, "wood").await < mmo::persistence::MAX_CARRY {
            let before = qty_of_inventory(&db, &pid, "wood").await;
            db.add_to_inventory(&pid, "wood", mmo::persistence::MAX_CARRY).await.unwrap();
            if qty_of_inventory(&db, &pid, "wood").await == before {
                break;
            }
        }
        assert_eq!(qty_of_inventory(&db, &pid, "wood").await, mmo::persistence::MAX_CARRY);

        zone.to_proxy
            .send(Message::Text(json!({
                "type": "gather_yield", "player_id": pid,
                "item_id": "dog_pelt", "qty": 1, "skill": "", "xp": 0,
                "source": "kill", "species": "wild_dog",
            }).to_string()))
            .unwrap();

        let lost = recv_until(&mut ws, "loot.lost").await;
        assert_eq!(lost["item_id"], "dog_pelt");
        assert_eq!(lost["qty"], 1);
        assert!(
            lost["detail"].as_str().unwrap().contains("full"),
            "the reason should be legible: {lost:?}"
        );
        assert_eq!(
            qty_of_inventory(&db, &pid, "dog_pelt").await,
            0,
            "nothing should have been persisted"
        );
        drop(ws);
    }

    /// Gathering with a full pack stays quiet — the `loot.lost` nudge is for
    /// kills specifically, where the thing you earned no longer exists to try
    /// again on.
    #[tokio::test]
    async fn a_full_pack_stays_quiet_about_ordinary_gathering() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let email = format!("gatherer_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Gath"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();
        while qty_of_inventory(&db, &pid, "wood").await < mmo::persistence::MAX_CARRY {
            let before = qty_of_inventory(&db, &pid, "wood").await;
            db.add_to_inventory(&pid, "wood", mmo::persistence::MAX_CARRY).await.unwrap();
            if qty_of_inventory(&db, &pid, "wood").await == before {
                break;
            }
        }

        // No `source`, i.e. an ordinary gather swing.
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "gather_yield", "player_id": pid,
                "item_id": "stone", "qty": 1, "skill": "gathering", "xp": 1,
            }).to_string()))
            .unwrap();
        recv_until(&mut ws, "inv.update").await;

        // Drain what's queued; none of it should be a loot.lost.
        for _ in 0..20 {
            let Some(v) = recv_frame(&mut ws).await else { break };
            assert_ne!(v["type"], "loot.lost", "gathering shouldn't nag about a full pack");
        }
        drop(ws);
    }

    /// Two markets in one session (#153): walking from the capital's market to
    /// the Market District's retargets the panel, and the rates that follow are
    /// that market's own — the payoff for #152's per-district config.
    ///
    /// The rate limiter is deliberately checked here too. It's keyed on player
    /// alone, and a second market is exactly the situation that would expose it
    /// if it had ever been keyed on `(player, market)`: a trader could double
    /// their command budget just by having somewhere else to stand.
    #[tokio::test]
    async fn walking_between_markets_retargets_and_charges_each_market_its_own_rates() {
        // The capital keeps the shipped 3% tax; the Market District undercuts it
        // at 1%, mirroring the shape of the committed market.toml.
        let cfg = mmo::market_config::MarketConfigSet::parse(
            "[districts.market]\nsale_tax_num = 1\nwarehouse_slots = 120\n",
        )
        .unwrap();
        let (proxy, db, _dbf, zone) = proxy_with_market_config(cfg).await;

        // Both markets built. Coordinates match the authored sites closely
        // enough to land in the right districts.
        let capital = db
            .insert_build_order(
                "civic", "market", r#"{"wood":1}"#, "completed", 0, None, 0,
                Some(mmo::persistence::BuildPlacement {
                    structure_kind: "market".to_string(), x: 12900, y: 12800, x1: None, y1: None,
                }),
                None,
            )
            .await
            .unwrap();
        let remote = db
            .insert_build_order(
                "market", "market_east", r#"{"wood":1}"#, "completed", 0, None, 0,
                Some(mmo::persistence::BuildPlacement {
                    structure_kind: "market".to_string(), x: 20800, y: 9600, x1: None, y1: None,
                }),
                None,
            )
            .await
            .unwrap();
        assert_ne!(capital.id, remote.id);

        let email = format!("hauler_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Hauler"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();

        // --- at the capital ---------------------------------------------------
        stand_at(&proxy, &pid, 12900, 12800);
        ws.send(Message::Text(json!({"type": "market.open"}).to_string())).await.unwrap();
        let opened = recv_until(&mut ws, "market.opened").await;
        assert_eq!(opened["market_id"].as_str(), Some(capital.id.as_str()));
        assert_eq!(opened["district"].as_str(), Some("civic"));
        assert_eq!(opened["rules"]["sale_tax_num"].as_i64(), Some(3), "capital keeps the default tax");
        assert_eq!(opened["rules"]["warehouse_slots"].as_i64(), Some(60));

        // --- walk 8.6km east and open the other one ---------------------------
        stand_at(&proxy, &pid, 20800, 9600);
        ws.send(Message::Text(json!({"type": "market.open"}).to_string())).await.unwrap();
        let opened = recv_until(&mut ws, "market.opened").await;
        assert_eq!(
            opened["market_id"].as_str(),
            Some(remote.id.as_str()),
            "the panel should retarget to the market actually stood at"
        );
        assert_eq!(opened["district"].as_str(), Some("market"));
        assert_eq!(opened["rules"]["sale_tax_num"].as_i64(), Some(1), "the remote market undercuts");
        assert_eq!(opened["rules"]["warehouse_slots"].as_i64(), Some(120));

        // --- the warehouses are separate, and the panel says so ---------------
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "gather_yield", "player_id": pid,
                "item_id": "wood", "qty": 20, "skill": "gathering", "xp": 1,
            }).to_string()))
            .unwrap();
        loop {
            recv_until(&mut ws, "inv.update").await;
            if qty_of_inventory(&db, &pid, "wood").await >= 20 { break; }
        }

        // Deposit at the REMOTE market.
        stand_at(&proxy, &pid, 20800, 9600);
        ws.send(Message::Text(
            json!({"type": "warehouse.deposit", "item_id": "wood", "qty": 20}).to_string(),
        ))
        .await
        .unwrap();
        loop {
            let st = recv_until(&mut ws, "warehouse.state").await;
            if st["market_id"].as_str() == Some(remote.id.as_str())
                && st["items"].as_array().map(|a| !a.is_empty()).unwrap_or(false)
            {
                assert_eq!(st["slots"].as_i64(), Some(120), "the remote market's slot count");
                break;
            }
        }
        assert!(
            db.warehouse_for_character(&capital.id, &pid).await.unwrap().is_empty(),
            "goods deposited in the Market District turned up in the capital"
        );

        // --- the tax actually charged is the remote market's ------------------
        stand_at(&proxy, &pid, 20800, 9600);
        ws.send(Message::Text(json!({
            "type": "market.sell", "item_id": "wood", "unit_price": 10, "qty": 20,
            "command_id": "haul-sell-1",
        }).to_string()))
        .await
        .unwrap();
        recv_until(&mut ws, "market.fees").await;

        let buyer_email = format!("hbuyer_{}@t.test", Uuid::new_v4().simple());
        let mut bws = dial(&proxy).await;
        bws.send(Message::Text(
            json!({"type": "register", "email": buyer_email, "password": "pw12", "name": "HBuyer"}).to_string(),
        ))
        .await
        .unwrap();
        let bid = recv_until(&mut bws, "welcome").await["player_id"].as_str().unwrap().to_string();
        stand_at(&proxy, &bid, 20800, 9600);
        bws.send(Message::Text(json!({
            "type": "market.buy", "item_id": "wood", "unit_price": 10, "qty": 20,
            "command_id": "haul-buy-1",
        }).to_string()))
        .await
        .unwrap();
        recv_until(&mut bws, "market.fees").await;

        // 1% of the 200g fill = 2g, not the capital's 3% = 6g. The ledger is
        // the authority, not the message.
        let trades = db.recent_trades(&remote.id, "wood", 10).await.unwrap();
        let t = trades.first().expect("the cross should have traded at the remote market");
        assert_eq!(t.unit_price * t.qty, 200);
        assert_eq!(t.sale_tax_gold, 2, "the remote market's 1% tax should have been charged");
        // And nothing landed on the capital's ledger.
        assert!(
            db.recent_trades(&capital.id, "wood", 10).await.unwrap().is_empty(),
            "the trade was recorded against the wrong market"
        );

        drop(ws);
        drop(bws);
    }

    /// The market command rate limit is per PLAYER, not per (player, market).
    /// With one market the distinction was invisible; with two, keying it on the
    /// market would hand anyone a second full budget just for walking east.
    #[tokio::test]
    async fn the_market_rate_limit_is_per_player_not_per_market() {
        let (proxy, _db, _dbf, _zone) = proxy_with_shared_db().await;
        let pid = format!("rate_{}", Uuid::new_v4().simple());
        let budget = test_market_cfg().commands_per_minute;

        // Spend the whole budget.
        for i in 0..budget {
            assert!(proxy.allow_market_command(&pid), "command {i} should be allowed");
        }
        assert!(!proxy.allow_market_command(&pid), "the budget should be spent");

        // Standing at a different market changes nothing — the limiter never
        // sees a market id, and this is the test that keeps it that way.
        assert!(
            !proxy.allow_market_command(&pid),
            "the rate limit refilled — is it keyed on the market rather than the player?"
        );
        // A different player is unaffected.
        let other = format!("rate_{}", Uuid::new_v4().simple());
        assert!(proxy.allow_market_command(&other), "one player's spending throttled another");
    }

    /// The warehouse over the wire (#138): gated by the same server-side range
    /// check as `market.open`, hydrated on open, and pushed after every move.
    #[tokio::test]
    async fn warehouse_deposit_and_withdraw_are_range_gated_and_push_state() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        // A built market to trade at, without hauling 80 units in a test.
        let market = db
            .insert_build_order(
                "civic", "market", r#"{"wood":1}"#, "completed", 0, None, 0,
                Some(mmo::persistence::BuildPlacement {
                    structure_kind: "market".to_string(), x: 12800, y: 12800, x1: None, y1: None,
                }),
                None,
            )
            .await
            .unwrap();

        let email = format!("stocker_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Stocker"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "gather_yield", "player_id": pid,
                "item_id": "wood", "qty": 30, "skill": "gathering", "xp": 1,
            }).to_string()))
            .unwrap();
        recv_until(&mut ws, "inv.update").await;

        // Out of range: refused, and nothing moves.
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: 12000, y: 12800, hp: 100, zone: "zone_a".into() });
        ws.send(Message::Text(
            json!({"type": "warehouse.deposit", "item_id": "wood", "qty": 10}).to_string(),
        ))
        .await
        .unwrap();
        let err = recv_until(&mut ws, "market.error").await;
        assert_eq!(err["code"].as_str().unwrap(), "out_of_range");
        assert!(db.warehouse_for_character(&market.id, &pid).await.unwrap().is_empty());

        // Step up to the market: opening hydrates the (empty) warehouse.
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: 12800, y: 12800, hp: 100, zone: "zone_a".into() });
        ws.send(Message::Text(json!({"type": "market.open"}).to_string())).await.unwrap();
        let state = recv_until(&mut ws, "warehouse.state").await;
        assert_eq!(state["market_id"].as_str().unwrap(), market.id);
        assert!(state["items"].as_array().unwrap().is_empty());
        assert_eq!(state["slots"].as_i64().unwrap(), test_market_cfg().warehouse_slots);

        // Deposit: state comes back with the stock, and carry drops.
        ws.send(Message::Text(
            json!({"type": "warehouse.deposit", "item_id": "wood", "qty": 20}).to_string(),
        ))
        .await
        .unwrap();
        let state = loop {
            let v = recv_until(&mut ws, "warehouse.state").await;
            if !v["items"].as_array().unwrap().is_empty() {
                break v;
            }
        };
        let items = state["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["item_id"].as_str().unwrap(), "wood");
        assert_eq!(items[0]["qty"].as_i64().unwrap(), 20);
        assert_eq!(items[0]["state"].as_str().unwrap(), "available");
        assert_eq!(state["used"].as_i64().unwrap(), 1);

        // Withdraw it back.
        ws.send(Message::Text(
            json!({"type": "warehouse.withdraw", "item_id": "wood", "qty": 20}).to_string(),
        ))
        .await
        .unwrap();
        loop {
            let v = recv_until(&mut ws, "warehouse.state").await;
            if v["items"].as_array().unwrap().is_empty() {
                break;
            }
        }
        assert_eq!(
            qty_of_inventory(&db, &pid, "wood").await, 30,
            "everything came back to carry"
        );

        drop(ws);
    }

    async fn qty_of_inventory(db: &Db, pid: &str, item: &str) -> i64 {
        db.inventory_for_character(pid)
            .await
            .unwrap()
            .iter()
            .filter(|i| i.item_id == item)
            .map(|i| i.qty)
            .sum()
    }

    /// The trading loop over the wire (#139): stock a warehouse, rest a sell,
    /// have someone else cross it, and check both sides are paid and told.
    #[tokio::test]
    async fn market_sell_and_buy_execute_over_the_wire_at_the_resting_price() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let market = db
            .insert_build_order(
                "civic", "market", r#"{"wood":1}"#, "completed", 0, None, 0,
                Some(mmo::persistence::BuildPlacement {
                    structure_kind: "market".to_string(), x: 12800, y: 12800, x1: None, y1: None,
                }),
                None,
            )
            .await
            .unwrap();

        // Two traders, both standing at the market.
        let mut seller = dial(&proxy).await;
        seller.send(Message::Text(json!({
            "type": "register", "email": format!("s_{}@t.test", Uuid::new_v4().simple()),
            "password": "pw12", "name": "Seller",
        }).to_string())).await.unwrap();
        let sid = recv_until(&mut seller, "welcome").await["player_id"].as_str().unwrap().to_string();
        let mut buyer = dial(&proxy).await;
        buyer.send(Message::Text(json!({
            "type": "register", "email": format!("b_{}@t.test", Uuid::new_v4().simple()),
            "password": "pw12", "name": "Buyer",
        }).to_string())).await.unwrap();
        let bid_ = recv_until(&mut buyer, "welcome").await["player_id"].as_str().unwrap().to_string();

        // Seller mines wood and banks it at the market.
        zone.to_proxy.send(Message::Text(json!({
            "type": "gather_yield", "player_id": sid,
            "item_id": "wood", "qty": 20, "skill": "gathering", "xp": 1,
        }).to_string())).unwrap();
        // Loop past login hydration's EMPTY inv.update — grabbing that one
        // instead of the gather's would deposit before the wood arrives.
        loop {
            let v = recv_until(&mut seller, "inv.update").await;
            if v["items"].as_array().unwrap().iter().any(|i| i["item_id"] == "wood") {
                break;
            }
        }
        stand_at(&proxy, &sid, 12800, 12800);
        seller.send(Message::Text(
            json!({"type": "warehouse.deposit", "item_id": "wood", "qty": 20}).to_string(),
        )).await.unwrap();
        recv_until(&mut seller, "warehouse.state").await;

        // A tool can never go on the book — it belongs to the listing board.
        stand_at(&proxy, &sid, 12800, 12800);
        seller.send(Message::Text(json!({
            "type": "market.sell", "command_id": "c0", "item_id": "pickaxe", "unit_price": 5, "qty": 1,
        }).to_string())).await.unwrap();
        let err = recv_until(&mut seller, "market.error").await;
        assert_eq!(err["code"].as_str().unwrap(), "not_a_commodity");

        // Rest a sell at 8.
        stand_at(&proxy, &sid, 12800, 12800);
        seller.send(Message::Text(json!({
            "type": "market.sell", "command_id": "c1", "item_id": "wood", "unit_price": 8, "qty": 20,
        }).to_string())).await.unwrap();
        let orders = recv_until(&mut seller, "market.orders").await;
        let mine = orders["orders"].as_array().unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0]["unit_price"].as_i64().unwrap(), 8);
        let order_id = mine[0]["order_id"].as_str().unwrap().to_string();

        // The buyer sees the depth, aggregated by level and with no ownership.
        stand_at(&proxy, &bid_, 12800, 12800);
        buyer.send(Message::Text(
            json!({"type": "market.book_request", "item_id": "wood"}).to_string(),
        )).await.unwrap();
        let book = recv_until(&mut buyer, "market.book").await;
        assert_eq!(book["asks"], json!([{"price": 8, "qty": 20}]));
        assert!(book.to_string().find(&sid).is_none(), "depth must not leak who is selling");

        // Bid 12 for 5: pays the resting 8, not the 12 bid.
        let buyer_gold = db.character_gold(&bid_).await.unwrap();
        let seller_gold = db.character_gold(&sid).await.unwrap();
        stand_at(&proxy, &bid_, 12800, 12800);
        buyer.send(Message::Text(json!({
            "type": "market.buy", "command_id": "c2", "item_id": "wood", "unit_price": 12, "qty": 5,
        }).to_string())).await.unwrap();
        // Poll the durable outcome rather than racing the ticker frame: under
        // parallel test load the broadcast can sit behind a queue of other
        // pushes, which makes a strict frame wait a test of frame ordering
        // instead of of the trade.
        let mut traded = Vec::new();
        for _ in 0..40 {
            traded = db.recent_trades(&market.id, "wood", 10).await.unwrap();
            if !traded.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(traded.len(), 1, "the buy should have crossed the resting ask");
        assert_eq!((traded[0].unit_price, traded[0].qty), (8, 5), "executed at the resting 8");
        // Net of fees (#141): the buyer paid 40 plus a listing fee; the seller
        // received 40 minus sale tax.
        let buy_fee = test_market_cfg().listing_fee(12 * 5);
        assert_eq!(
            db.character_gold(&bid_).await.unwrap(), buyer_gold - 40 - buy_fee,
            "5 x the resting 8, plus the listing fee"
        );
        assert_eq!(
            db.character_gold(&sid).await.unwrap(),
            seller_gold + 40 - test_market_cfg().sale_tax(40),
            "seller paid immediately, net of sale tax"
        );

        // Goods landed in the BUYER's warehouse at this market.
        let held: i64 = db.warehouse_for_character(&market.id, &bid_).await.unwrap()
            .iter().filter(|r| r.item_id == "wood").map(|r| r.qty).sum();
        assert_eq!(held, 5);

        // A resend of the same command_id doesn't buy again — and stays
        // SILENT, because the client already got its answer the first time.
        // A failure message here would be a lie about what happened.
        stand_at(&proxy, &bid_, 12800, 12800);
        buyer.send(Message::Text(json!({
            "type": "market.buy", "command_id": "c2", "item_id": "wood", "unit_price": 12, "qty": 5,
        }).to_string())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            db.character_gold(&bid_).await.unwrap(), buyer_gold - 40 - buy_fee,
            "charged exactly once — a resend costs no second fee either"
        );
        assert_eq!(
            db.recent_trades(&market.id, "wood", 10).await.unwrap().len(), 1,
            "and traded exactly once"
        );

        // Cancelling returns the unsold escrow to available.
        stand_at(&proxy, &sid, 12800, 12800);
        seller.send(Message::Text(json!({
            "type": "market.cancel", "command_id": "c3", "order_id": order_id,
        }).to_string())).await.unwrap();
        let state = loop {
            let v = recv_until(&mut seller, "warehouse.state").await;
            if v["items"].as_array().unwrap().iter().any(|i| i["state"] == "available") {
                break v;
            }
        };
        let avail: i64 = state["items"].as_array().unwrap().iter()
            .filter(|i| i["state"] == "available").map(|i| i["qty"].as_i64().unwrap()).sum();
        assert_eq!(avail, 15, "20 offered - 5 sold = 15 back");

        drop(seller);
        drop(buyer);
    }

    /// #140 over the wire: a buy with nothing to cross RESTS (escrowing gold)
    /// and shows up as a bid, a later seller crosses it and is paid the BID,
    /// and the rate limit refuses a flood.
    #[tokio::test]
    async fn a_resting_bid_is_visible_and_filled_at_the_bid_price() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let market = db
            .insert_build_order(
                "civic", "market", r#"{"wood":1}"#, "completed", 0, None, 0,
                Some(mmo::persistence::BuildPlacement {
                    structure_kind: "market".to_string(), x: 12800, y: 12800, x1: None, y1: None,
                }),
                None,
            )
            .await
            .unwrap();

        let mut buyer = dial(&proxy).await;
        buyer.send(Message::Text(json!({
            "type": "register", "email": format!("rb_{}@t.test", Uuid::new_v4().simple()),
            "password": "pw12", "name": "RestingBuyer",
        }).to_string())).await.unwrap();
        let bid_ = recv_until(&mut buyer, "welcome").await["player_id"].as_str().unwrap().to_string();
        let mut seller = dial(&proxy).await;
        seller.send(Message::Text(json!({
            "type": "register", "email": format!("ls_{}@t.test", Uuid::new_v4().simple()),
            "password": "pw12", "name": "LateSeller",
        }).to_string())).await.unwrap();
        let sid = recv_until(&mut seller, "welcome").await["player_id"].as_str().unwrap().to_string();

        // An empty book: the buy rests instead of failing.
        let buyer_gold = db.character_gold(&bid_).await.unwrap();
        stand_at(&proxy, &bid_, 12800, 12800);
        buyer.send(Message::Text(json!({
            "type": "market.buy", "command_id": "b1", "item_id": "wood", "unit_price": 9, "qty": 10,
        }).to_string())).await.unwrap();
        let orders = recv_until(&mut buyer, "market.orders").await;
        let mine = orders["orders"].as_array().unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0]["side"].as_str().unwrap(), "buy");
        let bid_fee = test_market_cfg().listing_fee(9 * 10);
        assert_eq!(
            db.character_gold(&bid_).await.unwrap(), buyer_gold - 90 - bid_fee,
            "escrowed 10 x 9, plus the listing fee"
        );

        // It shows as depth on the BID side.
        let book = recv_until(&mut buyer, "market.book").await;
        assert_eq!(book["bids"], json!([{"price": 9, "qty": 10}]));

        // A seller arrives with stock and asks 6 — under the bid, so it
        // crosses and they're paid the RESTING 9.
        zone.to_proxy.send(Message::Text(json!({
            "type": "gather_yield", "player_id": sid,
            "item_id": "wood", "qty": 10, "skill": "gathering", "xp": 1,
        }).to_string())).unwrap();
        loop {
            let v = recv_until(&mut seller, "inv.update").await;
            if v["items"].as_array().unwrap().iter().any(|i| i["item_id"] == "wood") {
                break;
            }
        }
        stand_at(&proxy, &sid, 12800, 12800);
        seller.send(Message::Text(
            json!({"type": "warehouse.deposit", "item_id": "wood", "qty": 10}).to_string(),
        )).await.unwrap();
        recv_until(&mut seller, "warehouse.state").await;
        let seller_gold = db.character_gold(&sid).await.unwrap();
        stand_at(&proxy, &sid, 12800, 12800);
        seller.send(Message::Text(json!({
            "type": "market.sell", "command_id": "s1", "item_id": "wood", "unit_price": 6, "qty": 4,
        }).to_string())).await.unwrap();
        let mut trades = Vec::new();
        for _ in 0..40 {
            trades = db.recent_trades(&market.id, "wood", 10).await.unwrap();
            if !trades.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            trades.len(), 1,
            "the crossing sell should have traded. seller warehouse={:?} bids={:?} asks={:?}",
            db.warehouse_for_character(&market.id, &sid).await.unwrap(),
            db.book_for(&market.id, "wood", "buy").await.unwrap(),
            db.book_for(&market.id, "wood", "sell").await.unwrap(),
        );
        assert_eq!((trades[0].unit_price, trades[0].qty), (9, 4), "paid the resting bid");
        assert_eq!(
            db.character_gold(&sid).await.unwrap(),
            seller_gold + 36 - test_market_cfg().sale_tax(36) - test_market_cfg().listing_fee(6 * 4),
            "paid the resting bid of 9 not their own 6, net of sale tax and their listing fee"
        );
        // The buyer's escrow covered it — no further charge beyond the fee.
        assert_eq!(db.character_gold(&bid_).await.unwrap(), buyer_gold - 90 - bid_fee);
        let held: i64 = db.warehouse_for_character(&market.id, &bid_).await.unwrap()
            .iter().filter(|r| r.item_id == "wood").map(|r| r.qty).sum();
        assert_eq!(held, 4, "goods delivered to the resting buyer");

        // The rate limit lets a normal trader through and stops a flood.
        // Checked directly rather than by flooding the socket: a flood also
        // trips the open-order cap and buries the answer under a hundred
        // other frames, which makes the assertion about frame ordering
        // instead of about the limiter.
        let flooder = format!("flood-{}", Uuid::new_v4().simple());
        for i in 0..test_market_cfg().commands_per_minute {
            assert!(proxy.allow_market_command(&flooder), "command {i} is within the limit");
        }
        assert!(!proxy.allow_market_command(&flooder), "one past the limit is refused");
        // Other players are unaffected — the window is per character.
        assert!(proxy.allow_market_command(&sid));

        drop(buyer);
        drop(seller);
    }

    /// The listing board over the wire (#142): bank a worn tool, list it, and
    /// have someone else buy it — arriving as the same instance, same wear.
    #[tokio::test]
    async fn a_unique_item_lists_and_sells_over_the_wire() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let market = db
            .insert_build_order(
                "civic", "market", r#"{"wood":1}"#, "completed", 0, None, 0,
                Some(mmo::persistence::BuildPlacement {
                    structure_kind: "market".to_string(), x: 12800, y: 12800, x1: None, y1: None,
                }),
                None,
            )
            .await
            .unwrap();

        let mut seller = dial(&proxy).await;
        seller.send(Message::Text(json!({
            "type": "register", "email": format!("ls_{}@t.test", Uuid::new_v4().simple()),
            "password": "pw12", "name": "ToolSeller",
        }).to_string())).await.unwrap();
        let sid = recv_until(&mut seller, "welcome").await["player_id"].as_str().unwrap().to_string();
        let mut buyer = dial(&proxy).await;
        buyer.send(Message::Text(json!({
            "type": "register", "email": format!("lb_{}@t.test", Uuid::new_v4().simple()),
            "password": "pw12", "name": "ToolBuyer",
        }).to_string())).await.unwrap();
        let bid_ = recv_until(&mut buyer, "welcome").await["player_id"].as_str().unwrap().to_string();

        // Give the seller a pickaxe and wear it, so identity is falsifiable.
        zone.to_proxy.send(Message::Text(json!({
            "type": "gather_yield", "player_id": sid,
            "item_id": "pickaxe", "qty": 1, "skill": "gathering", "xp": 1,
        }).to_string())).unwrap();
        let inv = loop {
            let v = recv_until(&mut seller, "inv.update").await;
            if v["items"].as_array().unwrap().iter().any(|i| i["item_id"] == "pickaxe") {
                break v;
            }
        };
        let instance = inv["items"].as_array().unwrap().iter()
            .find(|i| i["item_id"] == "pickaxe").unwrap()["id"].as_str().unwrap().to_string();
        db.equip_instance(&sid, &instance).await.unwrap();
        db.wear_equipped_tool(&sid, "tool", 8).await.unwrap();
        // Derived, so a durability retune (#129) doesn't fail a test about
        // instance identity surviving a sale.
        let worn = mmo::world::tool_max_durability("pickaxe").unwrap() - 8;

        // Bank it, then list it.
        stand_at(&proxy, &sid, 12800, 12800);
        seller.send(Message::Text(
            json!({"type": "warehouse.deposit", "item_id": "pickaxe", "qty": 1}).to_string(),
        )).await.unwrap();
        let state = loop {
            let v = recv_until(&mut seller, "warehouse.state").await;
            if !v["items"].as_array().unwrap().is_empty() {
                break v;
            }
        };
        let wh_id = state["items"].as_array().unwrap()[0]["id"].as_str().unwrap().to_string();
        assert_eq!(state["items"].as_array().unwrap()[0]["durability"].as_i64(), Some(worn));

        stand_at(&proxy, &sid, 12800, 12800);
        seller.send(Message::Text(json!({
            "type": "listing.place", "command_id": "l1",
            "warehouse_item_id": wh_id, "ask_price": 75, "duration_hours": 24,
        }).to_string())).await.unwrap();
        let page = loop {
            let v = recv_until(&mut seller, "listing.page").await;
            if !v["listings"].as_array().unwrap().is_empty() {
                break v;
            }
        };
        let listed = &page["listings"].as_array().unwrap()[0];
        assert_eq!(listed["ask_price"].as_i64(), Some(75));
        assert_eq!(listed["durability"].as_i64(), Some(worn), "the board advertises real wear");
        assert_eq!(listed["mine"].as_bool(), Some(true));
        let listing_id = listed["listing_id"].as_str().unwrap().to_string();

        // A stale price is refused, and nothing is charged.
        let buyer_gold = db.character_gold(&bid_).await.unwrap();
        stand_at(&proxy, &bid_, 12800, 12800);
        buyer.send(Message::Text(json!({
            "type": "listing.buy", "command_id": "b1", "listing_id": listing_id, "expected_price": 60,
        }).to_string())).await.unwrap();
        let err = recv_until(&mut buyer, "market.error").await;
        assert_eq!(err["code"].as_str().unwrap(), "price_changed");
        assert_eq!(db.character_gold(&bid_).await.unwrap(), buyer_gold, "no surprise charge");

        // At the advertised price it goes through.
        let seller_gold = db.character_gold(&sid).await.unwrap();
        stand_at(&proxy, &bid_, 12800, 12800);
        buyer.send(Message::Text(json!({
            "type": "listing.buy", "command_id": "b2", "listing_id": listing_id, "expected_price": 75,
        }).to_string())).await.unwrap();
        let mut held = Vec::new();
        for _ in 0..40 {
            held = db.warehouse_for_character(&market.id, &bid_).await.unwrap();
            if !held.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(held.len(), 1, "the buyer received it");
        assert_eq!(held[0].id, wh_id, "the SAME instance that was advertised");
        assert_eq!(held[0].durability, Some(worn), "with its wear intact");
        assert_eq!(held[0].state, "available", "and collectable");
        assert_eq!(db.character_gold(&bid_).await.unwrap(), buyer_gold - 75);
        assert_eq!(
            db.character_gold(&sid).await.unwrap(),
            seller_gold + 75 - test_market_cfg().sale_tax(75),
            "seller paid the ask net of sale tax"
        );
        assert!(db.listing_by_id(&listing_id).await.unwrap().is_none(), "the listing is gone");

        drop(seller);
        drop(buyer);
    }

    /// Price history over the wire (#143): real trades roll up into candles a
    /// client can ask for.
    #[tokio::test]
    async fn market_history_answers_with_candles_from_real_trades() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let market = db
            .insert_build_order(
                "civic", "market", r#"{"wood":1}"#, "completed", 0, None, 0,
                Some(mmo::persistence::BuildPlacement {
                    structure_kind: "market".to_string(), x: 12800, y: 12800, x1: None, y1: None,
                }),
                None,
            )
            .await
            .unwrap();

        let mut seller = dial(&proxy).await;
        seller.send(Message::Text(json!({
            "type": "register", "email": format!("hs_{}@t.test", Uuid::new_v4().simple()),
            "password": "pw12", "name": "HistSeller",
        }).to_string())).await.unwrap();
        let sid = recv_until(&mut seller, "welcome").await["player_id"].as_str().unwrap().to_string();
        let mut buyer = dial(&proxy).await;
        buyer.send(Message::Text(json!({
            "type": "register", "email": format!("hb_{}@t.test", Uuid::new_v4().simple()),
            "password": "pw12", "name": "HistBuyer",
        }).to_string())).await.unwrap();
        let bid_ = recv_until(&mut buyer, "welcome").await["player_id"].as_str().unwrap().to_string();

        // An empty history is an empty list, not an error or a fake candle.
        stand_at(&proxy, &bid_, 12800, 12800);
        buyer.send(Message::Text(
            json!({"type": "market.history_request", "item_id": "wood"}).to_string(),
        )).await.unwrap();
        let h = recv_until(&mut buyer, "market.history").await;
        assert_eq!(h["item_id"].as_str().unwrap(), "wood");
        assert_eq!(h["interval_secs"].as_i64(), Some(test_market_cfg().candle_interval_secs));
        assert!(h["candles"].as_array().unwrap().is_empty(), "no trades yet, so no candles");

        // Trade for real.
        zone.to_proxy.send(Message::Text(json!({
            "type": "gather_yield", "player_id": sid,
            "item_id": "wood", "qty": 20, "skill": "gathering", "xp": 1,
        }).to_string())).unwrap();
        loop {
            let v = recv_until(&mut seller, "inv.update").await;
            if v["items"].as_array().unwrap().iter().any(|i| i["item_id"] == "wood") {
                break;
            }
        }
        stand_at(&proxy, &sid, 12800, 12800);
        seller.send(Message::Text(
            json!({"type": "warehouse.deposit", "item_id": "wood", "qty": 20}).to_string(),
        )).await.unwrap();
        recv_until(&mut seller, "warehouse.state").await;
        stand_at(&proxy, &sid, 12800, 12800);
        seller.send(Message::Text(json!({
            "type": "market.sell", "command_id": "h1", "item_id": "wood", "unit_price": 11, "qty": 20,
        }).to_string())).await.unwrap();
        recv_until(&mut seller, "market.orders").await;
        stand_at(&proxy, &bid_, 12800, 12800);
        buyer.send(Message::Text(json!({
            "type": "market.buy", "command_id": "h2", "item_id": "wood", "unit_price": 11, "qty": 7,
        }).to_string())).await.unwrap();
        for _ in 0..40 {
            if !db.recent_trades(&market.id, "wood", 5).await.unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // The rollup is a background job on a slow cadence, so drive it
        // directly rather than waiting minutes for the timer.
        let now = now_secs();
        db.roll_up_candles(test_market_cfg().candle_interval_secs, 0, now + 1).await.unwrap();

        stand_at(&proxy, &bid_, 12800, 12800);
        buyer.send(Message::Text(
            json!({"type": "market.history_request", "item_id": "wood", "days": 1}).to_string(),
        )).await.unwrap();
        let h = loop {
            let v = recv_until(&mut buyer, "market.history").await;
            if !v["candles"].as_array().unwrap().is_empty() {
                break v;
            }
        };
        let candles = h["candles"].as_array().unwrap();
        assert_eq!(candles.len(), 1);
        let c = &candles[0];
        assert_eq!((c["o"].as_i64(), c["h"].as_i64(), c["l"].as_i64(), c["c"].as_i64()),
            (Some(11), Some(11), Some(11), Some(11)), "one price, so OHLC all agree");
        assert_eq!(c["v"].as_i64(), Some(7), "volume is units traded");
        assert_eq!(c["n"].as_i64(), Some(1), "one fill");
        assert_eq!(
            c["t"].as_i64().unwrap() % test_market_cfg().candle_interval_secs, 0,
            "a bucket start is always a multiple of the interval"
        );

        drop(seller);
        drop(buyer);
    }

    /// Build wages (#145): an ordinary contribution pays the city's rate and
    /// pushes the new balance, while a DEMOLITION contribution pays nothing —
    /// demolition refunds the stone without clawing wages back, so paying for
    /// teardown as well as rebuild would subsidise the place → earn →
    /// demolish → replace loop in both directions.
    #[tokio::test]
    async fn build_wages_pay_for_building_but_never_for_demolition() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let mut editor_ws = dial_editor(&proxy, &db).await;

        let email = format!("wager_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "Wager"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();
        let start = db.character_gold(&pid).await.unwrap();

        // Build a 40m road stub for real, on site.
        editor_ws
            .send(Message::Text(json!({"type": "road.plan", "points": [[13800, 12600], [13840, 12600]]}).to_string()))
            .await
            .unwrap();
        let road = recv_until(&mut editor_ws, "road.planned").await["order_id"].as_str().unwrap().to_string();
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "gather_yield", "player_id": pid,
                "item_id": "stone", "qty": 10, "skill": "gathering", "xp": 1,
            }).to_string()))
            .unwrap();
        recv_until(&mut ws, "inv.update").await;
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: 13820, y: 12600, hp: 100, zone: "zone_a".into() });
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "build_contribute", "player_id": pid,
                "order_id": road, "item_id": "stone", "qty": 3,
            }).to_string()))
            .unwrap();

        // The wage push carries the authoritative balance and the delta.
        let paid = recv_until(&mut ws, "gold.update").await;
        let delta = paid["delta"].as_i64().unwrap();
        assert!(delta > 0, "building pays");
        assert_eq!(paid["reason"].as_str().unwrap(), "build_wages");
        assert_eq!(paid["gold"].as_i64().unwrap(), start + delta, "balance is authoritative, not client-computed");
        assert_eq!(db.character_gold(&pid).await.unwrap(), start + delta);

        // Finish the road, then post a demolition on it.
        for _ in 0..10 {
            zone.to_proxy
                .send(Message::Text(json!({
                    "type": "build_contribute", "player_id": pid,
                    "order_id": road, "item_id": "stone", "qty": 3,
                }).to_string()))
                .unwrap();
        }
        recv_until(&mut ws, "build.completed").await;
        let earned_building = db.character_gold(&pid).await.unwrap();
        assert!(earned_building > start, "the whole road paid wages");

        editor_ws
            .send(Message::Text(json!({"type": "road.demolish", "order_id": road}).to_string()))
            .await
            .unwrap();
        let demo_id = recv_until(&mut editor_ws, "road.demolition_planned").await["demo_order_id"]
            .as_str().unwrap().to_string();

        // Working the demolition pays NOTHING, even though it's a real
        // contribution that really completes the order.
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
        loop {
            let v = recv_until(&mut ws, "despawn").await;
            if v["player_id"].as_str().unwrap().starts_with("structure_road_") {
                break;
            }
        }
        assert_eq!(
            db.character_gold(&pid).await.unwrap(), earned_building,
            "demolition labour earns no wages"
        );

        drop(editor_ws);
        drop(ws);
    }

    /// `road.cells_request` (#134) answers with a road's full cell geometry
    /// and state, in path order — what the client seeds its progressive
    /// render and nearest-cell contribution readout from.
    #[tokio::test]
    async fn road_cells_request_answers_with_geometry_and_state() {
        let (proxy, db, _dbf, zone) = proxy_with_shared_db().await;
        let mut editor_ws = dial_editor(&proxy, &db).await;

        // A 10m stub: 2 cells.
        editor_ws
            .send(Message::Text(json!({"type": "road.plan", "points": [[700, 700], [710, 700]]}).to_string()))
            .await
            .unwrap();
        let order_id = recv_until(&mut editor_ws, "road.planned").await["order_id"].as_str().unwrap().to_string();

        editor_ws.send(Message::Text(json!({"type": "road.cells_request", "order_id": order_id}).to_string()))
            .await
            .unwrap();
        let resp = recv_until(&mut editor_ws, "road.cells").await;
        assert_eq!(resp["order_id"].as_str().unwrap(), order_id);
        let cells = resp["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 2, "10m / 5m cells");
        assert_eq!((cells[0]["x0"].as_i64(), cells[0]["y0"].as_i64()), (Some(700), Some(700)));
        assert_eq!((cells[1]["x1"].as_i64(), cells[1]["y1"].as_i64()), (Some(710), Some(700)));
        assert!(!cells[0]["completed"].as_bool().unwrap());
        assert_eq!(cells[0]["progress"], json!({}));

        // Finish cell 0 for real, then re-request: the answer reflects it live.
        let email = format!("cellreq_{}@t.test", Uuid::new_v4().simple());
        let mut ws = dial(&proxy).await;
        ws.send(Message::Text(
            json!({"type": "register", "email": email, "password": "pw12", "name": "CellReq"}).to_string(),
        ))
        .await
        .unwrap();
        let pid = recv_until(&mut ws, "welcome").await["player_id"].as_str().unwrap().to_string();
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "gather_yield", "player_id": pid,
                "item_id": "stone", "qty": 5, "skill": "gathering", "xp": 1,
            }).to_string()))
            .unwrap();
        recv_until(&mut ws, "inv.update").await;
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: 700, y: 700, hp: 100, zone: "zone_a".into() });
        zone.to_proxy
            .send(Message::Text(json!({
                "type": "build_contribute", "player_id": pid,
                "order_id": order_id, "item_id": "stone", "qty": 5,
            }).to_string()))
            .unwrap();
        recv_until(&mut ws, "road.cell_progress").await;

        editor_ws.send(Message::Text(json!({"type": "road.cells_request", "order_id": order_id}).to_string()))
            .await
            .unwrap();
        let resp = recv_until(&mut editor_ws, "road.cells").await;
        let cells = resp["cells"].as_array().unwrap();
        assert!(cells[0]["completed"].as_bool().unwrap(), "cell 0 reflects its real completion");

        // Unknown order id: an empty answer, not a hang or error.
        editor_ws.send(Message::Text(json!({"type": "road.cells_request", "order_id": "no-such"}).to_string()))
            .await
            .unwrap();
        let resp = recv_until(&mut editor_ws, "road.cells").await;
        assert!(resp["cells"].as_array().unwrap().is_empty());

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
        proxy.entity_state.lock().unwrap().insert(pid.clone(), EntityCache { x: 2000, y: 2000, hp: 100, zone: "zone_a".into() });
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