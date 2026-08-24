#!/usr/bin/env bash
# Attach the computed prominences to the OSM peaks they belong to.
#
# The prominence run knows where summits are but not what they are called; OSM
# knows the opposite. Joining them is a spatial match, and the radius it needs
# is uncomfortably wide: GEDTM30 is 30 m data, so it places a summit up to
# ~75 m from where the LiDAR and OSM agree it is -- measured at Kralova hola
# (74 m), Rysy (64 m), Sitno (21 m). A tight radius would miss precisely the
# sharp summits that matter most.
#
# So the distance is stored beside the value. Every match this makes is a
# guess of some size, and a guess whose size is recorded can be audited, sorted
# on, or thrown away later; one that is silently folded into a number cannot.
#
#   Usage:  bin/prominence-join.sh [--radius M] [--dry-run]
#
# Adds two columns to peaks.gpkg:
#   prominence   metres, NULL where nothing matched within the radius
#   prom_dist_m  how far the matched summit was from the OSM node

set -euo pipefail

DEM_ROOT="${DEM_ROOT:-/fm/storage2/dem}"
PEAKS="${PEAKS:-$DEM_ROOT/peaks.gpkg}"
MERGED="${MERGED:-$DEM_ROOT/scratch/out-eu/merged.txt}"
WORK="${WORK:-$DEM_ROOT/scratch/join.db}"
RADIUS=150
DRY=

while [ $# -gt 0 ]; do
	case "$1" in
	--radius) RADIUS="$2"; shift 2 ;;
	--dry-run) DRY=1; shift ;;
	*) echo "usage: $0 [--radius M] [--dry-run]" >&2; exit 1 ;;
	esac
done

[ -f "$PEAKS" ] || { echo "no peaks file at $PEAKS" >&2; exit 1; }
[ -f "$MERGED" ] || { echo "no prominence output at $MERGED" >&2; exit 1; }

# Before anything else. This file is what the running service reads.
BACKUP="$PEAKS.before-prominence.$(date +%Y%m%d-%H%M%S)"
if [ -z "$DRY" ]; then
	cp -a "$PEAKS" "$BACKUP"
	echo "backed up to $BACKUP"
fi

rm -f "$WORK"

# peaks.gpkg is only ever written through OGR, never through plain sqlite3.
# Its r-tree triggers call ST_IsEmpty and fire on any UPDATE of the table --
# the WHEN clause is evaluated before it is found to be false -- and bare
# sqlite3 has no such function, so the whole statement fails to parse. Reads
# and the work database are fine; the write is not.
# Keyed on `osm_id`, not on `fid`. OGR treats a selected `fid` as the feature
# id and consumes it rather than writing it out, whatever it is aliased to, so
# the export came back two columns wide and every match silently failed.
# `osm_id` is the better key regardless -- it is unique across all 488 232 rows,
# it is what identifies a peak to everyone else, and it survives the file being
# rebuilt.
ogr2ogr -f CSV /vsistdout/ "$PEAKS" -dialect SQLITE \
	-sql "SELECT osm_id, ST_X(geom) AS lon, ST_Y(geom) AS lat FROM peaks" \
	> "$WORK.peaks.csv"

# One degree of latitude is 111 320 m everywhere; longitude shrinks by cos(lat).
# Good to a fraction of a percent over 150 m, which is far inside the error the
# match radius exists to absorb.
#
# Nearest wins, not highest. Two real summits can sit inside the radius -- Rysy
# has three within 130 m, all called "Rysy", one of them the highest point of
# Poland -- and taking the highest would collapse them onto one answer, which
# is exactly the failure a radius this wide makes possible.
sqlite3 "$WORK" <<SQL
CREATE TABLE prom(lat REAL, lon REAL, ele REAL, col_lat REAL, col_lon REAL, prominence REAL);
CREATE TABLE pk(osm_id INTEGER, lon REAL, lat REAL);
.mode csv
.import '$MERGED' prom
.import --skip 1 '$WORK.peaks.csv' pk
-- A grid bucket rather than an r-tree. The r-tree wants per-row query bounds
-- here, because the longitude radius depends on latitude, and the planner
-- would not use the index for that: the same join ran for ten minutes at full
-- CPU without finishing. Bucketing is a plain B-tree lookup and the cell size
-- is chosen so a 3x3 neighbourhood always covers the search radius --
-- 0.002 deg of latitude is 222 m everywhere, and 0.005 deg of longitude is
-- 172 m at 72 N, the worst case in the window. Offsets keep the cell index
-- positive so integer truncation cannot fold two cells together at 0.
CREATE TABLE prom_cell AS
SELECT rowid AS id, lat, lon, prominence,
       CAST((lat + 90.0) / 0.002 AS INTEGER) AS ci,
       CAST((lon + 180.0) / 0.005 AS INTEGER) AS cj
FROM prom;
CREATE INDEX prom_cell_ij ON prom_cell(ci, cj);

