#!/usr/bin/env bash
# Compute topographic prominence and isolation for every summit in Europe.
#
# Prominence is what dominance cannot be: a property of the summit and the
# landmass, identical from every viewpoint. Dominance answers "does this stand
# out from where I am standing", which is viewpoint-dependent by design --
# measured across two Slovak viewpoints, 20% of shared summits flipped sign and
# 40% of those earning a label from one were dropped from the other. Prominence
# is the stable term that stops a real mountain vanishing because of where the
# viewer happens to be.
#
# Isolation -- distance to the nearest higher ground -- is the other half of the
# same question. A summit can have modest prominence and still command a
# horizon because nothing higher stands within fifty kilometres.
#
#   Usage:  bin/prominence.sh [tile|prominence|isolation|merge|all]
#   Output: $WORK/out/merged.txt      peak_lat,peak_lon,ele,col_lat,col_lon,prominence
#           $WORK/iso/*.txt           peak_lat,peak_lon,ele,higher_lat,higher_lon,isolation_km
#
# Run it under bin/run.sh so it is niced, logged and survives a disconnect:
#   tmux new-session -d -s prom 'bin/run.sh prominence bin/prominence.sh all'

set -euo pipefail

DEM_ROOT="${DEM_ROOT:-/fm/storage2/dem}"
WORK="${WORK:-$DEM_ROOT/scratch}"
SRC="${SRC:-/fm/storage1/dtm/gedtm_rf_m_30m_s_20060101_20151231_go_epsg.4326.3855_v1.2.tif}"
BIN="$WORK/mountains/code/release"

# GEDTM30 is the right input despite being our coarsest layer, and the national
# LiDAR is the wrong one. Prominence is a topological question about where the
# key col lies, and the col that settles a Slovak peak is often in Poland: mix
# 6 m inside one border with 30 m across it and the values stop being
# comparable exactly where the chain crosses. At 6 m bare earth every hummock
# also becomes a local maximum, which is noise to filter rather than signal.
# Elevations for the API keep coming from the pyramid, which reads summits
# 10-67 m higher than GEDTM30 does; this run contributes one number per peak.
#
# The window reaches well past the coastlines because a key col can lie a long
# way from its peak -- Gerlach's is out in the Moravian Gate. Peaks whose col
# lies outside still come back, with prominence equal to their elevation by the
# tool's convention for a window high point, so treat any peak whose prominence
# equals its elevation as a lower bound rather than a value.
W=${W:--25}   # west
E=${E:-45}    # east
S=${S:-34}    # south
N=${N:-72}    # north

MIN_PROM=${MIN_PROM:-30}     # metres, matching the API's min_dominance floor
MIN_ISO=${MIN_ISO:-1}        # km
THREADS=${THREADS:-8}        # of 12; the box also serves tiles

# 3601 x 3601 samples, two bytes each. A tile of any other size is a partial
# write, and the reader aborts the whole run on one rather than skipping it.
TILE_BYTES=$((3601 * 3601 * 2))

HGT="$WORK/hgt-eu"
OUT="$WORK/out-eu"
ISO="$WORK/iso-eu"

