// Connection-density load generator for the proxy.
//
// Opens many concurrent client websockets, holds them open (auto-ponging so the
// proxy's liveness reaper keeps them), and reports how many connections are
// actually alive. Optionally subscribes to the admin port to read the proxy's
// OWN view (total_players, dropped_frames) so client-side and server-side counts
// can be compared.
//
// Because the load generator uses the same tokio/tungstenite stack as the proxy,
// the client is not the bottleneck — but note the limits below.
//
// Usage:
//   loadtest [ws_uri] [connections] [--ramp N] [--move-ms MS] [--hold S] [--admin URI]
//
//   ws_uri        proxy client endpoint            (default ws://127.0.0.1:8766)
//   connections   how many to open                 (default 1000)
//   --ramp N      new connections per second       (default 500; 0 = all at once)
//   --move-ms MS  each bot sends a move every MS    (default 0 = idle, pure density)
//   --hold S      seconds to hold after ramp-up     (default 30)
//   --admin URI   admin endpoint to read server stats (e.g. ws://127.0.0.1:8767)
//
// Example (idle density to 20k, watching the server's own count):
//   loadtest ws://127.0.0.1:8766 20000 --ramp 2000 --hold 60 --admin ws://127.0.0.1:8767
//
// IMPORTANT density caveats when running everything on one box:
//   * Outbound ephemeral ports cap concurrent localhost connections. On Windows
//     the default dynamic range is ~16k (49152-65535). Raise it with:
//        netsh int ipv4 set dynamicport tcp start=10000 num=55000
//   * Each connection also costs an fd/handle on the proxy side.
//   * A zone must be registered or the proxy rejects clients immediately.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde_json::{json, Value};
use tokio::time::{sleep, sleep_until};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::Message;

#[derive(Default)]
struct Stats {
    established: AtomicU64, // successful handshakes (cumulative)
    failed: AtomicU64,      // failed connect attempts (cumulative)
    closed: AtomicU64,      // connections that dropped after being established
    alive: AtomicUsize,     // currently-open connections
    moves_sent: AtomicU64,  // move packets sent (cumulative)
    server_players: AtomicU64, // proxy-reported total_players (latest)
    server_dropped: AtomicU64, // proxy-reported dropped_frames (latest)
}

struct Config {
    uri: String,
    connections: usize,
    ramp_per_sec: u64,
    move_ms: u64,
    hold_secs: u64,
    admin_uri: Option<String>,
}

fn parse_config() -> Config {
    let args: Vec<String> = std::env::args().collect();
    let mut cfg = Config {
        uri: "ws://127.0.0.1:8766".to_string(),
        connections: 1000,
        ramp_per_sec: 500,
        move_ms: 0,
        hold_secs: 30,
        admin_uri: None,
    };

    // Positional: [uri] [connections]
    let mut positional = 0;
    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--ramp" => {
                cfg.ramp_per_sec = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(cfg.ramp_per_sec);
                i += 2;
            }
            "--move-ms" => {
                cfg.move_ms = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(cfg.move_ms);
                i += 2;
            }
            "--hold" => {
                cfg.hold_secs = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(cfg.hold_secs);
                i += 2;
            }
            "--admin" => {
                cfg.admin_uri = args.get(i + 1).cloned();
                i += 2;
            }
            _ => {
                match positional {
                    0 => cfg.uri = a.clone(),
                    1 => cfg.connections = a.parse().unwrap_or(cfg.connections),
                    _ => {}
                }
                positional += 1;
                i += 1;
            }
        }
    }
    cfg
}

/// One bot: connect, then stay alive (reading drives auto-pong) and optionally
/// send moves, until the deadline.
async fn run_connection(stats: Arc<Stats>, uri: String, move_ms: u64, deadline: Instant) {
    let ws = match connect_async(&uri).await {
        Ok((ws, _)) => ws,
        Err(_) => {
            stats.failed.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    stats.established.fetch_add(1, Ordering::Relaxed);
    stats.alive.fetch_add(1, Ordering::Relaxed);

    let (mut sink, mut stream) = ws.split();

    // A timer that's effectively disabled when move_ms == 0.
    let period = if move_ms > 0 {
        Duration::from_millis(move_ms)
    } else {
        Duration::from_secs(3600)
    };
    let mut move_timer = tokio::time::interval(period);
    move_timer.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            _ = sleep_until(deadline.into()) => break,
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(_)) => { /* keep alive; tungstenite auto-pongs while polled */ }
                    _ => break, // closed or errored
                }
            }
            _ = move_timer.tick() => {
                if move_ms > 0 {
                    let (dx, dy) = {
                        let mut rng = rand::thread_rng();
                        (rng.gen_range(-10..=10), rng.gen_range(-10..=10))
                    };
                    let packet = json!({"type": "move", "dx": dx, "dy": dy}).to_string();
                    if sink.send(Message::Text(packet)).await.is_ok() {
                        stats.moves_sent.fetch_add(1, Ordering::Relaxed);
                    } else {
                        break;
                    }
                }
            }
        }
    }

    stats.alive.fetch_sub(1, Ordering::Relaxed);
    stats.closed.fetch_add(1, Ordering::Relaxed);
}

