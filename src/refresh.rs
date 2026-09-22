//! Derive every source's build metadata from the data, once.
//!
//! The elevation source list says which datasets exist and where their rasters
//! are; everything else the build needs follows from the rasters themselves or
//! from a rule over them. Measuring costs one header read per source -- about
//! 24 s for the whole catalogue -- which is nothing once, and far too much per
//! `warp-env` invocation, of which a build makes thousands. So it is measured
//! here and cached.
//!
//! The cache is a build artifact, not configuration: it is regenerated rather
//! than edited, and `check` fails when it no longer matches the data.

use anyhow::{Context, Result, bail};
use std::path::Path;

use crate::config::{FootprintMode, Nodata, Source, ground_res};
use crate::credit::Entry;
use crate::gdal_cli;
use gdal::spatial_ref::{AxisMappingStrategy, CoordTransform, SpatialRef};

/// Latitude the level/resolution table is quoted at. Central Europe, where
/// most of the data is.
const REF_LAT: f64 = 49.0;

/// A source is never materialised into a level finer than its own data. The
/// slack absorbs sources whose nominal resolution is a rounder number than
/// their geotransform.
const UPSAMPLE_SLACK: f64 = 0.75;

/// Distance, in pixels, that `gdal_fillnodata` searches when healing speckle.
/// Far enough to bridge single pixels lost to a sentinel that collides with
/// real terrain, short enough to leave genuine coverage gaps alone.
pub const FILL_NODATA_MD: u32 = 5;

/// `010-sk` -> `sk`, `090-es-29` -> `es_29`.
///
/// The prefix is precedence and belongs in `priority`; the rest names the
/// dataset. Dashes become underscores because the id is a path component for
/// `norm/` and `footprints/`.
fn id_of(dir: &str) -> String {
    dir.split_once('-')
        .map_or(dir, |(_, rest)| rest)
        .replace('-', "_")
}

/// The two lists are precedence orders written in opposite directions: the
/// source list ascends, so `010` outranks `999`, while priority descends.
fn priority_of(dir: &str) -> Result<i64> {
    let prefix = dir
        .split_once('-')
        .map_or(dir, |(head, _)| head)
        .parse::<i64>()
        .with_context(|| format!("{dir}: directory does not start with a number"))?;

    Ok(-prefix)
}

/// The finest level this source can be materialised into without being
/// upsampled.
fn finest_level_of(native_res: f64, coarsest: u32, finest: u32) -> u32 {
    (coarsest..=finest)
        .rev()
        .find(|&z| ground_res(z, REF_LAT) >= native_res * UPSAMPLE_SLACK)
        .unwrap_or(coarsest)
}

/// Average whenever an output pixel covers more than one source pixel, which
/// is every national source. When it covers less -- GEDTM30's 30 m read into
/// z12's 25 m ground -- there is nothing to average, and averaging only blurs;
/// bilinear interpolates instead, where cubicspline would overshoot ridges.
fn resampling_of(native_res: f64, finest_level: u32) -> String {
    if ground_res(finest_level, REF_LAT) < native_res {
        "bilinear".to_owned()
    } else {
        "average".to_owned()
    }
}

/// `tiles` reads a VRT's source rectangles; there is nothing to read in a
/// single raster, whose real edge its nodata resolves at warp time anyway.
fn footprint_of(path: &str) -> FootprintMode {
    if path.ends_with(".vrt") {
        FootprintMode::Tiles
    } else {
        FootprintMode::Bbox
    }
}

/// Only a sentinel that is itself a plausible elevation can collide with real
/// terrain. Every other sentinel in the catalogue is far outside the range.
fn fill_nodata_of(nodata: Option<f64>) -> Option<u32> {
    (nodata == Some(0.0)).then_some(FILL_NODATA_MD)
}

/// How finely the raster is sampled when working out its lon/lat extent.
/// 65x65 points, computed once per source, is not worth economising on.
const EXTENT_STEPS: usize = 64;

/// Degrees added to every side of the derived extent, to absorb what the
/// sampling still misses between points.
///
/// Erring wide costs a read that finds nodata and falls through; erring narrow
/// loses data silently. The bias is deliberate, and matches what the elevation
/// API allows itself for the same derivation.
const EXTENT_MARGIN_DEG: f64 = 0.01;

