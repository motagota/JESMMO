// Simulated player bots.
//
// Each bot is a real persistent character (registers/logs in like a human
// client) that drives the actual gameplay loop instead of just wandering:
// it gathers from resource nodes it discovers and contributes what it
// carries to a build order, so load tests exercise gather/inventory/build
// traffic and DB writes, not just connection density and movement.
//
// It is not owned by any zone — the proxy assigns it a starting zone and the
// normal seamless-migration path carries it across servers as it travels.
// Stop this process and the bots disappear; the zone servers hold no AI of
// their own.
//
// Usage:
//   bots [ws_uri] [count] [--move-ms MS]
//     ws_uri      proxy client endpoint   (default ws://127.0.0.1:8766)
//     count       number of bots          (default 6)
//     --move-ms   ms between moves         (default 300)

use std::collections::HashMap;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde_json::{json, Value};
use tokio::time::sleep;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::Message;

use mmo::util::{dist2, random_heading};

/// How many units of one item a bot gathers before heading off to contribute
/// it, so it doesn't camp a single node forever.
const CARRY_GOAL: i64 = 3;
/// Distance (world units) at which a bot considers itself "arrived" at a
/// target and switches from walking to acting — comfortably inside the
/// server's own gather/board range (50/60) so it doesn't hover on the edge.
const ARRIVE_RANGE: i32 = 30;
/// How far a bot will notice a node/board it hasn't walked past yet. Nodes
/// and boards are only known once a `status_update` for them has arrived, so
/// this just bounds how "eager" a bot is to detour toward a distant one.
const NOTICE_RANGE: i32 = 500;

struct Config {
    uri: String,
    count: usize,
    move_ms: u64,
}

fn parse_config() -> Config {
    let args: Vec<String> = std::env::args().collect();
    let mut cfg = Config {
        uri: "ws://127.0.0.1:8766".to_string(),
        count: 6,
        move_ms: 300,
    };
    let mut positional = 0;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--move-ms" => {
                cfg.move_ms = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(cfg.move_ms);
                i += 2;
            }
            a => {
                match positional {
                    0 => cfg.uri = a.to_string(),
                    1 => cfg.count = a.parse().unwrap_or(cfg.count),
                    _ => {}
                }
                positional += 1;
                i += 1;
            }
        }
    }
    cfg
}

/// A step (each axis clamped to `step`) from `(fx,fy)` toward `(tx,ty)` — no
/// pathfinding, just a straight line, which is fine for load-testing on the
/// open capital.
fn heading_toward(fx: i32, fy: i32, tx: i32, ty: i32, step: i32) -> (i32, i32) {
    ((tx - fx).clamp(-step, step), (ty - fy).clamp(-step, step))
}

#[derive(Clone)]
struct KnownNode { x: i32, y: i32, qty: i64 }

#[derive(Clone)]
struct KnownBoard { x: i32, y: i32 }

#[derive(Clone)]
struct KnownOrder {
    order_id: String,
    state: String,
    required: HashMap<String, i64>,
}

#[derive(Clone)]
enum BotState {
    Wander,
    SeekNode(String),
    Gathering(String),
    SeekBoard,
}

/// Everything a bot has learned about itself and the world from server
/// pushes, kept just accurate enough to drive the state machine.
#[derive(Default)]
struct BotWorld {
    player_id: String,
    x: i32,
    y: i32,
    inventory: HashMap<String, i64>,
    nodes: HashMap<String, KnownNode>,
    boards: HashMap<String, KnownBoard>,
    orders: Vec<KnownOrder>,
}

