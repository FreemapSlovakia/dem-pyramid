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
