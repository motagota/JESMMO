# terrain-bake

Offline terrain bake pipeline (terrain pipeline epic, [#56](https://github.com/motagota/JESMMO/issues/56)). Turns a DEM (or, before real ingest, a deterministic synthetic placeholder) into the tiled artifact `terrain-common` reads — the same one the server and Godot client both load.

```
cargo run -p terrain-bake -- --config terrain.toml
```

writes `artifacts/world_v2/` (the production capital bake: real Brisbane DEM at native 5m resolution, 100 tiles — see `terrain.toml`'s own comments). `--stage <ingest|water|stylize|detail|erode|classify|export>` runs a single stage; `--debug-dump <dir>` writes a hillshade/water-mask/biome-map PNG at every stage boundary; `--force` bypasses the per-stage cache.

## Real DEM ingest (issue #69)

Getting a real DEM in requires two things this repo can't automate:

1. **The source file.** Geoscience Australia's [ELVIS portal](https://elevation.fsdf.org.au/) requires manually drawing an AOI polygon and registering for delivery — get a 5m LiDAR DTM GeoTIFF covering the area you want (SRTM 30m works too for quick iteration, at the cost of canopy-height contamination on forested ranges — don't ship that as a final artifact).
2. **Reprojection/cropping/resampling**, done once outside this tool by `tools/convert_dem.py`, *not* by a `gdal` crate dependency in Rust. `terrain-bake` itself has zero GeoTIFF-parsing code — it just reads back the small raw grid the script writes (see `src/ingest.rs`).

### Getting GDAL working on Windows

The design doc assumes "a GDAL install" without saying what that takes on Windows. What actually worked, with **no system GDAL, no OSGeo4W, no conda** required:

```
python -m venv .venv
.venv\Scripts\pip install rasterio numpy
```

`rasterio`'s Windows wheels bundle their own GDAL binaries — `pip install rasterio` alone gets you a fully working GDAL. If you already have a system GDAL you'd rather use, `pip install gdal` works too, but it's the fiddlier path (needs a matching prebuilt wheel or a C++ build toolchain) and wasn't necessary here.

### Converting a DEM

```
python tools/convert_dem.py \
    --input path/to/your_dem.tif \
    --output terrain-bake/testdata/your_area.grid \
    --dst-crs EPSG:32756 \
    --res 25 \
    --bounds 484000 6960000 492000 6968000 \
    --sea-level 0
```

- `--dst-crs` — UTM zone 56S (EPSG:32756) covers Brisbane/Moreton Bay; pick whatever zone matches your AOI.
- `--bounds` — crop rectangle in the *target* CRS (left, bottom, right, top), in meters. Omit to use the DEM's full reprojected extent instead.
- `--res` — target cell size in meters; this becomes `working_res_m` in your `terrain.toml`'s `[source]` section.
- `--sea-level` — LiDAR DTMs have NoData gaps over water; filled with this height so downstream stages never see a hole.

Point a config's `[source] dem_path` at the script's output and every stage runs exactly as it does against the synthetic placeholder — no code differences, no special-casing.

### The demo fixture

`testdata/brisbane_demo.toml` + `testdata/brisbane_hills_demo.grid` (committed, ~400KB) is a real 320x320-cell / 25m slice of the D'Aguilar Range foothills west of Brisbane, produced exactly as shown above from a real Geoscience Australia 5m DTM. It exists to prove — and keep proving, since it's exercised by `tests/pipeline_validation.rs`-style runs — that real DEM data flows through every stage unmodified. It is **not** the production capital bake (that's the repo-root `terrain.toml`; see its own comments for why it stays synthetic).

Regenerate its debug dumps with:

```
cargo run -p terrain-bake -- --config terrain-bake/testdata/brisbane_demo.toml --debug-dump <some-dir>
```

`ingest_hillshade.png` should show real, natural drainage-pattern relief (ridgelines and valleys), visibly different from the synthetic placeholder's smoother, hand-shaped noise.
