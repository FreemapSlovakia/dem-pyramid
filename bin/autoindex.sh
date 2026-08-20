#!/usr/bin/env bash
# Keep Layer B current while Layer A is being built, then index once more at
# the end and exit.
#
# Tiles are invisible to the marcher until they are in the GTI, so without this
# a multi-day build finishes with nothing queryable. Re-indexing is cheap --
# the index holds no pixels, only rows -- so doing it periodically also means
# the pyramid is progressively usable as countries land, rather than only at
# the end.
#
# Exits by itself once both build sessions are gone.

set -euo pipefail

DEM_ROOT="${DEM_ROOT:-/fm/storage2/dem}"
INTERVAL="${INTERVAL:-1800}"
cd "$DEM_ROOT/build"

reindex() {
  nice -n 19 ionice -c 3 bash bin/index.sh >>"$DEM_ROOT/logs/index.log" 2>&1 || true
  echo "reindexed $(date -Is): $(find "$DEM_ROOT/norm" -name '*.tif' | wc -l) tiles"
}

while tmux has-session -t build 2>/dev/null || tmux has-session -t fallback 2>/dev/null; do
  sleep "$INTERVAL"
  reindex
done

echo "=== builds finished, final index $(date -Is)"
reindex
echo "=== done"