-- Mutual nearest, not merely nearest. A DEM summit stands for one real
-- summit, so lending it to several OSM nodes manufactures agreement that is
-- not there: Rysy has three named tops inside 130 m and GEDTM30 at 30 m
-- resolves one of them, so a one-way match gave all three the same 269 m --
-- including the north-west top, the highest point of Poland, whose real
-- prominence is a few tens of metres. Requiring the summit to choose the peak
-- back leaves the other two NULL, which is what we actually know about them.
CREATE TABLE match(osm_id INTEGER PRIMARY KEY, prominence REAL, dist REAL);
INSERT INTO match
SELECT osm_id, prominence, d FROM (
  SELECT pk.osm_id AS osm_id, c.prominence AS prominence, c.id AS prom_id,
         round(sqrt(((c.lat - pk.lat) * 111320.0) * ((c.lat - pk.lat) * 111320.0)
             + ((c.lon - pk.lon) * 111320.0 * cos(pk.lat * 3.14159265358979 / 180.0))
             * ((c.lon - pk.lon) * 111320.0 * cos(pk.lat * 3.14159265358979 / 180.0))), 1) AS d,
         row_number() OVER (
           PARTITION BY pk.osm_id
           ORDER BY ((c.lat - pk.lat) * 111320.0) * ((c.lat - pk.lat) * 111320.0)
                  + ((c.lon - pk.lon) * 111320.0 * cos(pk.lat * 3.14159265358979 / 180.0))
                  * ((c.lon - pk.lon) * 111320.0 * cos(pk.lat * 3.14159265358979 / 180.0))
         ) AS rn,
         row_number() OVER (
           PARTITION BY c.id
           ORDER BY ((c.lat - pk.lat) * 111320.0) * ((c.lat - pk.lat) * 111320.0)
                  + ((c.lon - pk.lon) * 111320.0 * cos(pk.lat * 3.14159265358979 / 180.0))
                  * ((c.lon - pk.lon) * 111320.0 * cos(pk.lat * 3.14159265358979 / 180.0))
         ) AS rn_summit
  FROM pk
  JOIN prom_cell c
    ON c.ci BETWEEN CAST((pk.lat + 90.0) / 0.002 AS INTEGER) - 1
                AND CAST((pk.lat + 90.0) / 0.002 AS INTEGER) + 1
   AND c.cj BETWEEN CAST((pk.lon + 180.0) / 0.005 AS INTEGER) - 1
                AND CAST((pk.lon + 180.0) / 0.005 AS INTEGER) + 1
)
WHERE rn = 1 AND rn_summit = 1 AND d <= $RADIUS;
SQL

echo "loaded $(sqlite3 "$WORK" 'SELECT count(*) FROM prom') prominence points"
echo "matched $(sqlite3 "$WORK" 'SELECT count(*) FROM match') peaks"

if [ -n "$DRY" ]; then
	echo "dry run: peaks.gpkg untouched"
	exit 0
fi

# Idempotent: re-running replaces the values rather than failing on the column.
ogrinfo "$PEAKS" -sql "ALTER TABLE peaks ADD COLUMN prominence REAL" >/dev/null 2>&1 || true
ogrinfo "$PEAKS" -sql "ALTER TABLE peaks ADD COLUMN prom_dist_m REAL" >/dev/null 2>&1 || true

# The match table is carried into the GeoPackage as a plain table -- creating
# and filling one fires none of the peaks triggers -- and only the UPDATE
# itself goes through OGR.
sqlite3 "$WORK" ".mode csv" ".headers off" "SELECT osm_id, prominence, dist FROM match" > "$WORK.match.csv"
sqlite3 "$PEAKS" "DROP TABLE IF EXISTS prom_match; CREATE TABLE prom_match(osm_id INTEGER PRIMARY KEY, prominence REAL, dist REAL);"
sqlite3 "$PEAKS" ".mode csv" ".import '$WORK.match.csv' prom_match"

ogrinfo "$PEAKS" -sql "UPDATE peaks SET prominence = NULL, prom_dist_m = NULL" >/dev/null
ogrinfo "$PEAKS" -sql "UPDATE peaks SET prominence = (SELECT m.prominence FROM prom_match m WHERE m.osm_id = peaks.osm_id), prom_dist_m = (SELECT m.dist FROM prom_match m WHERE m.osm_id = peaks.osm_id) WHERE osm_id IN (SELECT osm_id FROM prom_match)" >/dev/null
sqlite3 "$PEAKS" "DROP TABLE prom_match; PRAGMA journal_mode = DELETE;" >/dev/null

total=$(sqlite3 "$PEAKS" 'SELECT count(*) FROM peaks')
matched=$(sqlite3 "$PEAKS" 'SELECT count(*) FROM peaks WHERE prominence IS NOT NULL')
echo "matched $matched of $total peaks within ${RADIUS} m"
echo "match distance: median $(sqlite3 "$PEAKS" 'SELECT round(prom_dist_m,1) FROM peaks WHERE prominence IS NOT NULL ORDER BY prom_dist_m LIMIT 1 OFFSET (SELECT count(*)/2 FROM peaks WHERE prominence IS NOT NULL)') m"
echo "shared summits (one DEM top claimed by several OSM nodes):"
sqlite3 "$PEAKS" "SELECT count(*) FROM (SELECT prominence, count(*) c FROM peaks WHERE prominence IS NOT NULL GROUP BY prominence HAVING c > 1)"
