#!/usr/bin/env bash
# Layer A: warp one source into one tile of the global grid, once and for ever.
#
# Each source is materialised into its own COG tiles at its finest level, with
# a full overview chain down to z8. Nothing here knows about other sources --
# priority and fall-through are resolved later, by the index, at read time.
# That is what keeps adding a source proportional to the new data.
#
# Usage: bin/layer-a.sh <source-id> <tx> <ty>
#
# Resumable: an existing output is left alone. Writes to a .tmp and renames, so
# an interrupted run never leaves a half-written tile that looks finished.

set -euo pipefail

DEM_ROOT="${DEM_ROOT:-/fm/storage2/dem}"
TOOL="$DEM_ROOT/build/target/release/dem-tool"
THREADS="${GDAL_NUM_THREADS:-4}"

id="${1:?usage: layer-a.sh <source-id> <tx> <ty>}"
tx="${2:?}"
ty="${3:?}"

# SRC_PATH LEVEL PX BLOCK TE RESAMPLING SRCNODATA WARP_NODATA SCALE
# FILL_NODATA_MD DST_DIR DST
eval "$("$TOOL" warp-env "$id" "$tx" "$ty")"

if [ -f "$DST" ]; then
  echo "skip   $DST (exists)"
  exit 0
fi

mkdir -p "$DST_DIR" "$DEM_ROOT/tmp"
vrt="$DEM_ROOT/tmp/${id}_${tx}_${ty}.vrt"
tmp="$DST.tmp.tif"
trap 'rm -f "$vrt" "$tmp" "$vrt.filled.vrt"' EXIT

echo "build  $DST  (z$LEVEL, ${PX}x${PX})"

# 1. Warp to the tile's exact extent and pixel count. -of VRT writes no pixels;
#    the real work happens when gdal_translate reads it, so the sources are
#    read exactly once.
warp=(-of VRT -t_srs EPSG:3857 -te $TE -ts "$PX" "$PX"
      -r "$RESAMPLING" -ot Float32 -dstnodata "$WARP_NODATA"
      -multi -wo NUM_THREADS="$THREADS")
if [ -n "$SRCNODATA" ]; then
  warp+=(-srcnodata "$SRCNODATA")
fi
gdalwarp -q "${warp[@]}" "$SRC_PATH" "$vrt"

src="$vrt"

# 2. Optional speckle healing, for sources whose nodata value collides with
#    real terrain (pl uses 0, which is also genuine Baltic coastal elevation).
#    Small holes get interpolated; large out-of-coverage regions stay nodata.
if [ -n "$FILL_NODATA_MD" ]; then
  echo "       fillnodata -md $FILL_NODATA_MD"
  gdal_fillnodata -q -md "$FILL_NODATA_MD" -of GTiff \
    -co COMPRESS=ZSTD -co PREDICTOR=3 -co TILED=YES -co BIGTIFF=YES \
    "$vrt" "$vrt.filled.tif"
  src="$vrt.filled.tif"
fi

# 3. Encode to uint16 decimetres and write the COG with its overview chain.
#    Source nodata (-9999) scales far below zero and is clamped to 0 by the
#    UInt16 conversion, which is exactly the reserved nodata value.
gdal_translate -q -of COG -ot UInt16 \
  -scale $SCALE -a_scale 0.1 -a_offset -500 -a_nodata 0 \
  -co COMPRESS=ZSTD -co PREDICTOR=2 -co BLOCKSIZE="$BLOCK" \
  -co RESAMPLING=AVERAGE -co OVERVIEW_RESAMPLING=AVERAGE \
  -co OVERVIEW_COUNT="$OVERVIEW_COUNT" \
  -co NUM_THREADS="$THREADS" -co BIGTIFF=YES -co SPARSE_OK=TRUE \
  "$src" "$tmp"

mv "$tmp" "$DST"
echo "done   $DST  $(du -h "$DST" | cut -f1)"
