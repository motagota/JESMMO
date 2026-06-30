# JESMMO — Godot 3D Client (Phase 1 skeleton)

The net-new 3D client for the capital, built with **Godot 4.4+ / GDScript**
(developed against 4.4; parse/smoke-checked on 4.6). It connects to the gateway,
logs in, renders a district in 3D, and moves the local player with client-side
prediction while the server stays authoritative. The 2D
[`client/client.html`](../client/client.html) remains as a debug/admin spectator.

Implements issue **#6** (M1).

## Layout

```
client_godot/
  project.godot          engine config; main scene = Main.tscn
  Main.tscn / Main.gd    builds the scene tree in code and wires signals
  net/Protocol.gd        wire-protocol mirror of docs/protocol.md (types, version, tuning)
  net/NetworkClient.gd   WebSocket + JSON codec + typed signal dispatch
  world/World.gd         ground, district tiles (named, tinted by safety), roads, town centre
  world/EntityManager.gd remote players/mobs; spawn, interpolate, despawn
  player/LocalPlayer.gd  input -> move, prediction + reconciliation, 3rd-person camera
  ui/Login.gd            login / register / guest overlay (code-built)
  ui/Hud.gd              connection / zone / position readout
  tests/smoke.gd         headless end-to-end network smoke test
```

The scene tree is assembled in code (only a one-node `Main.tscn`), so the project
reviews as plain script diffs without opening the editor.

## Run it

1. Start the server (from the repo root), keeping the proxy's stdin open:
   ```sh
   cd rust_server && cargo run --bin proxy &           # gateway on :8766
   cargo run --bin zone_server -- zone_a 9001 ws://127.0.0.1:8764 &
   ```
   (On Windows, `..\start_servers.ps1` launches both in their own windows.)
2. Open `client_godot/` in Godot 4.4+ and press **F5**, or run the main scene from
   the CLI:
   ```sh
   Godot --path client_godot
   ```
3. Log in / register / play as guest. Move with **WASD** or the arrow keys;
   **Space** swings; **E** gathers the nearest resource node in range (walk up to a
   green tree or grey rock). Your inventory and gathering skill show in the HUD;
   gathered items persist for logged-in characters.

The gateway is `ws://127.0.0.1:8766` (see `Main.gd::GATEWAY_URL`). A session token
is cached in `user://session.cfg` for silent reconnects.

## How it works

- **Handshake** (`Main` + `NetworkClient`): connect → `auth_required` → resume a
  saved token, or `login`/`register`/`guest` → `auth_ok` (token stored) →
  `welcome` (spawn) → `partition` (draw districts) → `status_update` stream. The
  client sends its `protocol_version`, so a mismatched build is refused cleanly.
- **Prediction + reconciliation** (`LocalPlayer`): every `MOVE_TICK` (60 ms) the
  client sends a `move {dx,dy}` delta **and** applies it locally so input feels
  instant. An authoritative self-`status_update` only snaps the position when it
  has drifted past `RECONCILE_DRIFT` (after a migration, respawn, or world-edge
  clamp). True input-replay reconciliation needs sequence numbers the protocol
  doesn't carry yet — noted for a later pass.
- **Remote entities** (`EntityManager`): other players/mobs ease toward their last
  authoritative position each frame (20 Hz snapshots → display-rate interpolation).

## Verify (headless)

Parse-check every script:
```sh
Godot --headless --path client_godot --editor --quit
```

End-to-end smokes (require the server running, per "Run it"):
```sh
Godot --headless --path client_godot -s res://tests/smoke.gd         # connect/auth/welcome
Godot --headless --path client_godot -s res://tests/smoke_gather.gd  # register, walk, gather wood
```
`smoke.gd` expects `SMOKE_OK welcome …` then a `status_update` stream;
`smoke_gather.gd` registers a character, walks to the civic tree, gathers, and
expects `SMOKE_GATHER_OK inventory shows wood`. Both exit non-zero on timeout.

## Scope (Phase 1, M1)

In: connect, auth, render one district (ground/roads/town centre), move with
prediction, see other players. Deferred to later milestones: the gameplay UIs
(inventory, build-order board, skills, rent), build/place mode, and structure
models — the protocol already reserves those message domains.
