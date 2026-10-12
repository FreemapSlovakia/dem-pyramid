# dem-pyramid

Build scripts for Freemap's elevation pyramid — the shared substrate for
panorama, viewshed and (later) cast shadows.

Authored here and deployed to fm6 by pushing to `main`, where the build
directory is a checkout of this repository. All data lives on fm6 under
`/fm/storage2/dem`.

## Layout

```
src/                    dem-tool: source measurement, staleness check, index plan, renders
bin/index.sh            write the per-level indexes
bin/run.sh              deprioritised, logged step runner
bin/sync.sh             push to fm6 and build there
```

`dem-tool` reads the index through libgdal, and shells out to `gdalinfo -json`
and `ogr2ogr` where a tool's output is simpler to read than the bindings.

The binary is built **on fm6** — its glibc (2.41) is older than the
workstation's (2.42), so a locally built binary would not run there.

On fm6:

```
/fm/storage2/dem/
  build/                this repo
  state/sources.json    what `refresh` measured: every raster the index reads
  index/z{14..8}.gti.gpkg   one GTI per level, rows only
  logs/ state/ tmp/
```

## Design in one paragraph

The pyramid holds no pixels. It reads the rasters terrain-tiles serves for
hillshading — one Cloud-Optimized GeoTIFF per country, EPSG:3857 on the XYZ
grid of its zoom, Int32 in 2 mm steps, overviews down to z8 — through one GTI
index per level. Each row is a `vrt://` connection string that picks the
raster's overview for that level and converts it on read to the marcher's
uint16 decimetres (`scale=-250000,3026750,0,65535`, nodata 0); GDAL rounds the
conversion and leaves nodata as 0. Priority is a sort column, so overlap is
resolved at read time and nodata falls through to whatever is beneath. A raster
contributes only to levels at or coarser than its own zoom, so gedtm30 (z13) is
absent from z14 and the marcher falls back to the finest level that has data.

## Grid

EPSG:3857 at standard XYZ resolutions. Level `z` has a **projected** pixel size
of `156543.03392804097 / 2^z` — projected metres, not ground metres. Ground
resolution at 49N is `102700 / 2^z`, so z14 is 6.27 m there, 3.0 m in northern
Norway and 7.8 m in southern Spain. The marcher picks a level from the *local*
ground cell size, not from a fixed distance table.

## Usage

Everything below runs on fm6 in `/fm/storage2/dem/build`. A push to `main`
deploys the binary and the unit; `bin/sync.sh` pushes work in progress without
one. Long steps go in tmux and log to `$DEM_ROOT/logs/<step>.log`, so nothing
depends on staying attached.

**Start with `check`.** It re-measures every raster and names whatever no
longer matches the cache, ending with the command that fixes it.

| to | run |
| --- | --- |
| see what the pyramid reads | `./target/release/dem-tool list` |
| find out whether the cache is current | `./target/release/dem-tool check` |
| bring the cache up to date | `./target/release/dem-tool refresh` |
| make the cache visible to renders | `bin/run.sh index bash bin/index.sh` |

`index.sh` takes levels to narrow it, and `INDEX_DIR` to write somewhere other
than `index/`. Each level is written to a temporary file and moved into place,
so killing it leaves the previous index; a full run takes minutes.

A change in the sources — a dataset added, removed or renumbered in the source
list, or its raster rebuilt at another zoom — needs `refresh`, then `index.sh`,
then `systemctl restart terrain`. `serve` reads `state/sources.json` and the elevation source list for
crediting at startup, and refuses to start while a cached source is missing
from the list, so restart only after `refresh`.

