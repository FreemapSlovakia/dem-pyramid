# dem-pyramid

Build scripts for Freemap's elevation pyramid — the shared substrate for
panorama, viewshed and (later) cast shadows.

Scripts are authored here and rsynced to fm6; fm6 holds only a copy. All data
lives on fm6 under `/fm/storage2/dem`.

## Layout

```
sources.yaml            every DTM source: path, priority, nodata, footprint
src/                    dem-tool: config validation, drift check, footprints
bin/run.sh              deprioritised, logged step runner
bin/sync.sh             push to fm6 and build there
```

Orchestration is bash calling the GDAL command line tools; `dem-tool` holds the
parts that are actual logic. It shells out to `gdalinfo -json`, `gdalsrsinfo`
and `ogr2ogr` rather than linking libgdal, so it needs no C++ build and is not
tied to the host's GDAL version.

The binary is built **on fm6** — its glibc (2.41) is older than the
workstation's (2.42), so a locally built binary would not run there.

On fm6:

```
/fm/storage2/dem/
  build/                this repo
  footprints/           <id>.gpkg + summary.json
  norm/<id>/            Layer A: per-source zoom-aligned COGs + overviews
  index/z{14..8}.gti.gpkg   Layer B: composite, index only
  logs/ state/ tmp/
```

## Design in one paragraph

Two layers. **Layer A** warps each source exactly once, ever, into its own
zoom-aligned COG tiles on the global Web-Mercator grid. **Layer B** is a GTI
index per level with priority as a sort column — no pixels, so adding a source
means warping that source and inserting rows. Each source is materialised only
at levels at or coarser than its own resolution, so z14/z13 are sparse
(national data only) and the ray marcher falls back to the finest level that
has data. Nothing is ever rebuilt from scratch.

## Grid

EPSG:3857 at standard XYZ resolutions. Level `z` has a **projected** pixel size
of `156543.03392804097 / 2^z` — projected metres, not ground metres. Ground
resolution at 49N is `102700 / 2^z`, so z14 is 6.27 m there, 3.0 m in northern
Norway and 7.8 m in southern Spain. The marcher picks a level from the *local*
ground cell size, not from a fixed distance table.

Tiles are 32768 px at z14 = 313.086 km, origin at the Web-Mercator origin, with
512 px blocks — so the in-file overview chain reaches z8 and the tile grid never
shifts when a source is added. That fixed grid is what makes incremental
rebuilds possible.

## Usage

```sh
bin/sync.sh                                    # local -> fm6, then cargo build

# on fm6, in /fm/storage2/dem/build:
./target/release/dem-tool list
./target/release/dem-tool check                # re-measure, fail on drift
./target/release/dem-tool elevation-sources    # regenerate for freemap.conf

bin/run.sh footprints ./target/release/dem-tool footprints
tail -f /fm/storage2/dem/logs/footprints.log
```

Long steps go in tmux and log to `$DEM_ROOT/logs/<step>.log`; nothing depends on
staying attached.

## Pilot results (tile 70_44, 2026-08-20)

Tile 70_44 spans lon 16.875–19.6875, lat 47.04–48.92: southwestern Slovakia,
northeastern Austria, northern Hungary.

| source | level | build | size | valid % | ratio | B/px |
|---|---|---|---|---|---|---|
| sk | 14 | 33.5 min | 437.7 MB | 51.1 | 3.34× | 0.80 |
| at | 14 | 2.0 min | 13.9 MB | 2.3 | 4.66× | 0.57 |
| gedtm30 | 12 | 15 s | 83.1 MB | 100 | 2.15× | 1.24 |

Ratio is stored bytes against the raw bytes of the *valid* pixels plus their
overview chain; SPARSE_OK means nodata blocks are never written.

At 0.80 B/px the national coverage projects to **~90 GB** (49.6 GB for
se+fi+no at 63N, 40.8 GB for the rest at 46N — pixel count per km² of ground
rises with latitude). This tile is lowland plus low hills; Alpine 1 m LiDAR
will compress worse, so budget 90–150 GB.

Verified:

- **Priority** at a genuine sk/at overlap: sk 6388, at 6393, index returns
  6388. SORT_FIELD resolves correctly.
- **Fall-through** sk → at → gedtm30 at points where each is the first with
  data.
- **Sparse fine levels**: z14 returns nodata over Hungary while z12 returns
  215.1 m from gedtm30 — the marcher's fallback case.
- **Geometric alignment**: sk and at tiles have bit-identical geotransforms and
  corners. Because every source is warped to the same `-te`/`-ts`, a
  half-cell seam is impossible by construction.
- **Vertical agreement**: sk − at = −0.129 m mean, sd 0.158 m over the overlap
  (n=7 — the overlap strip is thin, so treat as an order of magnitude). That is
  the expected decimetre-scale vertical datum difference, and it is 1/70 of a
  pixel at 10 km. Not worth chasing.

The visual seam check was **inconclusive**: the sk/at boundary here follows the
Morava floodplain, whose paleochannel microrelief saturates a hillshade long
before a 13 cm step becomes visible. Natural exaggeration showed no gross
artefacts. A mountainous border (SK/PL Tatras) would be the better test.

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

Not yet done: SVG output, peak labels, sun path.

## Known source hazards

Re-measured 2026-08-20, all 23 sources.

- **si** declares no nodata on the VRT *or* the underlying tiles, so it returns
  0 over every uncovered part of a bbox that reaches into Italy, Austria and
  Croatia. `rebuild_vrt` regenerates it with `gdalbuildvrt -vrtnodata -9999`.
- **pl** uses 0 for out-of-coverage, colliding with genuine 0 m coastal terrain
  (Żuławy). Healed with `gdal_fillnodata -md 5`.
- **es_29** and **es_31** use `-32767`; **es_30** uses `-9999`. Same country.
- **sk** and **gedtm30** use float-max (`3.4e38`) sentinels.
- **no** uses `-32767`, **fr** and all DROM use `-99999`.
- Vertical datums differ across sources (RH2000, IGN69, LN02, EGM2008 …).
  Offsets are decimetre-scale — invisible to the ray marcher, visible in a
  hillshade seam check. Don't chase them.

Priority runs **higher wins**, but note the two opposite conventions
downstream: `freemap-v3-api` is first-wins, `gdalbuildvrt` is last-wins.
`dem-tool elevation-sources` handles the reversal.
