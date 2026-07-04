# Rust server

Rust port of the Python server (`../server/proxy.py` and `../server/zoneServer.py`).
Two binaries:

- **proxy** — routes client websockets to zones, supports 3-phase migration via stdin commands.
- **zone_server** — holds player/bot state, self-registers with the proxy, random-walk bots.

The browser client (`../client/client.html`) and `../client/test_client.py` work unchanged;
the proxy listens on the same ports as the Python version.

## Ports

- `ws://127.0.0.1:8766` — client connections
- `ws://127.0.0.1:8764` — zone registration
- `ws://127.0.0.1:8767` — admin / management UI

## Build

```sh
cargo build --release
```

## Run

In separate terminals:

```sh
# 1. proxy
cargo run --release --bin proxy

# 2. one or more zones (zone_id, port, proxy registration uri)
cargo run --release --bin zone_server zone_a 9001 ws://127.0.0.1:8764
cargo run --release --bin zone_server zone_b 9002 ws://127.0.0.1:8764
```

Then open `../client/client.html` in a browser.

## Accounts & persistence (M0)

The gateway now has durable identity. On connect it asks the client to authenticate
(`Login` / `Register` / `Play as guest` in the browser client):

- **Register / Login** — a real account backed by SQLite. Your character's position
  is saved periodically and on disconnect, so logging back in (even after a server
  restart) restores you where you were.
- **Guest** — an ephemeral character (the old behaviour); nothing is persisted.

The database is created automatically on first run:

- Default: a `mmo_dev.db` SQLite file in the working directory.
- Override with `DATABASE_URL` to point at a different SQLite file
  (e.g. `sqlite://my_other.db`). **SQLite only for now** — `Db::connect` is
  built on `SqlitePool`, so a Postgres URL will not work despite the schema
  in `migrations/` being written to be driver-portable. Postgres support for
  staging/prod is tracked as future work.

The load-test bots and any older client keep working without changes: a client that
sends gameplay frames without authenticating is treated as a guest.

See `../docs/protocol.md` for the full handshake and message catalogue.

## Management UI

Open `../client/admin.html` in a browser. It connects to the admin port
(`ws://127.0.0.1:8767`) and shows:

- total players online and per-zone player counts (refreshed every second)
- each zone's migration state (`normal` / `marking` / `migrating` / `retired`)
- buttons to issue a migration (auto, or individual phases) between a selected
  source and target zone

This is the same set of actions as the stdin commands below, just driven from
the browser.

## Migration commands (type into the proxy's stdin)

```
migrate phase1 <zone_id>                    # mark zone for migration (buffer packets)
migrate phase2 <source_zone> <target_zone>  # transfer players
migrate phase3 <zone_id>                    # retire zone
migrate auto <source_zone> <target_zone>    # run all three phases
```
