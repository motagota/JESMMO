## Wire-protocol mirror of `docs/protocol.md` / `mmo::protocol`.
##
## Single source of truth for message type strings and the protocol version the
## client was built against, plus the movement/render tuning that must match the
## server's authoritative model. Keep in sync with the Rust `protocol.rs`.
class_name Protocol
extends RefCounted

## Bumped on incompatible changes; sent in handshake frames so the gateway can
## refuse a mismatched client (see proxy `run_handshake`).
const VERSION := 1

# --- server -> client ---------------------------------------------------------
const S_AUTH_REQUIRED := "auth_required"
const S_AUTH_OK := "auth_ok"
const S_AUTH_ERROR := "auth_error"
const S_WELCOME := "welcome"
const S_PARTITION := "partition"
const S_STATUS_UPDATE := "status_update"
const S_DESPAWN := "despawn"
const S_ZONE_MIGRATION := "zone_migration"
const S_ZONE_CAPTURE := "zone_capture"
const S_YOU_DIED := "you_died"

# --- client -> server ---------------------------------------------------------
const C_REGISTER := "register"
const C_LOGIN := "login"
const C_TOKEN := "token"
const C_GUEST := "guest"
const C_MOVE := "move"
const C_ATTACK := "attack"

# --- gameplay: gathering / inventory / skills / storage (M2) ------------------
const C_GATHER_START := "gather.start"
const C_GATHER_STOP := "gather.stop"
const S_GATHER_PROGRESS := "gather.progress"
const S_GATHER_RESULT := "gather.result"
const S_INV_UPDATE := "inv.update"
const S_SKILL_UPDATE := "skill.update"
const C_STORE_DEPOSIT := "store.deposit"
const C_STORE_WITHDRAW := "store.withdraw"
const S_STORE_UPDATE := "store.update"

# --- gameplay: build orders (M2) ----------------------------------------------
## `build.list` is bidirectional: the client sends it to request the district's
## board; the server also pushes it (hydration / after an unlock) with `orders`.
const C_BUILD_LIST := "build.list"
const S_BUILD_LIST := "build.list"
const C_BUILD_CONTRIBUTE := "build.contribute"
const S_BUILD_PROGRESS := "build.progress"
const S_BUILD_COMPLETED := "build.completed"
const S_BUILD_UNLOCKED := "build.unlocked"

## Must be within this many world units of a node to gather it (mirrors the server).
const GATHER_RANGE := 50.0
## Must be within this of a storage point to deposit/withdraw (mirrors the server).
const STORAGE_RANGE := 60.0
## Must be within this of a build board to contribute (mirrors the server).
const BOARD_RANGE := 60.0

# --- movement / render tuning (mirrors client.html and the server) ------------
## World units sent per move tick, per axis. The server applies the delta directly.
const MOVE_STEP := 10
## Seconds between move sends (~16/s) — a steady cadence, not OS key-repeat.
const MOVE_TICK := 0.06
## Accept the server's position as a correction only past this drift (units), so
## local prediction stays smooth between authoritative snapshots.
const RECONCILE_DRIFT := 30.0
## World units -> metres in the 3D scene (1200-unit world -> 120 m).
const WORLD_SCALE := 0.1

## Map a server world position `(wx, wy)` to a ground-plane point in the 3D scene.
## The server's Y axis becomes the scene's Z axis; height (Y) is gameplay-flat.
static func w2v(wx: float, wy: float, y: float = 0.0) -> Vector3:
	return Vector3(wx * WORLD_SCALE, y, wy * WORLD_SCALE)
