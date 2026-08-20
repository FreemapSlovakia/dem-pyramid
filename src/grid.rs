//! The global tile grid, and the GDAL arguments for building one tile.
//!
//! Tiles are 32768 px at z14 = 313086.0688 projected metres, anchored at the
//! Web Mercator origin. That divides the world into exactly 128x128 tiles, and
//! the span is the same at every level (a z13 tile is 16384 px of twice the
//! size), so a tile's geographic extent never depends on the level.
//!
//! The grid being fixed and global is what makes incremental rebuilds
//! possible: adding a source can never shift a tile boundary, so only the
//! tiles it actually touches are affected.

use anyhow::{Result, bail};

use crate::config::{EARTH_CIRCUMFERENCE, Grid, Nodata, Source, level_res};

/// Left/top edge of the Web Mercator extent.
pub const ORIGIN: f64 = EARTH_CIRCUMFERENCE / 2.0;

/// Elevation encoding: uint16, decimetres, offset -500 m.
///
/// `v = (h + 500) * 10`, so 65535/6553.5 is exactly 10 and the encoding is
/// exact decimetres. 0 is reserved for nodata, which costs the single value
/// h = -500.00 m -- no terrain in coverage sits there. Source nodata maps far
/// below 0 and is clamped to 0 by the UInt16 conversion, which is exactly the
/// wanted behaviour.
pub const SCALE_MIN_M: f64 = -500.0;
pub const SCALE_MAX_M: f64 = 6053.5;

/// Value gdalwarp writes for nodata in the Float32 intermediate.
pub const WARP_NODATA: f64 = -9999.0;

pub fn tile_span(g: &Grid) -> f64 {
    f64::from(g.tile_px) * level_res(g.finest_level)
}

pub fn tiles_across(g: &Grid) -> u32 {
    (EARTH_CIRCUMFERENCE / tile_span(g)).round() as u32
}

/// Pixels per tile edge at `level`.
pub fn tile_px_at(g: &Grid, level: u32) -> Result<u32> {
    if level > g.finest_level || level < g.coarsest_level {
        bail!(
            "level {level} outside the pyramid ({}..{})",
            g.coarsest_level,
            g.finest_level
        );
    }
    Ok(g.tile_px >> (g.finest_level - level))
}

/// `[x0, y0, x1, y1]` in EPSG:3857.
pub fn tile_extent(g: &Grid, tx: u32, ty: u32) -> [f64; 4] {
    let span = tile_span(g);
    let x0 = -ORIGIN + f64::from(tx) * span;
    let y1 = ORIGIN - f64::from(ty) * span;
    [x0, y1 - span, x0 + span, y1]
}

pub fn lonlat_to_merc(lon: f64, lat: f64) -> (f64, f64) {
    let x = lon / 180.0 * ORIGIN;
    let y = (std::f64::consts::FRAC_PI_4 + lat.to_radians() / 2.0)
        .tan()
        .ln()
        * 6378137.0;
    (x, y)
}

pub fn merc_to_lonlat(x: f64, y: f64) -> (f64, f64) {
    let lon = x / ORIGIN * 180.0;
    let lat = (2.0 * (y / 6378137.0).exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees();
    (lon, lat)
}

/// Every tile whose extent intersects a lon/lat bbox.
pub fn cover(g: &Grid, bbox: [f64; 4]) -> Vec<(u32, u32)> {
    let span = tile_span(g);
    let n = tiles_across(g);

    // Mercator blows up at the poles; clamp to the projection's usable range.
    let lat_lo = bbox[1].max(-85.05112878);
    let lat_hi = bbox[3].min(85.05112878);
    if lat_lo >= lat_hi {
        return Vec::new();
    }

    let (x0, y0) = lonlat_to_merc(bbox[0], lat_lo);
    let (x1, y1) = lonlat_to_merc(bbox[2], lat_hi);

    let tx0 = (((x0 + ORIGIN) / span).floor().max(0.0) as u32).min(n - 1);
    let tx1 = (((x1 + ORIGIN) / span).ceil().max(1.0) as u32).min(n);
    let ty0 = (((ORIGIN - y1) / span).floor().max(0.0) as u32).min(n - 1);
    let ty1 = (((ORIGIN - y0) / span).ceil().max(1.0) as u32).min(n);

    let mut out = Vec::new();
    for ty in ty0..ty1 {
        for tx in tx0..tx1 {
            out.push((tx, ty));
        }
    }
    out
}

/// Shell-sourceable variables for building one (source, tile) at one level.
///
/// The build script stays a dumb executor: all the arithmetic and all the
/// per-source policy lives here.
pub fn warp_env(g: &Grid, s: &Source, tx: u32, ty: u32, root: &str) -> Result<String> {
    let level = s.finest_level;
    let px = tile_px_at(g, level)?;
    let [x0, y0, x1, y1] = tile_extent(g, tx, ty);

    let srcnodata = match &s.nodata {
        Nodata::Value(v) => format!("{v}"),
        Nodata::Declared(_) => String::new(),
    };

    // Sources flagged rebuild_vrt are read through our own regenerated VRT,
    // never the original -- see si, whose upstream VRT declares no nodata and
    // therefore returns 0 over three neighbouring countries.
    let src_path = if s.rebuild_vrt {
        format!("{root}/vrt/{}.vrt", s.id)
    } else {
        s.path.clone()
    };

    let mut out = String::new();
    out.push_str(&format!("SRC_ID={}\n", s.id));
    out.push_str(&format!("SRC_PATH={}\n", shell_quote(&src_path)));
    out.push_str(&format!("LEVEL={level}\n"));
    out.push_str(&format!("TX={tx}\nTY={ty}\n"));
    out.push_str(&format!("PX={px}\n"));
    out.push_str(&format!("BLOCK={}\n", g.block_px));
    // Stop the overview chain exactly at the pyramid's coarsest level. Left to
    // itself the COG driver keeps halving past the block size, adding levels
    // below z8 that nothing reads.
    out.push_str(&format!(
        "OVERVIEW_COUNT={}\n",
        level - g.coarsest_level
    ));
    out.push_str(&format!("TE=\"{x0:.6} {y0:.6} {x1:.6} {y1:.6}\"\n"));
    out.push_str(&format!("RESAMPLING={}\n", s.resampling));
    out.push_str(&format!("SRCNODATA={srcnodata}\n"));
    out.push_str(&format!("WARP_NODATA={WARP_NODATA}\n"));
    out.push_str(&format!("SCALE=\"{SCALE_MIN_M} {SCALE_MAX_M} 0 65535\"\n"));
    out.push_str(&format!(
        "FILL_NODATA_MD={}\n",
        s.fill_nodata_md.map_or(String::new(), |v| v.to_string())
    ));
    out.push_str(&format!(
        "DST_DIR={}/norm/{}/{level}\n",
        shell_quote(root),
        s.id
    ));
    out.push_str(&format!("DST={}/norm/{}/{level}/{tx}_{ty}.tif\n", root, s.id));
    Ok(out)
}

fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}