/// The source's extent in lon/lat.
///
/// Not the four corners transformed: a projected edge bows away from the
/// straight line between them -- a fifth of a degree for a UTM-width extent at
/// high latitude, always outwards, so a corner hull clips it. An interior grid
/// catches that, and also the case an edge walk would miss: a polar
/// stereographic sheet containing the pole reaches 90 degrees at an interior
/// pixel, on no edge at all.
fn bbox_of(info: &serde_json::Value, path: &str) -> Result<[f64; 4]> {
    let gt: Vec<f64> = info["geoTransform"]
        .as_array()
        .with_context(|| format!("{path}: no geotransform"))?
        .iter()
        .map(|v| v.as_f64().unwrap_or(f64::NAN))
        .collect();

    if gt.len() != 6 || gt.iter().any(|v| !v.is_finite()) {
        bail!("{path}: unusable geotransform");
    }

    let size = |i: usize| -> Result<f64> {
        info["size"][i]
            .as_f64()
            .filter(|v| *v > 0.0)
            .with_context(|| format!("{path}: no raster size"))
    };

    let (width, height) = (size(0)?, size(1)?);

    let wkt = info["coordinateSystem"]["wkt"]
        .as_str()
        .with_context(|| format!("{path}: no coordinate system"))?;

    let mut src = SpatialRef::from_wkt(wkt).with_context(|| format!("{path}: unreadable CRS"))?;
    let mut dst = SpatialRef::from_epsg(4326)?;

    // Both, or EPSG:4326 hands back lat/lon and every box comes out transposed.
    src.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
    dst.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);

    let ct = CoordTransform::new(&src, &dst)?;

    let n = EXTENT_STEPS + 1;
    let mut xs = Vec::with_capacity(n * n);
    let mut ys = Vec::with_capacity(n * n);

    for i in 0..n {
        for j in 0..n {
            let px = width * (i as f64) / (EXTENT_STEPS as f64);
            let py = height * (j as f64) / (EXTENT_STEPS as f64);

            xs.push(gt[0] + px * gt[1] + py * gt[2]);
            ys.push(gt[3] + px * gt[4] + py * gt[5]);
        }
    }

    // A point outside the projection's valid domain comes back non-finite and
    // makes the whole call fail; it bounds nothing, so the survivors are what
    // matter rather than the return.
    drop(ct.transform_coords(&mut xs, &mut ys, &mut []));

    let (mut w, mut s, mut e, mut n_) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    let mut held = 0usize;

    for (lon, lat) in xs.iter().zip(&ys) {
        if lon.is_finite() && lat.is_finite() && lat.abs() <= 90.0 {
            w = w.min(*lon);
            s = s.min(*lat);
            e = e.max(*lon);
            n_ = n_.max(*lat);
            held += 1;
        }
    }

    if held == 0 {
        bail!("{path}: no sampled point has a lon/lat");
    }

    // A partial failure -- PROJ refusing points outside the projection's area
    // of use -- clips the box inwards, which is the direction that loses data,
    // by more than the margin covers. Refused rather than warned about: the
    // cache would otherwise hold a short box that `check` re-measures to the
    // same short box and calls agreement, and the source builds permanently
    // shy of its own edge with nothing ever saying so.
    if held < xs.len() {
        bail!(
            "{path}: {} of {} extent samples did not transform, so its box \
             would be clipped",
            xs.len() - held,
            held + (xs.len() - held)
        );
    }

    if w >= e || s >= n_ {
        bail!("{path}: degenerate extent");
    }

    // A raster crossing the antimeridian samples near both -180 and +180 and
    // yields a box the long way round the globe -- which reads here as the
    // global fallback and is skipped by the build with a reassuring message.
    // One lon/lat box has no way to say "wraps", so say so instead.
    let mut lons: Vec<f64> = xs.iter().copied().filter(|v| v.is_finite()).collect();
    lons.sort_by(f64::total_cmp);

    let widest_gap = lons.windows(2).map(|p| p[1] - p[0]).fold(0.0_f64, f64::max);

    if widest_gap > 180.0 {
        bail!("{path}: crosses the antimeridian, which one lon/lat box cannot express");
    }

    Ok([
        (w - EXTENT_MARGIN_DEG).max(-180.0),
        (s - EXTENT_MARGIN_DEG).max(-90.0),
        (e + EXTENT_MARGIN_DEG).min(180.0),
        (n_ + EXTENT_MARGIN_DEG).min(90.0),
    ])
}

/// Metres per pixel, whatever the CRS measures in.
fn native_res_of(info: &serde_json::Value, path: &str) -> Result<f64> {
    let res = info["geoTransform"][1]
        .as_f64()
        .with_context(|| format!("{path}: no geotransform"))?
        .abs();

    // A geographic CRS quotes degrees; convert at the equator, which is how
    // the declared figures were quoted.
    Ok(if res < 0.01 { res * 111_320.0 } else { res })
}