The sources are not configured here. They come from
[`elevation-sources`](https://github.com/FreemapSlovakia/elevation-sources),
checked out on fm6: every dataset that links a `mercator.tif` — the raster
terrain-tiles serves — is in the pyramid, read from that raster; its
directory's number is its priority (`010-sk` outranks `999-gedtm30`), and its
`source.json` credits it. The id is the directory without its number:
`100-es-30` is `es_30`. Everything else is measured from the raster, and
`refresh` refuses one that is off the z8 pixel grid, in other units, or short
of an overview.

Vertical datums differ across sources (RH2000, IGN69, LN02, EGM2008 …).
Offsets are decimetre-scale — invisible to the ray marcher, visible in a
hillshade seam check.

## Panorama

```sh
./target/release/dem-tool panorama --lon 18.64 --lat 48.63 \
    --az 20 --fov 90 --alt-min=-4 --alt-max=5 --out view.png
```

Note `--alt-min=-4` with the `=`: clap reads a bare `-4` as a flag.

Casts one ray per azimuth over the pyramid and builds a **distance buffer** --
distance to visible terrain per (azimuth, elevation-angle) cell, infinity for
sky. The render is derived from that buffer alone; skyline, haze and
silhouettes are not computed separately.

Marching runs near → far keeping a running maximum elevation angle: a sample is
visible exactly when its angle beats every nearer one, and the band of angles
between the old maximum and the new one is terrain at that distance. Each
column fills top-down in one pass, and a column that fills completely stops
early.

Level is chosen per sample -- the coarsest whose ground cell still resolves the
angular step at that distance -- which is what keeps cost logarithmic in range.
A full 360° at 0.05°/px marches ~52 M samples in ~15 s, touching ~400 cached
blocks (well under 100 MB).

Sampling is bilinear. With nearest-neighbour, adjacent rays at 2 km are 1.75 m
apart against 6.27 m cells, so many rays return the same cell and the skyline
comes out as a staircase.

### Supersampling

Renders at `--supersample-x` × `--supersample-y` the output resolution and
box-averages down. Both default to 9, and both are needed, for different
reasons:

- **Horizontal** — at long range several DEM cells fall inside one pixel's
  angular footprint (~2.9 cells at 100 km), and a single ray picks one
  arbitrary value out of them. Only real samples can average that.
- **Vertical** — sub-pixel placement *is* analytic, but only for one edge per
  cell. The buffer holds a single surface per cell, so where several ridge
  bands fall inside one output pixel, all but the nearest are discarded and
  never stroked. Extra rows give each band a cell of its own.

Rays scale with the horizontal factor only; vertical rows cost no marching.
Disk I/O does not change at all (same 396 blocks at every setting) — the extra
work is arithmetic over data already cached. Buffer memory is the *product* of
the two, which is the real limit.

Below 9 the far skyline visibly degrades.

Rendering **streams one output column at a time**: an output column depends
only on its own sub-columns, and shading is column-local (edge detection looks
only at the rows above and below within a column), so each column is marched,
shaded, averaged and discarded. Materialising the whole supersampled buffer
instead cost 6.7 GB for a 360° frame at 9×9; streaming it costs 848 MB, for
the same rays and the same time.

| view | rays | time | peak RSS |
|---|---|---|---|
| 90°, 9×9 | 117M | 5.8 s | ~250 MB |
| 360°, 9×9 | 465M | 20.3 s | 848 MB |

A further ~37% of ray cost is available by supersampling only beyond 7.2 km.
That distance is not a tuned guess: below it the level rule pins the finest
level, so the DEM is finer than the ray spacing and adjacent rays land in the
same cell; above it the chosen cell size equals the ray spacing by
construction, so neighbouring rays sample different cells and need averaging.

### Peaks and depth

`--peaks <gpkg>` writes a JSON sidecar: each visible summit's position in the
image, distance, azimuth, elevation angle, DTM elevation and **dominance** --
metres above the terrain beside it at its own depth, signed, so a top its own
ridge stands over scores below zero. Costs no extra rays: projection is
arithmetic, and each summit is answered from the render's own column at its
bearing, which the marcher walked anyway.

`--depth-raw` writes a per-pixel distance buffer, log-encoded to 16 bits,
quantised, row delta-coded and gzipped. At matched precision that is about half
the size of LERC, which is why LERC is not used despite the client already
decoding it.

### Serving

Runs as the `terrain` unit, installed from `deploy/terrain.service` by the
deploy. By hand:

```sh
./target/release/dem-tool serve --listen 127.0.0.1:3100 \
    --peaks /fm/storage2/dem/peaks.gpkg
```

One `POST /panorama` returning image, depth and peaks in a single
multipart/form-data body. See [docs/API.md](docs/API.md).

It reads `state/sources.json` and the source list at startup and refuses to
start if a source it would serve has no credit, so a render can always say
whose data answered it.

Not yet done: sun path, caching/precomputation.
