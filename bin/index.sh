#!/usr/bin/env bash
# The pyramid: one GTI per level over the rasters terrain-tiles serves.
#
# Each row is a `vrt://` connection string `dem-tool index-plan` writes: the
# raster's overview for that level, converted on read to the uint16 encoding
# the marcher reads. No pixels are copied -- a level's index is a few rows.
#
# Overlap is resolved at read time by the SORT_FIELD: higher priority is
# rendered last, so it wins, and nodata falls through to whatever is beneath.
# Each row also names its dataset, which is how `serve` knows what it serves
# and whom to credit.
#
# Usage: bin/index.sh [level ...]        default: every level, finest first

set -euo pipefail

DEM_ROOT="${DEM_ROOT:-/fm/storage2/dem}"
TOOL="${TOOL:-$DEM_ROOT/build/target/release/dem-tool}"

# An assignment, not a process substitution, so `set -e` sees the plan fail
# rather than reading it as a pyramid with no sources.
plan=$("$TOOL" index-plan)

levels=("$@")
if [ ${#levels[@]} -eq 0 ]; then
  mapfile -t levels < <(awk -F'\t' '$1 == "RES" { print $2 }' <<<"$plan")
fi

INDEX_DIR="$DEM_ROOT/index"
mkdir -p "$INDEX_DIR"

for z in "${levels[@]}"; do
  z=$((10#$z))
  index="$INDEX_DIR/z$z.gti.gpkg"
  tmp="$index.tmp.gti.gpkg"
  rm -f "$tmp"

  res=""
  ids=()
  priorities=()
  locs=()

  while IFS=$'\t' read -r kind level a b c; do
    [ "$level" = "$z" ] || continue
    case "$kind" in
      RES) res="$a" ;;
      SRC) ids+=("$a"); priorities+=("$b"); locs+=("$c") ;;
    esac
  done <<<"$plan"

  if [ -z "$res" ]; then
    echo "z$z: outside the pyramid" >&2
    exit 1
  fi

  # Removed rather than kept: the marcher would go on reading what it held.
  if [ ${#locs[@]} -eq 0 ]; then
    rm -f "$index"
    echo "z$z: no source reaches this level, index removed"
    continue
  fi

  # gdaltindex exits 0 having skipped a raster it cannot open, or having read
  # overview 1 for an `ovr=` the raster lacks; both only show on stderr. The
  # SORT_FIELD column is added below, before anything opens the index.
  if ! warnings=$(gdaltindex -q -f GPKG -lyr_name tiles \
      -tr "$res" "$res" -ot UInt16 -bandcount 1 -nodata 0 \
      -mo SORT_FIELD=priority -mo SORT_FIELD_ASC=YES \
      "$tmp" "${locs[@]}" 2>&1 >/dev/null) || [ -n "$warnings" ]; then
    echo "z$z: gdaltindex: ${warnings:-failed}" >&2
    echo "z$z: index not replaced" >&2
    rm -f "$tmp"
    exit 1
  fi

  ogrinfo "$tmp" -sql "ALTER TABLE tiles ADD COLUMN priority INTEGER" >/dev/null
  ogrinfo "$tmp" -sql "ALTER TABLE tiles ADD COLUMN dataset TEXT" >/dev/null
  prio_cases=""
  id_cases=""
  for i in "${!locs[@]}"; do
    loc="'${locs[$i]//\'/\'\'}'"
    prio_cases+=" WHEN $loc THEN ${priorities[$i]}"
    id_cases+=" WHEN $loc THEN '${ids[$i]//\'/\'\'}'"
  done
  ogrinfo "$tmp" -sql \
    "UPDATE tiles SET priority = CASE location$prio_cases END, dataset = CASE location$id_cases END" \
    >/dev/null

  # One labelled row per planned raster, or the level is not what was
  # planned: a missing row is a hole, and a row whose location gdaltindex
  # rewrote keeps a NULL priority and sorts unpredictably.
  count() {
    ogrinfo -q "$tmp" -sql "SELECT COUNT(*) AS n FROM tiles $1" |
      sed -n 's/.*n (Integer) = //p'
  }
  rows=$(count "")
  unset_rows=$(count "WHERE priority IS NULL OR dataset IS NULL")
  if [ "${rows:-}" != ${#locs[@]} ] || [ "${unset_rows:-1}" != 0 ]; then
    echo "z$z: ${rows:-?} row(s) for ${#locs[@]} planned, ${unset_rows:-?} without" \
      "a priority or dataset; index not replaced" >&2
    rm -f "$tmp"
    exit 1
  fi

  mv "$tmp" "$index"
  echo "z$z: $index"
  for i in "${!ids[@]}"; do
    printf "   %-14s priority=%s\n" "${ids[$i]}" "${priorities[$i]}"
  done
done
