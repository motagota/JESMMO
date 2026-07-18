#!/usr/bin/env python3
"""Seed MILTON ROAD (#99, roads & quarry epic #93) — the inaugural road plan:
town centre to the Mt Coot-tha roundabout, plus the roundabout ring itself
and the quarry spur. The world is a real-Brisbane bake, and these follow the
real geography: Mt Coot-tha's 281m summit sits at world (6800, 14000), the
quarry face on its NE bench (~8232, 13915), the roundabout at the slope's
base (~8500, 14250), and the road runs WSW from the town centre the way the
real Milton Road does.

These are *plans*: each becomes one open build order (stone scaled by
length, ~900 + 533 + 30 + 136 stone) that players fulfil by hauling quarry stone
— the inaugural community build. Roads live in the DB, not the repo: run
this against a running gateway to (re)author them on a fresh database.
Idempotent: a plan whose exact start point already appears in an existing
civic road order's path is skipped.

Usage: python scripts/seed_milton_road.py [--url ws://127.0.0.1:8766]
"""

import argparse
import asyncio
import json

try:
    import websockets
except ImportError:
    raise SystemExit("pip install websockets")

EDITOR_EMAIL = "editor@capital.town"
EDITOR_PASSWORD = "editor12345"

# Milton Road: town centre -> the Mt Coot-tha roundabout. Waypoint diagonals
# are laid as x-run-then-y-run staircases (the 1m grid's axis-aligned rule);
# every corner was probed dry against the bake.
# Split into two staged plans: a single plan is capped at 4km server-side
# (ROAD_MAX_LENGTH_M), and Milton Road is ~5.7km of staircase.
_MILTON_EAST = [
    (12800, 12800),  # town centre
    (12000, 13000),
    (11000, 13300),
    (10000, 13600),
]
_MILTON_WEST = [
    (10000, 13600),
    (9200, 13850),
    (8515, 14250),   # arrives at the roundabout's east side
]

def _staircase(waypoints):
    pts = [list(waypoints[0])]
    for (x0, y0), (x1, y1) in zip(waypoints, waypoints[1:]):
        if x1 != x0:
            pts.append([x1, y0])
        if y1 != y0:
            pts.append([x1, y1])
    return pts

PLANS = {
    "Milton Road (east)": _staircase(_MILTON_EAST),
    "Milton Road (west)": _staircase(_MILTON_WEST),
    # The Mt Coot-tha roundabout: a 30m ring at the base of the climb.
    "Mt Coot-tha roundabout": [
        [8485, 14235], [8515, 14235], [8515, 14265], [8485, 14265], [8485, 14235],
    ],
    # The quarry spur: from the ring's west side up to the working face.
    "Quarry spur": _staircase([(8485, 14250), (8260, 14250), (8260, 13930)]),
}


async def main(url: str) -> None:
    async with websockets.connect(url, max_size=16 * 1024 * 1024) as ws:
        async def until(t: str):
            while True:
                m = json.loads(await asyncio.wait_for(ws.recv(), 15))
                if m.get("type") == t:
                    return m

        await ws.send(json.dumps({"type": "login", "email": EDITOR_EMAIL,
                                  "password": EDITOR_PASSWORD, "protocol_version": 1}))
        w = await until("welcome")
        if w.get("role") != "editor":
            raise SystemExit(f"need the editor account (got role {w.get('role')})")

        # Existing civic road paths, for idempotency (the editor spawns in civic).
        await ws.send(json.dumps({"type": "build.list"}))
        board = await until("build.list")
        existing_starts = set()
        for o in board.get("orders", []):
            path = o.get("path")
            if path:
                existing_starts.add((int(path[0][0]), int(path[0][1])))

        for name, pts in PLANS.items():
            start = (pts[0][0], pts[0][1])
            if start in existing_starts:
                print(f"{name}: already planned (start {start}) — skipped")
                continue
            await ws.send(json.dumps({"type": "road.plan", "points": pts}))
            while True:
                m = json.loads(await asyncio.wait_for(ws.recv(), 15))
                if m.get("type") == "road.planned":
                    length = sum(abs(b[0] - a[0]) + abs(b[1] - a[1]) for a, b in zip(pts, pts[1:]))
                    print(f"{name}: planned as order {m['order_id']} ({length}m, ~{max(length // 4, 5)} stone)")
                    break
                if m.get("type") == "road.plan_error":
                    raise SystemExit(f"{name}: rejected — {m.get('message')}")


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default="ws://127.0.0.1:8766")
    asyncio.run(main(ap.parse_args().url))
