// Simulated player bots.
//
// Each bot is just a player: it connects to the proxy's client endpoint exactly
// like a human, then walks around. It is not owned by any zone — the proxy
// assigns it a starting zone and the normal seamless-migration path carries it
// across servers as it travels. Stop this process and the bots disappear; the
// zone servers hold no AI of their own.
//
// Movement is a "patrol": each bot keeps a horizontal heading (with a little
// vertical wander) and reverses when it gets pinned against an outer wall. This
// makes it actually traverse the world and cross the seam between zones
// regularly — a pure random walk would just diffuse in place and never cross.
//
// Usage:
//   bots [ws_uri] [count] [--move-ms MS]
//     ws_uri      proxy client endpoint   (default ws://127.0.0.1:8766)
//     count       number of bots          (default 6)
//     --move-ms   ms between moves         (default 300)

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde_json::json;
use tokio::time::sleep;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::Message;

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

/// Pick a random 8-direction heading (never standing still).
fn random_heading() -> (i32, i32) {
    let dirs = [
        (1, 0), (-1, 0), (0, 1), (0, -1),
        (1, 1), (1, -1), (-1, 1), (-1, -1),
    ];
    dirs[rand::thread_rng().gen_range(0..dirs.len())]
}

/// One simulated player: connect and wander the world. Each bot keeps its own
/// heading and re-rolls it occasionally, so the population spreads in all
/// directions and crosses region boundaries naturally (no synchronised sweep).
async fn run_bot(index: usize, uri: String, move_ms: u64) {
    loop {
        match connect_async(&uri).await {
            Ok((ws, _)) => {
                println!("[bot {index}] connected");
                let (mut sink, mut stream) = ws.split();
                let mut tick = tokio::time::interval(Duration::from_millis(move_ms));
                let (mut hx, mut hy) = random_heading();

                loop {
                    tokio::select! {
                        // Drain inbound (welcome / status / partition / zone_migration).
                        // Reading also lets tungstenite auto-pong (liveness).
                        incoming = stream.next() => {
                            match incoming {
                                Some(Ok(_)) => {}
                                _ => break, // closed or errored -> reconnect
                            }
                        }
                        _ = tick.tick() => {
                            // Occasionally change direction so paths vary.
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