/// Measure one source.
pub fn derive(entry: &Entry, coarsest: u32, finest: u32) -> Result<Source> {
    let info = gdal_cli::info_json(&entry.file)
        .with_context(|| format!("{}: reading {}", entry.dir, entry.file))?;

    let native_res = native_res_of(&info, &entry.file)?;
    let finest_level = finest_level_of(native_res, coarsest, finest);
    let declared = info["bands"][0]["noDataValue"].as_f64();

    Ok(Source {
        id: id_of(&entry.dir),
        dir: entry.dir.clone(),
        api_name: entry.name.clone(),
        path: entry.file.clone(),
        priority: priority_of(&entry.dir)?,
        native_res,
        nodata: match declared {
            Some(_) => Nodata::Declared("declared".to_owned()),
            None => bail!("{}: declares no nodata", entry.dir),
        },
        bbox: bbox_of(&info, &entry.file)?,
        finest_level,
        resampling: resampling_of(native_res, finest_level),
        footprint: footprint_of(&entry.file),
        fill_nodata_md: fill_nodata_of(declared),
    })
}

/// Where the cache lives. Under the data root rather than the checkout: it
/// describes the data, is regenerated from it, and is never committed.
pub fn cache_path(root: &Path) -> std::path::PathBuf {
    root.join("state").join("sources.json")
}

/// Measure every dataset in the source list, reporting failures rather than
/// stopping at the first.
pub fn derive_all(entries: &[Entry], coarsest: u32, finest: u32) -> (Vec<Source>, usize) {
    let mut out = Vec::new();
    let mut failed = 0usize;

    for entry in entries.iter().filter(|e| e.pyramid) {
        match derive(entry, coarsest, finest) {
            Ok(d) => out.push(d),
            Err(e) => {
                println!("FAIL {}: {e:#}", entry.dir);
                failed += 1;
            }
        }
    }

    out.sort_by_key(|d| -d.priority);

    (out, failed)
}

