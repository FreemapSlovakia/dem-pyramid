#!/usr/bin/env bash
# Materialise the global fallback once, over a region, rather than in buffers
# around each country.
#
# A panorama sees 300 km, so wherever national data ends the view carries on
# into whatever is beyond -- Hungary from the Tatras, Austria from Bratislava.
# Building GEDTM30 per-country with a one-tile margin works but has to be
# redone every time a country is added, and the margins overlap anyway. Doing
# the whole region once removes the bookkeeping for good.
#
# Cost is modest because the source is 30 m: 675 tiles for lon -25..45,
# lat 30..72, at roughly 15 s and 90 MB each, and ocean tiles are nodata so
# they cost almost nothing. Going fully global would be ~3500 land tiles,
# perhaps 15 hours and 320 GB -- worth it only if coverage ever leaves Europe.
#
# Usage: bin/fallback.sh [lon0,lat0,lon1,lat1]

set -euo pipefail

DEM_ROOT="${DEM_ROOT:-/fm/storage2/dem}"
TOOL="$DEM_ROOT/build/target/release/dem-tool"
cd "$DEM_ROOT/build"

BBOX="${1:--25,30,45,72}"

mapfile -t tiles < <("$TOOL" cover gedtm30 --bbox="$BBOX" | tail -n +2)
echo "=== gedtm30 over $BBOX: ${#tiles[@]} tiles  ($(date -Is))"

n=0
built=0
started=$(date +%s)
for t in "${tiles[@]}"; do
  tx="${t% *}"
  ty="${t#* }"
  n=$((n + 1))
  [ -f "$DEM_ROOT/norm/gedtm30/12/${tx}_${ty}.tif" ] && continue
  if bash bin/layer-a.sh gedtm30 "$tx" "$ty" >>"$DEM_ROOT/logs/fallback.log" 2>&1; then
    built=$((built + 1))
    if [ $((built % 25)) -eq 0 ]; then
      echo "[$n/${#tiles[@]}] $built built, $(( ($(date +%s) - started) / 60 ))m elapsed"
    fi
  else
    echo "FAILED gedtm30 ${tx}_${ty}" >&2
  fi
done

echo "=== fallback complete: $built tiles built of ${#tiles[@]}, $(( ($(date +%s) - started) / 60 ))m"
