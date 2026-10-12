#!/usr/bin/env bash
# Fetch the OSM peaks from fm5's database into $DEM_ROOT/peaks-osm.gpkg.
#
# The key can run only freemap-osm-api's scripts/peaks-dump.sh as `osm` on
# fm5, which writes the GeoPackage to stdout; see that repo's README, "Peaks
# dump for dem-pyramid". A dump that is not shaped as `serve` reads peaks, or
# is far smaller than the last, is refused, so a broken import upstream never
# replaces a good file. FORCE=1 accepts a smaller one.
#
# Usage: bin/peaks.sh

set -euo pipefail

DEM_ROOT="${DEM_ROOT:-/fm/storage2/dem}"
KEY="${KEY:-$HOME/.ssh/id_peaks_fm5}"
OUT="$DEM_ROOT/peaks-osm.gpkg"
# The extension too, so GDAL takes it for the GeoPackage it is.
tmp="$DEM_ROOT/peaks-osm.tmp.gpkg"

mkdir -p "$DEM_ROOT"

# One run at a time: a second would write the same temporary file.
exec 9>"$DEM_ROOT/peaks-osm.lock"
flock -n 9 || {
  echo "peaks: another run holds $DEM_ROOT/peaks-osm.lock" >&2
  exit 1
}

# Empty where the file has no readable `peaks` layer.
count() {
  ogrinfo -q "$1" -sql "SELECT COUNT(*) AS n FROM peaks" |
    sed -n 's/.*n (Integer[0-9]*) = //p' || true
}

trap 'rm -f "$tmp"' EXIT

ssh -o IdentitiesOnly=yes -o BatchMode=yes -i "$KEY" -p 21122 \
  peaks-dump@fm5.freemap.sk >"$tmp"

n=$(count "$tmp")
if [ -z "$n" ] || [ "$n" -eq 0 ]; then
  echo "peaks: the dump is not a peaks GeoPackage, $OUT not replaced" >&2
  exit 1
fi

# `serve` reads the columns by position.
fields=$(ogrinfo -so "$tmp" peaks | sed -n 's/^\([a-z_]*\): .*/\1/p' | paste -sd,)
if [ "$fields" != "osm_id,name,type,ele_osm" ]; then
  echo "peaks: the dump's columns are $fields, $OUT not replaced" >&2
  exit 1
fi

if [ -e "$OUT" ]; then
  was=$(count "$OUT")
  if [ -z "$was" ]; then
    echo "peaks: $OUT is unreadable, replacing it" >&2
  elif [ "$n" -lt $((was * 9 / 10)) ] && [ "${FORCE:-}" != 1 ]; then
    echo "peaks: $n peaks against $was before, $OUT not replaced (FORCE=1 to accept)" >&2
    exit 1
  fi
fi

mv "$tmp" "$OUT"
echo "peaks: $n peaks -> $OUT"
