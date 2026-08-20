#!/usr/bin/env bash
# Pilot: one tile, three sources, all three priority cases.
#
# Tile 70_44 (lon 16.875-19.6875, lat 47.04-48.92) covers southwestern
# Slovakia, northeastern Austria, and northern Hungary. So it exercises
# national-over-national priority (sk outranks at), national-over-fallback
# (both outrank gedtm30), and pure fallback where no national source exists --
# which is exactly what the composite index has to get right.
#
# Validating the mechanism on one tile costs hours; getting it wrong on the
# continental build costs days.
#
# Usage: bin/pilot.sh   (resumable -- finished tiles are skipped)

set -euo pipefail

DEM_ROOT="${DEM_ROOT:-/fm/storage2/dem}"
cd "$DEM_ROOT/build"

TX=70
TY=44

# Cheapest first: gedtm30 is 15 s and proves the chain before the 1 m sources
# commit hours to it.
for id in gedtm30 sk at; do
  step="layer-a-$id-$TX-$TY"
  echo "=== $step"
  bash bin/run.sh "$step" bash bin/layer-a.sh "$id" "$TX" "$TY" || {
    echo "FAILED: $step -- see $DEM_ROOT/logs/$step.log" >&2
    exit 1
  }
  tail -2 "$DEM_ROOT/logs/$step.log"
done

echo
echo "=== pilot Layer A complete"
find "$DEM_ROOT/norm" -name "${TX}_${TY}.tif" -printf "%10s  %p\n" | sort -k2
