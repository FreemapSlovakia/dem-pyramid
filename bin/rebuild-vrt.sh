#!/usr/bin/env bash
# Regenerate a source's VRT with a real nodata sentinel.
#
# For sources flagged `rebuild_vrt` in sources.yaml. The upstream VRT declares
# no nodata, so every part of its canvas not covered by a tile reads as 0 --
# real terrain, at sea level, outranking lower-priority sources. Rebuilding
# over the same leaf tiles with -vrtnodata gives uncovered canvas a sentinel
# that falls through properly, while genuine 0 m coastal pixels stay valid.
#
# Usage: bin/rebuild-vrt.sh <source-id>

set -euo pipefail

DEM_ROOT="${DEM_ROOT:-/fm/storage2/dem}"
TOOL="$DEM_ROOT/build/target/release/dem-tool"
NODATA=-9999

id="${1:?usage: rebuild-vrt.sh <source-id>}"

mkdir -p "$DEM_ROOT/vrt" "$DEM_ROOT/tmp"

list="$DEM_ROOT/tmp/$id.files"
out="$DEM_ROOT/vrt/$id.vrt"
tmp="$DEM_ROOT/tmp/$id.rebuild.vrt"

"$TOOL" vrt-sources "$id" >"$list"
echo "$id: $(wc -l <"$list") leaf files"

gdalbuildvrt -q -vrtnodata "$NODATA" -input_file_list "$list" "$tmp"
mv "$tmp" "$out"
rm -f "$list"

echo "$id: wrote $out"
