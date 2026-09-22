#!/usr/bin/env bash
# Build Layer A for every source, in priority order, skipping tiles the source
# does not actually touch.
#
# Ordered by usefulness rather than alphabetically: central Europe first, so
# the pyramid is usable within a day, then the Nordics -- se+fi+no are 59% of
# the tiles and roughly 3 TB of the 6.7 TB to read -- and last whatever the
# sources have gained since, which nobody has put in an order yet.
#
# Tile lists come from each source's bbox, then get filtered against its
# footprint. That matters: Norway's bbox spans 169 tiles while the country is a
# thin diagonal through it, and every skipped tile saves opening a VRT that can
# be 44 MB of XML.
#
# Resumable -- layer-a.sh skips finished tiles -- so it is safe to kill and
# restart at any point.
#
# Usage: bin/build-all.sh [source-id ...]     default: all, in priority order

set -euo pipefail

DEM_ROOT="${DEM_ROOT:-/fm/storage2/dem}"
BUILD_DIR="${BUILD_DIR:-$DEM_ROOT/build}"
TOOL="${TOOL:-$BUILD_DIR/target/release/dem-tool}"
# Overridable so a build can be driven from a checkout other than the deployed
# one -- bringing a new source in without waiting for a deploy, say. The
# scripts below are taken from here too, so the binary and the scripts that
# call it stay from the same tree.
cd "$BUILD_DIR"
export TOOL

# Preferred order, not the list: central Europe first so the pyramid is usable
# within a day, then the Alps and the west, Iberia and Britain, the Nordic bulk
# -- se+fi+no are 59% of the tiles -- and the overseas departments. Anything the
# sources hold that is not named here follows, in priority order, so a dataset
# added upstream is built rather than quietly skipped.
PREFERRED=(
  sk cz at hr si pl
  ch it fr
  es_29 es_30 es_31 en
  se fi no
  fr_guyane fr_reunion fr_martinique fr_spm fr_mayotte fr_guadeloupe
)

sources=("$@")
if [ ${#sources[@]} -eq 0 ]; then
  # Captured rather than piped from a process substitution, whose failure
  # `set -e` does not see: an empty list would otherwise read as "no sources"
  # when what happened is that the cache could not be loaded.
  listing=$("$TOOL" list) || {
    echo "build-all: could not read the sources -- run dem-tool refresh" >&2
    exit 1
  }
  mapfile -t all < <(printf '%s\n' "$listing" | tail -n +3 | awk '{print $1}')
  [ ${#all[@]} -gt 0 ] || { echo "build-all: the cache holds no sources" >&2; exit 1; }

  sources=()
  for id in "${PREFERRED[@]}"; do
    for have in "${all[@]}"; do
      [ "$id" = "$have" ] && sources+=("$id") && break
    done
  done
  for have in "${all[@]}"; do
    case " ${sources[*]} " in *" $have "*) ;; *) sources+=("$have") ;; esac
  done
fi

started=$(date +%s)
done_tiles=0
skipped=0

for id in "${sources[@]}"; do
  fp="$DEM_ROOT/footprints/$id.gpkg"
  # Not in a process substitution: `set -e` does not see a failure there, and
  # an unknown id would read as a source with nothing to build rather than as
  # the mistake it is.
  set +e
  cover=$("$TOOL" cover "$id")
  rc=$?
  set -e

  # 3 means the source spans the globe and wants a region named; bin/fallback.sh
  # owns those. Anything else is a mistake worth stopping for.
  if [ $rc -eq 3 ]; then
    echo "=== $id: skipped -- built over a region by bin/fallback.sh"
    continue
  fi
  [ $rc -eq 0 ] || {
    echo "build-all: $id: no such source" >&2
    exit 1
  }
  mapfile -t tiles < <(printf '%s\n' "$cover" | tail -n +2)
  echo "=== $id: ${#tiles[@]} candidate tiles  ($(date -Is))"

  for t in "${tiles[@]}"; do
    tx="${t% *}"
    ty="${t#* }"

    # Does the source's real coverage reach this tile? The footprint is in
    # lon/lat, so use the tile's geographic bounds.
    if [ -f "$fp" ]; then
      bounds=$("$TOOL" tile "$tx" "$ty" | awk '/^lonlat/ {print $2, $3, $4, $5}')
      # dem-tool prints lon0 lat0 lon1 lat1 with lat0 < lat1 already.
      n=$(ogrinfo -q -spat $bounds "$fp" footprint 2>/dev/null | grep -c "^OGRFeature" || true)
      if [ "${n:-0}" -eq 0 ]; then
        skipped=$((skipped + 1))
        continue
      fi
    fi

    if [ -f "$DEM_ROOT/norm/$id/14/${tx}_${ty}.tif" ] ||
       [ -f "$DEM_ROOT/norm/$id/12/${tx}_${ty}.tif" ]; then
      continue
    fi

    t0=$(date +%s)
    if bash bin/layer-a.sh "$id" "$tx" "$ty" >>"$DEM_ROOT/logs/build-all.log" 2>&1; then
      done_tiles=$((done_tiles + 1))
      mins=$(( ($(date +%s) - t0) / 60 ))
      elapsed=$(( ($(date +%s) - started) / 3600 ))
      echo "[$done_tiles] $id ${tx}_${ty} done in ${mins}m  (${elapsed}h elapsed, $skipped empty tiles skipped)"
    else
      echo "FAILED $id ${tx}_${ty} -- see $DEM_ROOT/logs/build-all.log" >&2
    fi
  done
done

echo "=== all sources complete: $done_tiles tiles built, $skipped skipped, $(( ($(date +%s) - started) / 3600 ))h"
