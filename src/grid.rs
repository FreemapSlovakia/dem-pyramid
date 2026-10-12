//! Web Mercator, and how a terrain-tiles raster is read as a pyramid level.
//!
//! The pyramid holds no pixels. Each level is an index over the rasters
//! terrain-tiles serves, which are already on the XYZ grid with overviews down
//! to z8, so a level is a choice of overview and a conversion to the encoding
//! the marcher reads -- both done by GDAL at read time.

use crate::config::EARTH_CIRCUMFERENCE;

/// Left/top edge of the Web Mercator extent.
pub const ORIGIN: f64 = EARTH_CIRCUMFERENCE / 2.0;

/// Elevation encoding the marcher reads: uint16, decimetres, offset -500 m.
///
/// `v = (h + 500) * 10`, so 65535/6553.5 is exactly 10 and the encoding is
/// exact decimetres. 0 is reserved for nodata, which costs the single value
/// h = -500.00 m -- no terrain in coverage sits there.
pub const SCALE_MIN_M: f64 = -500.0;
pub const SCALE_MAX_M: f64 = 6053.5;

/// terrain-tiles stores Int32 heights in 2 mm steps.
pub const TERRAIN_STEPS_PER_M: f64 = 500.0;

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

/// What the index opens for a raster whose base is `base_level`, read at
/// `level`, or `None` where the level is finer than the raster. GDAL rounds the
/// scaling and leaves the raster's nodata as 0.
pub fn location(path: &str, base_level: u32, level: u32) -> Option<String> {
    if level > base_level {
        return None;
    }

    let ovr = if level == base_level {
        String::new()
    } else {
        format!("ovr={}&", base_level - level - 1)
    };

    Some(format!(
        "vrt://{path}?{ovr}ot=UInt16&scale={},{},0,65535&a_nodata=0",
        SCALE_MIN_M * TERRAIN_STEPS_PER_M,
        SCALE_MAX_M * TERRAIN_STEPS_PER_M
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scale_is_the_encoding_in_2_mm_steps() {
        assert_eq!(
            location("/t/sk.tif", 18, 14).unwrap(),
            "vrt:///t/sk.tif?ovr=3&ot=UInt16&scale=-250000,3026750,0,65535&a_nodata=0"
        );
    }

    /// Overview 0 is the first halving, so the base itself takes no `ovr`.
    #[test]
    fn the_overview_follows_the_base_zoom() {
        assert!(location("/t/it.tif", 15, 14).unwrap().contains("?ovr=0&"));
        assert!(location("/t/it.tif", 15, 8).unwrap().contains("?ovr=6&"));
        assert!(!location("/t/gedtm30.tif", 13, 13).unwrap().contains("ovr"));
    }

    /// The marcher falls back to a coarser level by itself; a raster blown up
    /// into a finer one would only cost reads.
    #[test]
    fn a_level_finer_than_the_raster_is_absent() {
        assert_eq!(location("/t/gedtm30.tif", 13, 14), None);
    }
}
