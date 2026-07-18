#!/usr/bin/env python3
"""Seed the starting-area poison-forest pen (#90, player-attributes epic #83).

Placed poison trees live in the world_object table (dev: mmo_dev.db), not in
the repo — run this against a running gateway to (re)author the pen on a
fresh database. Idempotent: any planned tree that already has a poison tree
within DEDUPE_M of its spot is skipped, so it's safe to re-run (and to resume
a partial run).

The pen (geometry read off the baked water mask; #90, west wall moved out in
#98, extended around Mt Coot-tha in #99):

    (6200,11900) ─────────────── north wall ─────────────── (15500,11900)
        │                                                        │
     west wall   MT COOT-THA (summit 6800,14000, h=281)      east wall
        │           QUARRY (~8232,13915) ── Milton Rd ──┐        │
        │           ROUNDABOUT (~8500,14250)     SPAWN (12800,12800)
        │                                                   (15500,14650)
    (6200,16700) ────── SW leg ────── (11650,16700)   east channel bank ┘
                                       └ west channel bank

The south/southeast side is the Brisbane River itself (west channel → CBD
S-bend → east channel — one continuous snake); each wall's far end anchors
into a river bank, so forest + water close the loop. (There is NO water west
of ~x=11600 at these latitudes — probed in #98 — hence the L: the west wall
runs south BEHIND Mt Coot-tha, then the SW leg runs east along y=16700 into
the channel; the leg fords a small creek mid-span, where trees simply stand
in the shallows.) Walls are 3 staggered rows, ROW_GAP_M apart, SPACING_M
along the line: with the 15m poison radius that yields >=1 poison source
everywhere in a ~55m-deep band — crossing at walk speed (8 m/s) is ~7s of
exposure, which procs (uncurable) well before the far edge.

Usage:
    python scripts/seed_poison_pen.py [--url ws://127.0.0.1:8766]
    python scripts/seed_poison_pen.py --prune   # also DELETE placed trees
        # that aren't in the current plan — a wall relocation (like #98's
        # west-wall move) is one idempotent --prune run.
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

# (x0, y0) -> (x1, y1) wall centre-lines, in world metres.
WALLS = [
    ((6200, 16700), (6200, 11900)),  # west: SW corner -> NW corner, behind Mt Coot-tha (#99)
    # SW leg: the river has no water west of ~x=11600 here, so the pen
    # closes with an east-running leg that anchors into the west channel
    # bank (~11600-11800 at y=16700).
    ((6200, 16700), (11650, 16700)),
    ((6200, 11900), (15500, 11900)),  # north: NW corner -> NE corner
    # East: NE corner -> river. The east channel is a meander whose two water
    # strips leave a walkable land corridor at x~15500, y 14050-14550 (found
    # by the #90 perimeter probe at azimuth 30deg) — the wall runs THROUGH
    # that corridor into the southern strip, so it can't be threaded.
    ((15500, 11900), (15500, 14650)),  # east: NE corner -> river bank
]
ROWS = (-13, 0, 13)  # perpendicular row offsets, metres
SPACING_M = 15       # along-line tree spacing
STAGGER_M = 7        # alternate rows shift half a step (no see-through lanes)
DEDUPE_M = 6         # a planned spot with an existing tree this close is done


def plan() -> list[tuple[int, int]]:
    """Every tree position the pen wants, deduplicated on a coarse grid."""
    spots: dict[tuple[int, int], tuple[int, int]] = {}
    for (x0, y0), (x1, y1) in WALLS:
        length = ((x1 - x0) ** 2 + (y1 - y0) ** 2) ** 0.5
        ux, uy = (x1 - x0) / length, (y1 - y0) / length  # along the wall
        px, py = -uy, ux  # perpendicular
        for row_i, off in enumerate(ROWS):
            shift = STAGGER_M if row_i % 2 else 0
            d = float(shift)
            while d <= length:
                x = round(x0 + ux * d + px * off)
                y = round(y0 + uy * d + py * off)
                spots[(x // DEDUPE_M, y // DEDUPE_M)] = (x, y)
                d += SPACING_M
    return list(spots.values())


async def main(url: str, prune: bool) -> None:
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

        await ws.send(json.dumps({"type": "object.list"}))
        existing = [(o["id"], int(o["x"]), int(o["y"])) for o in (await until("object.list"))["objects"]
                    if o.get("kind") == "poison_tree"]
        # Bucket existing trees for the dedupe lookup.
        occupied = set()
        for _oid, ex, ey in existing:
            occupied.add((ex // DEDUPE_M, ey // DEDUPE_M))

        wanted = plan()
        wanted_buckets = {(x // DEDUPE_M, y // DEDUPE_M) for x, y in wanted}
        todo = [(x, y) for x, y in wanted if (x // DEDUPE_M, y // DEDUPE_M) not in occupied]
        orphans = [(oid, x, y) for oid, x, y in existing
                   if (x // DEDUPE_M, y // DEDUPE_M) not in wanted_buckets]
        print(f"pen plan: {len(wanted)} trees; {len(existing)} already placed; "
              f"placing {len(todo)}; {len(orphans)} orphans"
              + (" (pruning)" if prune else " (kept — rerun with --prune to remove)"))

        placed = 0
        for x, y in todo:
            await ws.send(json.dumps({"type": "object.place", "kind": "poison_tree", "x": x, "y": y}))
            m = json.loads(await asyncio.wait_for(ws.recv(), 15))
            while m.get("type") not in ("object.placed", "object.edit_error"):
                m = json.loads(await asyncio.wait_for(ws.recv(), 15))
            if m["type"] == "object.edit_error":
                raise SystemExit(f"place at ({x},{y}) rejected: {m.get('message')}")
            placed += 1
            if placed % 200 == 0:
                print(f"  ...{placed}/{len(todo)} placed")

        pruned = 0
        if prune:
            for oid, x, y in orphans:
                await ws.send(json.dumps({"type": "object.delete", "object_id": oid}))
                m = json.loads(await asyncio.wait_for(ws.recv(), 15))
                while m.get("type") not in ("object.removed", "object.edit_error"):
                    m = json.loads(await asyncio.wait_for(ws.recv(), 15))
                if m["type"] == "object.edit_error":
                    raise SystemExit(f"delete of {oid} at ({x},{y}) rejected: {m.get('message')}")
                pruned += 1
                if pruned % 200 == 0:
                    print(f"  ...{pruned}/{len(orphans)} pruned")
        print(f"done: placed {placed}, pruned {pruned} (pen total ~{len(wanted)})")


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default="ws://127.0.0.1:8766")
    ap.add_argument("--prune", action="store_true",
                    help="delete placed poison trees that aren't in the current plan")
    main_args = ap.parse_args()
    asyncio.run(main(main_args.url, main_args.prune))