/// Read the proxy's admin status feed into the shared stats.
async fn monitor_admin(stats: Arc<Stats>, admin_uri: String) {
    loop {
        if let Ok((ws, _)) = connect_async(&admin_uri).await {
            let (_sink, mut stream) = ws.split();
            while let Some(Ok(msg)) = stream.next().await {
                if let Message::Text(t) = msg {
                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                        if v.get("type").and_then(|x| x.as_str()) == Some("status") {
                            if let Some(n) = v.get("total_players").and_then(|x| x.as_u64()) {
                                stats.server_players.store(n, Ordering::Relaxed);
                            }
                            if let Some(d) = v.get("dropped_frames").and_then(|x| x.as_u64()) {
                                stats.server_dropped.store(d, Ordering::Relaxed);
                            }
                        }
                    }
                }
            }
        }
        sleep(Duration::from_secs(2)).await; // reconnect on drop
    }
}

/// Print a status line once per second.
async fn reporter(stats: Arc<Stats>, cfg_has_admin: bool, start: Instant) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        interval.tick().await;
        let elapsed = start.elapsed().as_secs_f64().max(0.001);
        let est = stats.established.load(Ordering::Relaxed);
        let failed = stats.failed.load(Ordering::Relaxed);
        let alive = stats.alive.load(Ordering::Relaxed);
        let moves = stats.moves_sent.load(Ordering::Relaxed);
        let rate = est as f64 / elapsed;

        let mut line = format!(
            "[{:>5.1}s] alive={alive:<7} established={est:<7} failed={failed:<5} \
             connect_rate={rate:>7.0}/s moves={moves}",
            elapsed
        );
        if cfg_has_admin {
            line.push_str(&format!(
                " | server_players={} server_dropped={}",
                stats.server_players.load(Ordering::Relaxed),
                stats.server_dropped.load(Ordering::Relaxed),
            ));
        }
        println!("{line}");
    }
}

#[tokio::main]
async fn main() {
    let cfg = parse_config();

    println!("=== proxy connection-density load test ===");
    println!("target      : {}", cfg.uri);
    println!("connections : {}", cfg.connections);
    println!("ramp        : {}/s", cfg.ramp_per_sec);
    println!(
        "move-ms     : {}",
        if cfg.move_ms == 0 { "idle (pure density)".to_string() } else { format!("{} ms", cfg.move_ms) }
    );
    println!("hold        : {}s", cfg.hold_secs);
    if let Some(a) = &cfg.admin_uri {
        println!("admin       : {a}");
    }
    println!();

    let stats = Arc::new(Stats::default());
    let start = Instant::now();

    // How long the whole run lasts: ramp-up time + hold.
    let ramp_secs = if cfg.ramp_per_sec == 0 {
        0.0
    } else {
        cfg.connections as f64 / cfg.ramp_per_sec as f64
    };
    let deadline = start + Duration::from_secs_f64(ramp_secs) + Duration::from_secs(cfg.hold_secs);

    // Reporter.
    let has_admin = cfg.admin_uri.is_some();
    {
        let stats = stats.clone();
        tokio::spawn(async move { reporter(stats, has_admin, start).await });
    }

    // Admin monitor.
    if let Some(admin_uri) = cfg.admin_uri.clone() {
        let stats = stats.clone();
        tokio::spawn(async move { monitor_admin(stats, admin_uri).await });
    }

    // Ramp connections.
    let gap = if cfg.ramp_per_sec == 0 {
        Duration::ZERO
    } else {
        Duration::from_secs_f64(1.0 / cfg.ramp_per_sec as f64)
    };
    for _ in 0..cfg.connections {
        let stats = stats.clone();
        let uri = cfg.uri.clone();
        let move_ms = cfg.move_ms;
        tokio::spawn(async move { run_connection(stats, uri, move_ms, deadline).await });
        if !gap.is_zero() {
            sleep(gap).await;
        }
    }

    // Hold until the deadline, then summarize.
    sleep_until(deadline.into()).await;

    let est = stats.established.load(Ordering::Relaxed);
    let failed = stats.failed.load(Ordering::Relaxed);
    let alive = stats.alive.load(Ordering::Relaxed);
    let closed = stats.closed.load(Ordering::Relaxed);
    let moves = stats.moves_sent.load(Ordering::Relaxed);

    println!();
    println!("=== summary ===");
    println!("requested      : {}", cfg.connections);
    println!("established    : {est}");
    println!("failed         : {failed}");
    println!("alive at end   : {alive}");
    println!("closed early   : {closed}");
    println!("moves sent     : {moves}");
    if has_admin {
        println!(
            "server_players : {}",
            stats.server_players.load(Ordering::Relaxed)
        );
        println!(
            "server_dropped : {}",
            stats.server_dropped.load(Ordering::Relaxed)
        );
    }
    let success = est.saturating_sub(closed);
    if failed > 0 || (closed > 0 && cfg.move_ms == 0) {
        println!(
            "note: {failed} failed + {closed} closed early. If failures cluster at a \
             specific count, you've likely hit an ephemeral-port or fd ceiling."
        );
    }
    println!("net sustained  : ~{success}");
}
