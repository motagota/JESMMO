#!/usr/bin/env python3
"""One-time DEM conversion (design doc §5.1, issue #69).

Reprojects a source GeoTIFF DEM to a UTM zone, crops to a bounds_utm
rectangle, resamples to a target resolution, fills NoData (LiDAR gaps,
usually over water) with a fixed sea-level value, and writes the result as a
raw grid file using the exact same binary layout `terrain-bake`'s own
`Grid::encode()` produces (see `terrain-bake/src/grid.rs`):

    u64 LE width
    u64 LE height
    f32 LE cell_size_m
    width * height  f32 LE heights, row-major (row 0 = the source raster's
                    top row after reprojection, i.e. north for a north-up
                    UTM CRS)

`terrain-bake`'s `ingest` module reads this format straight back with
`Grid::decode` — no GDAL/GeoTIFF parsing exists anywhere in the Rust code,
by design (the design doc's stated fallback for a Windows dev box where
system GDAL is fiddly to install).

Requires `rasterio` (bundles its own GDAL — on Windows, `pip install
rasterio` just works, no system GDAL install needed; see
`terrain-bake/README.md`).

Example (the AOI baked into `terrain-bake/testdata/brisbane_hills_demo.grid`,
a hilly slice of the D'Aguilar Range foothills west of Brisbane):

    python convert_dem.py \
        --input 5m_DEM.tif \
        --output terrain-bake/testdata/brisbane_hills_demo.grid \
        --dst-crs EPSG:32756 \
        --res 25 \
        --bounds 484000 6960000 492000 6968000 \
        --sea-level 0
"""

import argparse
import struct
import sys

import numpy as np
import rasterio
from rasterio.transform import Affine
from rasterio.warp import Resampling, reproject, transform_bounds


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--input", required=True, help="Source GeoTIFF DEM path")
    parser.add_argument("--output", required=True, help="Output raw grid path (terrain-bake Grid::encode format)")
    parser.add_argument("--dst-crs", default="EPSG:32756", help="Target CRS (default: UTM zone 56S, Brisbane/Moreton Bay)")
    parser.add_argument("--res", type=float, required=True, help="Target cell size in meters (working_res_m)")
    parser.add_argument(
        "--bounds",
        type=float,
        nargs=4,
        metavar=("X0", "Y0", "X1", "Y1"),
        default=None,
        help="Crop rectangle in the target CRS (left, bottom, right, top). Omit to use the full reprojected extent, rounded to --res.",
    )
    parser.add_argument("--sea-level", type=float, default=0.0, help="Height (m) to fill NoData cells with (default: 0.0)")
    args = parser.parse_args()

    with rasterio.open(args.input) as src:
        if args.bounds is not None:
            x0, y0, x1, y1 = args.bounds
        else:
            x0, y0, x1, y1 = transform_bounds(src.crs, args.dst_crs, *src.bounds)
            # Round outward to a whole number of cells so the fixture's shape is exact.
            x0, y0 = np.floor(x0 / args.res) * args.res, np.floor(y0 / args.res) * args.res
            x1, y1 = np.ceil(x1 / args.res) * args.res, np.ceil(y1 / args.res) * args.res

        cols = int(round((x1 - x0) / args.res))
        rows = int(round((y1 - y0) / args.res))
        if cols <= 0 or rows <= 0:
            sys.exit(f"--bounds/--res produced a non-positive grid shape ({cols}x{rows}); check --bounds ordering")

        dst_transform = Affine(args.res, 0, x0, 0, -args.res, y1)
        dst = np.full((rows, cols), np.nan, dtype="float32")
        reproject(
            source=rasterio.band(src, 1),
            destination=dst,
            src_transform=src.transform,
            src_crs=src.crs,
            src_nodata=src.nodata,
            dst_transform=dst_transform,
            dst_crs=args.dst_crs,
            dst_nodata=np.nan,
            resampling=Resampling.bilinear,
        )

        nodata_count = int(np.isnan(dst).sum())
        dst = np.where(np.isnan(dst), np.float32(args.sea_level), dst)

    with open(args.output, "wb") as f:
        f.write(struct.pack("<QQf", cols, rows, args.res))
        f.write(np.ascontiguousarray(dst, dtype="<f4").tobytes())

    print(f"wrote {args.output}: {cols}x{rows} cells at {args.res}m")
    print(f"  UTM bounds: [{x0}, {y0}, {x1}, {y1}]")
    print(f"  height range: {np.nanmin(dst):.2f}m .. {np.nanmax(dst):.2f}m")
    print(f"  NoData cells filled with sea level ({args.sea_level}m): {nodata_count} ({100 * nodata_count / dst.size:.2f}%)")


if __name__ == "__main__":
    main()
