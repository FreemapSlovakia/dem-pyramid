#!/usr/bin/env bash
# Build Layer A for every source, in priority order, skipping tiles the source
# does not actually touch.
#
# Ordered by usefulness rather than alphabetically: central Europe first, so
# the pyramid is usable within a day, and the Nordics last -- se+fi+no are 59%
# of the tiles and roughly 3 TB of the 6.7 TB to read.
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
TOOL="$DEM_ROOT/build/target/release/dem-tool"
cd "$DEM_ROOT/build"

# Central Europe, then the Alps and the west, then Iberia and Britain, then the
# Nordic bulk, then the overseas departments.
DEFAULT_ORDER=(
  sk cz at hr si pl
  ch it fr
  es_29 es_30 es_31 en
  se fi no
  fr_guyane fr_reunion fr_martinique fr_spm fr_mayotte fr_guadeloupe
)

sources=("$@")
if [ ${#sources[@]} -eq 0 ]; then
  sources=("${DEFAULT_ORDER[@]}")
fi

started=$(date +%s)
done_tiles=0
skipped=0

for id in "${sources[@]}"; do
  fp="$DEM_ROOT/footprints/$id.gpkg"
  mapfile -t tiles < <("$TOOL" cover "$id" | tail -n +2)
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