pub fn write(path: &Path, derived: &[Source]) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("no parent")?)?;

    // Through a temp file: a half-written cache is a build against nothing.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(derived)? + "\n")?;
    std::fs::rename(&tmp, path)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_drops_the_prefix_and_keeps_the_rest() {
        assert_eq!(id_of("010-sk"), "sk");
        assert_eq!(id_of("090-es-29"), "es_29");
        assert_eq!(id_of("250-sonny-de"), "sonny_de");
    }

    /// Ascending directories, descending priority.
    #[test]
    fn precedence_survives_the_reversal() {
        let mut dirs = ["999-gedtm30", "010-sk", "240-be"];
        let mut by_priority = dirs;
        by_priority.sort_by_key(|d| -priority_of(d).unwrap());
        dirs.sort_unstable();

        assert_eq!(dirs, by_priority);
    }

    /// A source is never blown up into a level finer than its own data.
    #[test]
    fn no_source_is_upsampled_into_its_finest_level() {
        for res in [0.5, 1.0, 2.0, 5.0, 20.0, 30.0] {
            let z = finest_level_of(res, 8, 14);
            assert!(
                ground_res(z, REF_LAT) >= res * UPSAMPLE_SLACK,
                "{res} m landed at z{z}"
            );
        }
    }

    #[test]
    fn the_levels_match_what_the_catalogue_used() {
        // 1 m national data into z14, GEDTM30's 30 m into z12, Sonny's 20 m
        // likewise -- z13 is 12.5 m ground, which would upsample both.
        assert_eq!(finest_level_of(1.0, 8, 14), 14);
        assert_eq!(finest_level_of(0.5, 8, 14), 14);
        assert_eq!(finest_level_of(30.0, 8, 14), 12);
        assert_eq!(finest_level_of(20.0, 8, 14), 12);
    }

    /// Average when an output pixel covers more than one source pixel.
    #[test]
    fn resampling_follows_the_ratio() {
        // z14 is 6.27 m ground at 49N, z12 is 25.1 m.
        assert_eq!(resampling_of(1.0, 14), "average");
        assert_eq!(resampling_of(5.0, 14), "average");
        assert_eq!(resampling_of(20.0, 12), "average");
        assert_eq!(resampling_of(30.0, 12), "bilinear");
    }

    #[test]
    fn only_a_sentinel_that_could_be_terrain_is_healed() {
        assert_eq!(fill_nodata_of(Some(0.0)), Some(FILL_NODATA_MD));
        assert_eq!(fill_nodata_of(Some(-9999.0)), None);
        assert_eq!(fill_nodata_of(Some(3.4e38)), None);
        assert_eq!(fill_nodata_of(None), None);
    }

    /// A gdalinfo-shaped value for a raster in one UTM zone, high enough for
    /// the meridian convergence to matter.
    fn utm_info(epsg: u32, origin: (f64, f64), res: f64, size: (u32, u32)) -> serde_json::Value {
        let srs = SpatialRef::from_epsg(epsg).unwrap();

        serde_json::json!({
            "geoTransform": [origin.0, res, 0.0, origin.1, 0.0, -res],
            "size": [size.0, size.1],
            "coordinateSystem": { "wkt": srs.to_wkt().unwrap() },
        })
    }

    /// Lon/lat extent of a raster's four corners alone, for comparison.
    fn corner_hull(epsg: u32, origin: (f64, f64), res: f64, size: (u32, u32)) -> [f64; 4] {
        let mut src = SpatialRef::from_epsg(epsg).unwrap();
        let mut dst = SpatialRef::from_epsg(4326).unwrap();
        src.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
        dst.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
        let ct = CoordTransform::new(&src, &dst).unwrap();

        let (w, h) = (f64::from(size.0) * res, f64::from(size.1) * res);
        let mut xs = vec![origin.0, origin.0 + w, origin.0, origin.0 + w];
        let mut ys = vec![origin.1, origin.1, origin.1 - h, origin.1 - h];
        ct.transform_coords(&mut xs, &mut ys, &mut []).unwrap();

        [
            xs.iter().copied().fold(f64::MAX, f64::min),
            ys.iter().copied().fold(f64::MAX, f64::min),
            xs.iter().copied().fold(f64::MIN, f64::max),
            ys.iter().copied().fold(f64::MIN, f64::max),
        ]
    }

    /// The corners of a projected rectangle do not bound it: its edges bow
    /// outwards between them, and at high latitude by kilometres.
    ///
    /// Held against the corner hull *plus the margin*, because the corners are
    /// themselves grid points -- comparing against the bare hull would pass on
    /// the margin alone, and corner-only sampling would satisfy it.
    #[test]
    fn the_extent_is_wider_than_its_corners() {
        // UTM 33N, 400 km wide and 1000 km tall, reaching towards the pole,
        // where the meridian convergence is sharpest.
        let (epsg, origin, res, size) = (32633, (300_000.0, 7_800_000.0), 1000.0, (400, 1000));

        let [_, _, _, n] = bbox_of(&utm_info(epsg, origin, res, size), "synthetic").unwrap();
        let [_, _, _, hull_n] = corner_hull(epsg, origin, res, size);

        // The northern edge bows ~0.078 deg past its corners, well clear of
        // the 0.01 deg margin, so this fails if the interior is not sampled.
        assert!(
            n > hull_n + EXTENT_MARGIN_DEG,
            "north edge {n} is within the margin of the corner hull {hull_n}"
        );
    }

    /// Erring wide costs a nodata read; erring narrow loses data. Held against
    /// the same box derived without the margin, so it fails if the margin
    /// stops being applied.
    #[test]
    fn the_extent_is_padded_outwards() {
        let info = utm_info(32633, (300_000.0, 6_000_000.0), 100.0, (1000, 1000));
        let [w, s, e, n] = bbox_of(&info, "synthetic").unwrap();
        let [hw, hs, he, hn] = corner_hull(32633, (300_000.0, 6_000_000.0), 100.0, (1000, 1000));

        // A 100 km square this far from the pole barely bows, so the corner
        // hull is within a hair of the unpadded sample extent and the margin
        // is what separates the two.
        assert!(
            w < hw && s < hs && e > he && n > hn,
            "box not padded outwards"
        );
        assert!(
            (hw - w) > EXTENT_MARGIN_DEG / 2.0,
            "west padding {} is not the margin",
            hw - w
        );
    }

    #[test]
    fn a_raster_with_no_size_says_so() {
        let mut info = utm_info(32633, (300_000.0, 6_000_000.0), 100.0, (1000, 1000));
        info["size"] = serde_json::json!([0, 0]);

        let err = bbox_of(&info, "synthetic").unwrap_err().to_string();
        assert!(err.contains("no raster size"), "{err}");
    }

    #[test]
    fn footprint_mode_follows_the_file() {
        assert_eq!(footprint_of("/dtm/pl/poland.vrt"), FootprintMode::Tiles);
        assert_eq!(footprint_of("/dtm/sk.tif"), FootprintMode::Bbox);
    }
}
