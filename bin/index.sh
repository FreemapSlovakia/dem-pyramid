#!/usr/bin/env bash
# Layer B: the composite, as an index only.
#
# One GTI per pyramid level over the Layer A tiles. Coarser levels reference
# each tile's overviews through `vrt://<file>?ovr=<n>` connection strings, so
# no pixels are copied and no wrapper files are written -- a level's index is
# a few kilobytes of rows.
#
# Overlap is resolved at read time by the SORT_FIELD: higher priority is
# rendered last, so it wins, and nodata falls through to whatever is beneath.
# That is why adding a source costs a warp plus row inserts, and why changing
# priorities costs nothing at all.
#
# Usage: bin/index.sh [level ...]        default: every level, finest first

set -euo pipefail

DEM_ROOT="${DEM_ROOT:-/fm/storage2/dem}"
TOOL="$DEM_ROOT/build/target/release/dem-tool"

levels=("$@")
if [ ${#levels[@]} -eq 0 ]; then
  levels=(14 13 12 11 10 9 8)
fi

mkdir -p "$DEM_ROOT/index"

for z in "${levels[@]}"; do
  index="$DEM_ROOT/index/z$z.gti.gpkg"
  tmp="$index.tmp.gti.gpkg"
  rm -f "$tmp"

  res=""
  first=1
  declare -a present=()

  while IFS=$'\t' read -r kind id priority ovr dir; do
    case "$kind" in
      RES) res="$id"; continue ;;
      SRC) ;;
      *) continue ;;
    esac

    [ -d "$dir" ] || continue

    # Collect this source's tiles, as base images or as overview references.
    locs=()
    for f in "$dir"/*.tif; do
      [ -e "$f" ] || continue
      if [ "$ovr" -lt 0 ]; then
        locs+=("$f")
      else
        locs+=("vrt://$f?ovr=$ovr")
      fi
    done
    [ ${#locs[@]} -gt 0 ] || continue

    mo=()
    if [ "$first" = 1 ]; then
      # SORT_FIELD names a column added below; nothing opens the index in
      # between, so referring to it before it exists is harmless.
      mo=(-mo SORT_FIELD=priority -mo SORT_FIELD_ASC=YES)
      first=0
    fi

    gdaltindex -q -f GPKG -lyr_name tiles \
      -tr "$res" "$res" -ot UInt16 -bandcount 1 -nodata 0 \
      -write_absolute_path "${mo[@]}" \
      "$tmp" "${locs[@]}"

    present+=("$id:$priority:${#locs[@]}")
  done < <("$TOOL" index-plan "$z")

  if [ "$first" = 1 ]; then
    echo "z$z: no tiles yet, skipped"
    rm -f "$tmp"
    continue
  fi

  # Priority per row, matched on the source directory in the location string.
  ogrinfo "$tmp" -sql "ALTER TABLE tiles ADD COLUMN priority INTEGER" >/dev/null
  for entry in "${present[@]}"; do
    id="${entry%%:*}"
    rest="${entry#*:}"
    priority="${rest%%:*}"
    ogrinfo "$tmp" -sql \
      "UPDATE tiles SET priority = $priority WHERE location LIKE '%/norm/$id/%'" \
      >/dev/null
  done

  mv "$tmp" "$index"
  echo "z$z: $index"
  for entry in "${present[@]}"; do
    IFS=: read -r id priority n <<<"$entry"
    printf "   %-12s priority=%-4s tiles=%s\n" "$id" "$priority" "$n"
  done
done