impl BotWorld {
    fn apply(&mut self, v: &Value) {
        match v.get("type").and_then(|t| t.as_str()) {
            Some("welcome") => {
                if let Some(pid) = v.get("player_id").and_then(|p| p.as_str()) {
                    self.player_id = pid.to_string();
                }
            }
            Some("status_update") => {
                let Some(pid) = v.get("player_id").and_then(|p| p.as_str()) else { return };
                let Some(state) = v.get("state") else { return };
                let x = state.get("x").and_then(|n| n.as_i64()).unwrap_or(0) as i32;
                let y = state.get("y").and_then(|n| n.as_i64()).unwrap_or(0) as i32;
                if pid == self.player_id {
                    self.x = x;
                    self.y = y;
                    return;
                }
                match state.get("type").and_then(|t| t.as_str()) {
                    Some("resource") => {
                        let qty = state.get("qty").and_then(|q| q.as_i64()).unwrap_or(0);
                        self.nodes.insert(pid.to_string(), KnownNode { x, y, qty });
                    }
                    Some("build_board") => {
                        self.boards.insert(pid.to_string(), KnownBoard { x, y });
                    }
                    _ => {}
                }
            }
            Some("despawn") => {
                if let Some(pid) = v.get("player_id").and_then(|p| p.as_str()) {
                    self.nodes.remove(pid);
                    self.boards.remove(pid);
                }
            }
            Some("inv.update") => {
                let mut items = HashMap::new();
                if let Some(arr) = v.get("items").and_then(|i| i.as_array()) {
                    for it in arr {
                        if let (Some(id), Some(qty)) = (
                            it.get("item_id").and_then(|i| i.as_str()),
                            it.get("qty").and_then(|q| q.as_i64()),
                        ) {
                            *items.entry(id.to_string()).or_insert(0) += qty;
                        }
                    }
                }
                self.inventory = items;
            }
            Some("build.list") => {
                if let Some(arr) = v.get("orders").and_then(|o| o.as_array()) {
                    self.orders = arr
                        .iter()
                        .filter_map(|o| {
                            let order_id = o.get("order_id")?.as_str()?.to_string();
                            let state = o.get("state")?.as_str()?.to_string();
                            let required: HashMap<String, i64> = o
                                .get("required")
                                .and_then(|r| r.as_object())
                                .map(|obj| {
                                    obj.iter()
                                        .filter_map(|(k, v)| Some((k.clone(), v.as_i64()?)))
                                        .collect()
                                })
                                .unwrap_or_default();
                            Some(KnownOrder { order_id, state, required })
                        })
                        .collect();
                }
            }
            _ => {}
        }
    }

    /// The nearest known resource node still carrying stock, within notice range.
    fn nearest_node(&self) -> Option<(String, KnownNode)> {
        self.nodes
            .iter()
            .filter(|(_, n)| n.qty > 0)
            .filter(|(_, n)| dist2(self.x, self.y, n.x, n.y) <= (NOTICE_RANGE as i64).pow(2))
            .min_by_key(|(_, n)| dist2(self.x, self.y, n.x, n.y))
            .map(|(id, n)| (id.clone(), n.clone()))
    }

    fn nearest_board(&self) -> Option<(String, KnownBoard)> {
        self.boards
            .iter()
            .min_by_key(|(_, b)| dist2(self.x, self.y, b.x, b.y))
            .map(|(id, b)| (id.clone(), b.clone()))
    }

    /// An open order needing an item this bot is currently carrying, and how
    /// much of it to hand over.
    fn pick_contribution(&self) -> Option<(String, String, i64)> {
        for order in &self.orders {
            if order.state != "open" {
                continue;
            }
            for (item_id, needed) in &order.required {
                if let Some(&have) = self.inventory.get(item_id) {
                    if have > 0 && *needed > 0 {
                        return Some((order.order_id.clone(), item_id.clone(), have.min(*needed)));
                    }
                }
            }
        }
        None
    }

    fn carrying_at_least(&self, goal: i64) -> bool {
        self.inventory.values().any(|&qty| qty >= goal)
    }
}

