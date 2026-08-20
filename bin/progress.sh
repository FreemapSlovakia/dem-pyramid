#!/usr/bin/env bash
# Where the build has got to. Safe to run at any time; reads only.

set -euo pipefail

DEM_ROOT="${DEM_ROOT:-/fm/storage2/dem}"
TOOL="$DEM_ROOT/build/target/release/dem-tool"

running=$(tmux has-session -t build 2>/dev/null && echo yes || echo no)
echo "build session running: $running"
echo

printf "%-14s %7s %10s\n" source tiles size
printf -- "----------------------------------\n"
total=0
for d in "$DEM_ROOT"/norm/*/; do
  id=$(basename "$d")
  n=$(find "$d" -name '*.tif' | wc -l)
  sz=$(du -sh "$d" 2>/dev/null | cut -f1)
  total=$((total + n))
  printf "%-14s %7s %10s\n" "$id" "$n" "$sz"
done
printf -- "----------------------------------\n"
printf "%-14s %7s %10s\n" TOTAL "$total" "$(du -sh "$DEM_ROOT/norm" 2>/dev/null | cut -f1)"

echo
echo "disk free on storage2: $(df -h /fm/storage2 | awk 'NR==2 {print $4}')"
echo
echo "last 5 completed:"
tmux capture-pane -p -t build 2>/dev/null | grep -E "^\[[0-9]+\]" | tail -5 ||
  echo "  (no session; see $DEM_ROOT/logs/build-all.log)"