step_tile() {
	# Emptied, not merely created. A run killed between the gdalwarp and the
	# mv leaves a staged tile behind, and SRTMHGT has no update support -- so
	# the next run's gdalwarp fails trying to reopen it and the tile is
	# reported as "no data", which is a wrong diagnosis for a self-inflicted
	# wound. It self-heals a run later, silently, having wasted a pass.
	rm -rf "$HGT/.staging"
	mkdir -p "$HGT" "$HGT/.staging"
	local lat lon f n=0 skipped=0
	for ((lat = S; lat < N; lat++)); do
		for ((lon = W; lon < E; lon++)); do
			f=$(printf '%s/%s%02d%s%03d.hgt' "$HGT" \
				"$([ $lat -ge 0 ] && echo N || echo S)" "${lat#-}" \
				"$([ $lon -ge 0 ] && echo E || echo W)" "${lon#-}")
			# Size, not existence. A tile interrupted mid-write is still a
			# file, and the reader does not survive one: a short read gets
			# past its length check and corrupts the heap, so the whole run
			# aborts thousands of tiles later with "double free". Killing
			# the first attempt mid-tiling left exactly one such file and
			# cost two runs to find.
			if [ -f "$f" ] && [ "$(stat -c%s "$f")" = "$TILE_BYTES" ]; then
				skipped=$((skipped + 1))
				continue
			fi
			rm -f "$f"
			# -r near, not bilinear: resampling onto the 1-arcsecond grid
			# shifts samples by up to half a pixel either way, and a shifted
			# true elevation beats a smoothed summit when what matters is the
			# height of cols and tops.
			# gdalwarp rather than gdal_translate, and the -srcnodata pair is
			# the whole reason. GEDTM30 marks absent data with the float
			# maximum, 3.4e38; `gdal_translate -ot Int16` saturates that to
			# 32767, so every ocean pixel became a 32 km peak. It fails
			# quietly and completely: the fake plateau outranks every real
			# mountain, so it becomes the root of the divide tree and every
			# prominence in Europe is measured against it. The merge crashed
			# before it ever produced a number, which was luck.
			#
			# `-dstnodata 0` is asked for and not obeyed: the SRTMHGT driver
			# writes -32768, the SRTM void, whatever it is told. Left that way
			# after checking what reads it -- hgt_loader maps -32768 onto
			# Tile::NODATA_ELEVATION, so the sea is missing data rather than a
			# 32 km-deep hole, and an island becomes its own landmass whose
			# high point takes prominence equal to its elevation. That is the
			# right answer: Etna's prominence really is its full 3357 m. The
			# flag stays because it costs nothing and states the intent.
			#
			# Staged through a directory rather than a filename suffix:
			# SRTMHGT derives its geotransform from the name and warns, or
			# outright refuses, when the extension is not .hgt.
			gdalwarp -q -te "$lon" "$lat" $((lon + 1)) $((lat + 1)) \
				-ts 3601 3601 -r near -ot Int16 \
				-srcnodata 3.4028234663852886e+38 -dstnodata 0 \
				-of SRTMHGT "$SRC" "$HGT/.staging/$(basename "$f")" 2>/dev/null || {
				echo "warning: no data for $f, skipping" >&2
				rm -f "$HGT/.staging/$(basename "$f")"
				continue
			}
			if [ "$(stat -c%s "$HGT/.staging/$(basename "$f")")" != "$TILE_BYTES" ]; then
				echo "warning: $f came out the wrong size, discarding" >&2
				rm -f "$HGT/.staging/$(basename "$f")"
				continue
			fi
			mv "$HGT/.staging/$(basename "$f")" "$f"
			n=$((n + 1))
		done
	done
	echo "tiled $n new, $skipped already present, $(ls "$HGT" | grep -c '\.hgt$') total"
}

step_prominence() {
	mkdir -p "$OUT"
	# `--` is load-bearing: a western bound is negative, and getopt_long reads
	# a bare `-25` as the options -2 and -5 rather than as a coordinate. The
	# Slovakia pilot ran entirely east of Greenwich and never saw it.
	"$BIN/prominence" -i "$HGT" -o "$OUT" -f SRTM30 -t "$THREADS" \
		-m "$MIN_PROM" -- "$S" "$N" "$W" "$E"
}

step_merge() {
	# One tree per tile becomes one tree for the continent. -f finalises:
	# runoffs are deleted and the tree pruned, which is only correct once no
	# further merge is coming.
	"$BIN/merge_divide_trees" -f -m "$MIN_PROM" -t "$THREADS" \
		"$OUT/merged" "$OUT"/*.dvt
	echo "peaks with prominence >= ${MIN_PROM}m: $(wc -l < "$OUT/merged.txt")"
}

step_isolation() {
	mkdir -p "$ISO"
	# -f needs deploy/mountains-isolation-format.patch; upstream hardcodes
	# 3-arcsecond SRTM and would read these tiles with the wrong stride.
	"$BIN/isolation" -i "$HGT" -o "$ISO" -f SRTM30 -t "$THREADS" \
		-m "$MIN_ISO" -- "$S" "$N" "$W" "$E"
	echo "isolation files: $(ls "$ISO" | wc -l)"
}

case "${1:-all}" in
tile) step_tile ;;
prominence) step_prominence ;;
merge) step_merge ;;
isolation) step_isolation ;;
all)
	# Prominence only. Isolation is a separate pass over the same tiles and
	# stays available as `bin/prominence.sh isolation`, but the map renderer
	# gets its isolation from OpenTopoMap's tool instead: that one is driven
	# by the OSM peak list, so its output is keyed by `osm_id` and joins
	# straight into the rendering database. Kirmse's is DEM-driven and would
	# have to be matched back across a ~100 m radius, which is guesswork
	# wherever two real summits sit inside it -- the three Rysy tops are
	# 130 m apart and all called "Rysy". A misranked label in a panorama is a
	# nuisance; on a map it is an error someone reports.
	#
	# Prominence has no such alternative, and its matching cost is acceptable
	# because the value is an internal ranking prior rather than a number
	# printed on a label.
	echo "== tiling $((E - W))x$((N - S)) degrees =="
	time step_tile
	echo "== prominence =="
	time step_prominence
	echo "== merge =="
	time step_merge
	;;
*)
	echo "usage: $0 [tile|prominence|merge|isolation|all]" >&2
	exit 1
	;;
esac
