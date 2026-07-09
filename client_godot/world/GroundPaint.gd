## Ground vertex-color painting: district safety tint + river silt-brown.
##
## Extracted verbatim from `World.gd`'s original `_safety_color_at`/
## `_ground_color_at` (no logic changes) so both the whole-world coarse
## backdrop (`World._build_ground`) and the native-resolution streamed tiles
## (`TerrainStreamer`) paint identically — pure functions of `(zones,
## world_size, wx, wy)`, no dependency on which mesh/resolution is asking.
class_name GroundPaint
extends RefCounted

## Base ground look and its safe/wilds-tinted variants (design intent: a
## subtle green/red tint over the same base color, painted directly into
## the ground mesh's vertex colors rather than a separate floating plane).
const _GROUND_BASE_COLOR := Color(0.10, 0.14, 0.10)
const _GROUND_SAFE_COLOR := Color(0.11, 0.22, 0.14)
const _GROUND_WILDS_COLOR := Color(0.20, 0.13, 0.11)

## Purely a visual read of the real DEM's own elevation (issue #69's
## production bake) — no server round-trip, no nav/biome data needed.
## Deliberately just a color, not a nav-blocking water mask (terrain.toml's
## [water].sea_level_m stays low) — the real Brisbane River is famously
## muddy brown, not blue, so this is truer to the place than a generic
## water-blue would have been anyway.
##
## The wire heightmap the backdrop mesh uses is coarse (`Protocol
## .terrain_resolution()` corners, ~133m apart on the production world, far
## coarser than the real bake's own 5m fidelity) and Gouraud-shaded across
## each cell — a single low *corner* smears its color across every triangle
## touching it. A hard cutoff at a height that matches the real river's true
## width therefore visibly over-paints far more area than the real channel
## covers. Blending across a band instead (full brown only at the channel's
## genuinely deep points, fading out entirely by `_RIVER_FADE_M`) keeps only
## the deepest points solid and lets the smeared fringe read as a muddy bank
## tapering into normal ground, instead of a flat wash of brown. Streamed
## native-resolution tiles (see `TerrainStreamer`) don't have this smearing
## problem (their own vertices are only ~5m apart), but use the same blend
## band for a visually consistent look across both mesh tiers.
const _RIVER_FULL_M := -4.0   # at or below this height: fully river-brown
const _RIVER_FADE_M := 1.0    # at or above this height: no river tint at all
const _RIVER_COLOR := Color(0.35, 0.27, 0.16)

## The safety-only ground color for world point `(wx, wy)`: the base ground
## color tinted toward green (safe) or red (wilds) per whichever zone in
## `zones` (a `partition` message's raw zone-entry array) contains it.
## Safety is a static property of a district's identity (`Safety::Safe`/
## `Wilds` in `world.rs`, never redrawn by later re-partitioning/zone-splits
## — only the shard boundaries change, not which world positions are safe),
## so painting this once at mesh-build time never goes stale.
static func safety_color_at(zones: Array, world_size: float, wx: float, wy: float) -> Color:
    # Clamp to just inside the world bounds: a vertex sampled exactly at the
    # world's far edge (`wx == world_size` or `wy == world_size`, which the
    # backdrop's last row/column of vertices always does) would otherwise
    # satisfy no zone's exclusive `< x1`/`< y1` bound and fall through to the
    # neutral fallback color -- a visible one-vertex-wide untinted seam
    # around the whole map's perimeter.
    var qx := minf(wx, world_size - 0.01)
    var qy := minf(wy, world_size - 0.01)
    for entry_v in zones:
        var z: Dictionary = entry_v
        if qx >= float(z.get("x0", 0)) and qx < float(z.get("x1", 0)) \
                and qy >= float(z.get("y0", 0)) and qy < float(z.get("y1", 0)):
            return _GROUND_SAFE_COLOR if String(z.get("safety", "wilds")) == "safe" else _GROUND_WILDS_COLOR
    return _GROUND_BASE_COLOR

## The full ground-paint color for `(wx, wy)`: the safety color, blended
## toward river silt-brown as height drops from `_RIVER_FADE_M` down to
## `_RIVER_FULL_M` (see that constant's doc comment for why this is a band,
## not a hard cutoff). Reads height via `Protocol.terrain_height`, which
## itself prefers a loaded native-resolution tile when one covers `(wx,wy)`
## and falls back to the coarse backdrop grid otherwise -- this function
## doesn't need to know or care which.
static func ground_color_at(zones: Array, world_size: float, wx: float, wy: float) -> Color:
    var safety := safety_color_at(zones, world_size, wx, wy)
    var h := Protocol.terrain_height(wx, wy)
    if h >= _RIVER_FADE_M:
        return safety
    var t := 1.0 if h <= _RIVER_FULL_M else (_RIVER_FADE_M - h) / (_RIVER_FADE_M - _RIVER_FULL_M)
    return safety.lerp(_RIVER_COLOR, smoothstep(0.0, 1.0, t))
