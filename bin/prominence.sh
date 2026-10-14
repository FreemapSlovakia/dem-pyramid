#!/usr/bin/env bash
# Measure the OSM peaks and saddles in $DEM_ROOT/peaks-osm.gpkg into
# $DEM_ROOT/peaks.gpkg, which `serve` reads per request.
#
# Then fills, from data/peak-overrides.csv, what the run could not measure:
# a value is set only where the run left it null, so a measured one wins.
# A run with far fewer prominences than the file it would replace is
# refused; FORCE=1 accepts it.
#
# Usage: bin/prominence.sh     after bin/peaks.sh

set -euo pipefail

DEM_ROOT="${DEM_ROOT:-/fm/storage2/dem}"
TOOL="${TOOL:-$DEM_ROOT/build/target/release/dem-tool}"
OVERRIDES="${OVERRIDES:-$(dirname "$0")/../data/peak-overrides.csv}"
IN="$DEM_ROOT/peaks-osm.gpkg"
OUT="$DEM_ROOT/peaks.gpkg"
# The extension too, so GDAL takes it for the GeoPackage it is.
tmp="$DEM_ROOT/peaks.new.gpkg"

exec 9>"$DEM_ROOT/prominence.lock"
flock -n 9 || {
  echo "prominence: another run holds $DEM_ROOT/prominence.lock" >&2
  exit 1
}

trap 'rm -f "$tmp"' EXIT
rm -f "$tmp"

"$TOOL" prominence --input "$IN" --output "$tmp"

# Through GDAL, whose SQL functions the GeoPackage's triggers need.
# The name is last, where `read` leaves whatever commas it holds.
tail -n +2 "$OVERRIDES" | while IFS=, read -r osm_id field value _source _name; do
  case "$field" in
  prominence | isolation_m) ;;
  *)
    echo "prominence: unknown override field $field" >&2
    exit 1
    ;;
  esac
  ogrinfo -q "$tmp" -sql \
    "UPDATE peaks SET $field = $value WHERE osm_id = $osm_id AND $field IS NULL"
done

count() {
  ogrinfo -q "$1" -sql "SELECT COUNT(*) AS n FROM peaks WHERE prominence IS NOT NULL" |
    sed -n 's/.*n (Integer[0-9]*) = //p' || true
}

n=$(count "$tmp")
if [ -e "$OUT" ]; then
  was=$(count "$OUT")
  if [ -n "$was" ] && [ "${n:-0}" -lt $((was * 9 / 10)) ] && [ "${FORCE:-}" != 1 ]; then
    echo "prominence: $n prominences against $was before, $OUT not replaced (FORCE=1 to accept)" >&2
    exit 1
  fi
  cp -p "$OUT" "$DEM_ROOT/peaks.old.gpkg"
fi

mv "$tmp" "$OUT"
echo "prominence: $n prominences -> $OUT"
