#!/usr/bin/env bash
# Run one build step, deprioritised and logged.
#
# fm6 is a production box: nginx, the Freemap API, three tile servers, mariadb
# and dem-server share those 12 cores, and 45 of the 62 GB of RAM is page cache
# doing useful work. A multi-day GDAL job at ALL_CPUS would be felt as tile
# latency for days, so every step runs at nice 19 / ionice idle and leaves
# headroom. Slower, but the site stays responsive.
#
# Usage:  bin/run.sh <step-name> <command> [args...]
# Log:    $DEM_ROOT/logs/<step-name>.log     (appended, timestamped)
# Status: $DEM_ROOT/state/<step-name>.status (exit code, written at the end)
#
# Detach with tmux so it survives ssh disconnect:
#   tmux new-session -d -s dem 'bin/run.sh footprints ./target/release/dem-tool footprints'
# Then just tail the log -- nothing depends on attaching to the session.

set -euo pipefail

DEM_ROOT="${DEM_ROOT:-/fm/storage2/dem}"

export GDAL_CACHEMAX="${GDAL_CACHEMAX:-4096}"
export GDAL_NUM_THREADS="${GDAL_NUM_THREADS:-4}"
export GDAL_DISABLE_READDIR_ON_OPEN=EMPTY_DIR
export CPL_VSIL_CURL_NON_CACHED=

if [ $# -lt 2 ]; then
  echo "usage: $0 <step-name> <command> [args...]" >&2
  exit 2
fi

step="$1"
shift

mkdir -p "$DEM_ROOT/logs" "$DEM_ROOT/state"
log="$DEM_ROOT/logs/$step.log"
status="$DEM_ROOT/state/$step.status"

rm -f "$status"

{
  echo "=== $step start $(date -Is) on $(hostname)"
  echo "=== cmd: $*"
} >>"$log"

set +e
nice -n 19 ionice -c 3 "$@" >>"$log" 2>&1
rc=$?
set -e

{
  echo "=== $step end $(date -Is) rc=$rc"
  echo
} >>"$log"

echo "$rc" >"$status"
exit "$rc"