/// Authenticate on a fresh connection: try registering a persistent character
/// for this bot slot; if the email is already taken from a previous run, fall
/// back to logging into it. Returns once `welcome` (with our player_id) has
/// arrived, or `None` if the socket closed first.
async fn authenticate(
    index: usize,
    sink: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    stream: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Option<String> {
    let email = format!("bot{index}@sim.test");
    let password = "botpass1";
    let name = format!("Bot{index}");

    let register = json!({"type": "register", "email": email, "password": password, "name": name}).to_string();
    if sink.send(Message::Text(register)).await.is_err() {
        return None;
    }

    loop {
        let msg = stream.next().await?.ok()?;
        let Message::Text(text) = msg else { continue };
        let v: Value = serde_json::from_str(&text).ok()?;
        match v.get("type").and_then(|t| t.as_str()) {
            Some("auth_error") => {
                // Already registered from an earlier run: log in instead.
                let login = json!({"type": "login", "email": email, "password": password}).to_string();
                if sink.send(Message::Text(login)).await.is_err() {
                    return None;
                }
            }
            Some("welcome") => {
                return v.get("player_id").and_then(|p| p.as_str()).map(|s| s.to_string());
            }
            _ => {}
        }
    }
}

/// One simulated player: connect, authenticate, then run gather → contribute
/// loop, wandering in between while it looks for a node to work.
async fn run_bot(index: usize, uri: String, move_ms: u64) {
    loop {
        match connect_async(&uri).await {
            Ok((ws, _)) => {
                println!("[bot {index}] connected");
                let (mut sink, mut stream) = ws.split();

                let Some(player_id) = authenticate(index, &mut sink, &mut stream).await else {
                    println!("[bot {index}] auth failed or disconnected");
                    sleep(Duration::from_secs(2)).await;
                    continue;
                };
                println!("[bot {index}] registered as {player_id}");

                let mut world = BotWorld { player_id, ..Default::default() };
                let mut state = BotState::Wander;
                let (mut hx, mut hy) = random_heading();
                let mut tick = tokio::time::interval(Duration::from_millis(move_ms));

                loop {
                    tokio::select! {
                        incoming = stream.next() => {
                            match incoming {
                                Some(Ok(Message::Text(text))) => {
                                    if let Ok(v) = serde_json::from_str::<Value>(&text) {
                                        world.apply(&v);
                                    }
                                }
                                Some(Ok(_)) => {}
                                _ => break, // closed or errored -> reconnect
                            }
                        }
                        _ = tick.tick() => {
                            state = match state {
                                BotState::Wander => {
                                    if let Some((node_id, _)) = world.nearest_node() {
                                        BotState::SeekNode(node_id)
                                    } else {
                                        if rand::thread_rng().gen_bool(0.15) {
                                            let (nx, ny) = random_heading();
                                            hx = nx;
                                            hy = ny;
                                        }
                                        let mv = json!({"type": "move", "dx": hx * 10, "dy": hy * 10}).to_string();
                                        if sink.send(Message::Text(mv)).await.is_err() {
                                            break;
                                        }
                                        BotState::Wander
                                    }
                                }
                                BotState::SeekNode(node_id) => {
                                    match world.nodes.get(&node_id).cloned() {
                                        Some(n) if n.qty > 0 => {
                                            if dist2(world.x, world.y, n.x, n.y) <= (ARRIVE_RANGE as i64).pow(2) {
                                                let start = json!({"type": "gather.start", "node_id": node_id}).to_string();
                                                if sink.send(Message::Text(start)).await.is_err() {
                                                    break;
                                                }
                                                BotState::Gathering(node_id)
                                            } else {
                                                let (dx, dy) = heading_toward(world.x, world.y, n.x, n.y, 10);
                                                let mv = json!({"type": "move", "dx": dx, "dy": dy}).to_string();
                                                if sink.send(Message::Text(mv)).await.is_err() {
                                                    break;
                                                }
                                                BotState::SeekNode(node_id)
                                            }
                                        }
                                        _ => BotState::Wander, // depleted or forgotten
                                    }
                                }
                                BotState::Gathering(node_id) => {
                                    let depleted = world.nodes.get(&node_id).map(|n| n.qty <= 0).unwrap_or(true);
                                    if depleted || world.carrying_at_least(CARRY_GOAL) {
                                        let stop = json!({"type": "gather.stop"}).to_string();
                                        if sink.send(Message::Text(stop)).await.is_err() {
                                            break;
                                        }
                                        BotState::SeekBoard
                                    } else {
                                        BotState::Gathering(node_id)
                                    }
                                }
                                BotState::SeekBoard => {
                                    match world.nearest_board() {
                                        Some((_, b)) => {
                                            if dist2(world.x, world.y, b.x, b.y) <= (ARRIVE_RANGE as i64).pow(2) {
                                                if let Some((order_id, item_id, qty)) = world.pick_contribution() {
                                                    let contribute = json!({
                                                        "type": "build.contribute",
                                                        "order_id": order_id, "item_id": item_id, "qty": qty,
                                                    }).to_string();
                                                    let _ = sink.send(Message::Text(contribute)).await;
                                                }
                                                BotState::Wander
                                            } else {
                                                let (dx, dy) = heading_toward(world.x, world.y, b.x, b.y, 10);
                                                let mv = json!({"type": "move", "dx": dx, "dy": dy}).to_string();
                                                if sink.send(Message::Text(mv)).await.is_err() {
                                                    break;
                                                }
                                                BotState::SeekBoard
                                            }
                                        }
                                        None => BotState::Wander, // no board discovered yet
                                    }
                                }
                            };
                        }
                    }
                }
                println!("[bot {index}] disconnected, reconnecting...");
            }
            Err(e) => {
                println!("[bot {index}] connect failed: {e}");
            }
        }
        sleep(Duration::from_secs(2)).await;
    }
}

#[tokio::main]
async fn main() {
    let cfg = parse_config();
    println!(
        "Starting {} simulated player bots -> {} (move every {}ms)",
        cfg.count, cfg.uri, cfg.move_ms
    );

    let mut handles = Vec::new();
    for i in 0..cfg.count {
        let uri = cfg.uri.clone();
        let move_ms = cfg.move_ms;
        handles.push(tokio::spawn(async move { run_bot(i, uri, move_ms).await }));
    }

    // Run until killed.
    for h in handles {
        let _ = h.await;
    }
}
