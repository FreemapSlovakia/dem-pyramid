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

fn bbox_of(info: &serde_json::Value, path: &str) -> Result<[f64; 4]> {
    let ring = info["wgs84Extent"]["coordinates"][0]
        .as_array()
        .with_context(|| format!("{path}: no wgs84Extent"))?;

    let (mut w, mut s, mut e, mut n) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);

    for p in ring {
        let (lon, lat) = (
            p[0].as_f64().context("non-numeric extent")?,
            p[1].as_f64().context("non-numeric extent")?,
        );
        w = w.min(lon);
        s = s.min(lat);
        e = e.max(lon);
        n = n.max(lat);
    }

    if w >= e || s >= n {
        bail!("{path}: degenerate extent");
    }

    Ok([w, s, e, n])
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

pub fn write(root: &Path, derived: &[Source]) -> Result<()> {
    let path = cache_path(root);

    std::fs::create_dir_all(path.parent().context("no parent")?)?;

    // Through a temp file: a half-written cache is a build against nothing.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(derived)? + "\n")?;
    std::fs::rename(&tmp, &path)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_drops_the_prefix_and_keeps_the_rest() {
        assert_eq!(id_of("010-sk"), "sk");
        assert_eq!(id_of("090-es-29"), "es_29");
        assert_eq!(id_of("140-fr-fr1-lamb93-ign69"), "fr_fr1_lamb93_ign69");
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

    #[test]
    fn footprint_mode_follows_the_file() {
        assert_eq!(footprint_of("/dtm/pl/poland.vrt"), FootprintMode::Tiles);
        assert_eq!(footprint_of("/dtm/sk.tif"), FootprintMode::Bbox);
    }
}
