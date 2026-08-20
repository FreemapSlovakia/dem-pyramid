#!/usr/bin/env bash
# Push the build scripts to fm6. Authored locally so every change shows up as
# a reviewable diff; fm6 only ever holds a copy.
#
#   scripts -> fm6:/fm/storage2/dem/build
#   data    -> fm6:/fm/storage2/dem/{footprints,norm,index,logs,state,tmp}
#
# storage1 is at 93% with 1.2 T free and holds the sources; nothing is written
# there. storage2 has 5.3 T.

set -euo pipefail

HOST="${HOST:-fm6}"
DEM_ROOT="${DEM_ROOT:-/fm/storage2/dem}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

ssh "$HOST" "mkdir -p $DEM_ROOT/build $DEM_ROOT/footprints $DEM_ROOT/logs $DEM_ROOT/state $DEM_ROOT/tmp"

rsync -a --delete \
  --exclude '.git' \
  --exclude 'target' \
  "$HERE/" "$HOST:$DEM_ROOT/build/"

ssh "$HOST" "chmod +x $DEM_ROOT/build/bin/*.sh"

# Built on fm6, never copied: its glibc (2.41) is older than the workstation's,
# so a locally built binary would not run there.
ssh "$HOST" "cd $DEM_ROOT/build && \$HOME/.cargo/bin/cargo build --release -q"

echo "synced + built -> $HOST:$DEM_ROOT/build"
