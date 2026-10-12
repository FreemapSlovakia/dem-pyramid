//! Measure the rasters the pyramid reads, once.
//!
//! The elevation source list says which datasets the pyramid holds, in what
//! order, and how each is credited, and links each one's `mercator.tif` --
//! the raster terrain-tiles serves. Everything else follows from that raster.
//! Measuring costs one header read per raster, so it is done here and cached
//! rather than on every start.
//!
//! The cache is a build artifact, not configuration: it is regenerated rather
//! than edited, and `check` fails when it no longer matches the data.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::config::{Source, level_res};
use crate::credit::Entry;
use crate::gdal_cli;
use crate::grid::{ORIGIN, TERRAIN_STEPS_PER_M, merc_to_lonlat};
use gdal::spatial_ref::SpatialRef;

/// terrain-tiles' nodata, which the index's conversion leaves as 0.
const TERRAIN_NODATA: f64 = -2_147_483_648.0;

/// `010-sk` -> `sk`, `090-es-29` -> `es_29`: the dataset without its
/// precedence.
pub(crate) fn id_of(dir: &str) -> String {
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

/// What the dataset directory's `mercator.tif` link points at, absolute: the
/// index is opened from wherever `serve` runs, and a link repointed later
/// shows in `check` as a changed path.
fn raster_of(dir: &Path) -> Result<String> {
    let link = dir.join("mercator.tif");
    let abs = std::fs::canonicalize(&link)
        .with_context(|| format!("{}: no raster behind it", link.display()))?;

    abs.to_str()
        .map(str::to_owned)
        .with_context(|| format!("{}: not UTF-8", abs.display()))
}

/// The zoom whose pixel this is, if it is one.
fn level_of(res: f64) -> Option<u32> {
    let z = (level_res(0) / res).log2().round();
    if !(0.0..=30.0).contains(&z) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let z = z as u32;
    ((level_res(z) - res).abs() < 1e-6 * res).then_some(z)
}

/// A measured raster: its base zoom and its lon/lat box.
struct Measured {
    base_level: u32,
    bbox: [f64; 4],
}

/// Held to everything the index's `vrt://` conversion assumes: EPSG:3857,
/// Int32 in 2 mm steps with terrain-tiles' nodata, and an overview per zoom
/// down to the coarsest level, each exactly half the last and on that zoom's
/// pixel grid. A raster off any of these would be read silently wrong -- the
/// GTI resamples a misaligned row onto its own grid -- not refused.
fn measure(info: &serde_json::Value, path: &str, coarsest: u32) -> Result<Measured> {
    let wkt = info["coordinateSystem"]["wkt"]
        .as_str()
        .with_context(|| format!("{path}: no coordinate system"))?;
    let srs = SpatialRef::from_wkt(wkt).with_context(|| format!("{path}: unreadable CRS"))?;

    if srs.auth_name().as_deref() != Some("EPSG") || srs.auth_code().ok() != Some(3857) {
        bail!("{path}: not EPSG:3857");
    }

    let gt: Vec<f64> = info["geoTransform"]
        .as_array()
        .with_context(|| format!("{path}: no geotransform"))?
        .iter()
        .map(|v| v.as_f64().unwrap_or(f64::NAN))
        .collect();

    if gt.len() != 6 || gt[2] != 0.0 || gt[4] != 0.0 || (gt[1] + gt[5]).abs() > 1e-9 * gt[1] {
        bail!("{path}: not a north-up square-pixel grid");
    }

    let level =
        level_of(gt[1]).with_context(|| format!("{path}: {} m is no zoom's pixel", gt[1]))?;

    if level < coarsest {
        bail!("{path}: z{level} is coarser than the pyramid");
    }

    let band = &info["bands"][0];

    if band["type"].as_str() != Some("Int32")
        || band["noDataValue"].as_f64() != Some(TERRAIN_NODATA)
        || band["scale"].as_f64() != Some(1.0 / TERRAIN_STEPS_PER_M)
        || band["offset"].as_f64().unwrap_or(0.0) != 0.0
    {
        bail!("{path}: not Int32 heights in 2 mm steps with nodata {TERRAIN_NODATA}");
    }

    // On the coarsest level's grid, so on every finer one too.
    let coarse = level_res(coarsest);
    let off = |v: f64| {
        let r = v.rem_euclid(coarse);
        r.min(coarse - r) > 1e-3 * gt[1]
    };
    if off(gt[0] + ORIGIN) || off(ORIGIN - gt[3]) {
        bail!("{path}: origin is off the z{coarsest} pixel grid");
    }

    let size = |v: &serde_json::Value| -> Option<(u64, u64)> {
        Some((v["size"][0].as_u64()?, v["size"][1].as_u64()?))
    };
    let (w, h) = size(info).with_context(|| format!("{path}: no raster size"))?;

    let halvings = level - coarsest;
    if w % (1 << halvings) != 0 || h % (1 << halvings) != 0 {
        bail!("{path}: {w}x{h} does not halve evenly down to z{coarsest}");
    }

    let overviews = band["overviews"].as_array().map_or(&[][..], Vec::as_slice);

    for k in 0..halvings {
        let want = (w >> (k + 1), h >> (k + 1));
        if overviews.get(k as usize).and_then(size) != Some(want) {
            bail!(
                "{path}: overview {k} should be {}x{} for z{}",
                want.0,
                want.1,
                level - k - 1
            );
        }
    }

    #[allow(clippy::cast_precision_loss)]
    let (x1, y0) = (gt[0] + w as f64 * gt[1], gt[3] + h as f64 * gt[5]);
    let (west, south) = merc_to_lonlat(gt[0], y0);
    let (east, north) = merc_to_lonlat(x1, gt[3]);

    Ok(Measured {
        base_level: level,
        bbox: [west, south, east, north],
    })
}

/// Measure one dataset's raster.
pub fn derive(entry: &Entry, coarsest: u32) -> Result<Source> {
    let id = id_of(&entry.dir);
    let path = raster_of(&entry.path)?;

    let info = gdal_cli::info_json(&path).with_context(|| format!("reading {path}"))?;
    let m = measure(&info, &path, coarsest)?;

    Ok(Source {
        id,
        dir: entry.dir.clone(),
        api_name: entry.name.clone(),
        path,
        priority: priority_of(&entry.dir)?,
        base_level: m.base_level,
        bbox: m.bbox,
    })
}

/// Where the cache lives. Under the data root rather than the checkout: it
/// describes the data, is regenerated from it, and is never committed.
pub fn cache_path(root: &Path) -> PathBuf {
    root.join("state").join("sources.json")
}

/// Measure every dataset that links a `mercator.tif`, reporting
/// failures rather than stopping at the first. The ids that failed come back
/// with the rest.
pub fn derive_all(entries: &[Entry], coarsest: u32) -> (Vec<Source>, Vec<String>) {
    let mut out = Vec::new();
    let mut failed = Vec::new();

    for entry in entries.iter().filter(|e| e.mercator) {
        match derive(entry, coarsest) {
            Ok(d) => out.push(d),
            Err(e) => {
                println!("FAIL {}: {e:#}", entry.dir);
                failed.push(id_of(&entry.dir));
            }
        }
    }

    out.sort_by_key(|d| -d.priority);

    (out, failed)
}

pub fn write(path: &Path, derived: &[Source]) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("no parent")?)?;

    // Through a temp file: a half-written cache is an index against nothing.
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
        assert_eq!(id_of("245-de_by"), "de_by");
    }

    /// Ascending directories, descending priority.
    #[test]
    fn precedence_survives_the_reversal() {
        let mut dirs = ["999-gedtm30", "010-sk", "240-be_vlg"];
        let mut by_priority = dirs;
        by_priority.sort_by_key(|d| -priority_of(d).unwrap());
        dirs.sort_unstable();

        assert_eq!(dirs, by_priority);
    }

    #[test]
    fn the_raster_is_the_target_of_the_mercator_link() {
        let base = std::env::temp_dir().join(format!("dem-tool-raster-of-{}", std::process::id()));
        let (with, without) = (base.join("010-sk"), base.join("020-cz"));
        std::fs::create_dir_all(&with).unwrap();
        std::fs::create_dir_all(&without).unwrap();
        std::fs::write(base.join("sk.tif"), "").unwrap();
        std::os::unix::fs::symlink(base.join("sk.tif"), with.join("mercator.tif")).unwrap();

        let target = std::fs::canonicalize(base.join("sk.tif")).unwrap();
        assert_eq!(raster_of(&with).unwrap(), target.to_str().unwrap());
        assert!(raster_of(&without).is_err());

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_pixel_is_matched_to_its_zoom_exactly() {
        assert_eq!(level_of(0.597_164_283_477_939), Some(18));
        assert_eq!(level_of(19.109_257_071_294_063), Some(13));
        assert_eq!(level_of(1.0), None);
    }

    /// gdalinfo's JSON for sk.tif, trimmed to what is read.
    fn sk_info() -> serde_json::Value {
        let srs = SpatialRef::from_epsg(3857).unwrap();
        let (w, h) = (1_572_864u64, 786_432u64);
        let ovr: Vec<_> = (1..=10)
            .map(|k| serde_json::json!({ "size": [w >> k, h >> k] }))
            .collect();

        serde_json::json!({
            "geoTransform": [
                1_721_973.373_208_452, level_res(18), 0.0,
                6_418_264.391_049_679, 0.0, -level_res(18)
            ],
            "size": [w, h],
            "coordinateSystem": { "wkt": srs.to_wkt().unwrap() },
            "bands": [{
                "type": "Int32",
                "noDataValue": TERRAIN_NODATA,
                "scale": 0.002,
                "offset": 0.0,
                "overviews": ovr,
            }],
        })
    }

    #[test]
    fn sk_measures_at_z18_with_its_box() {
        let m = measure(&sk_info(), "sk.tif", 8).unwrap();
        assert_eq!(m.base_level, 18);

        let [w, s, e, n] = m.bbox;
        assert!(
            (w - 15.46875).abs() < 1e-9 && (e - 23.90625).abs() < 1e-9,
            "{:?}",
            m.bbox
        );
        assert!(s < 47.75 && n > 49.6 && s < n, "{:?}", m.bbox);
    }

    /// One overview short of z8, and the coarsest level would be read from a
    /// raster at the wrong resolution, resampled without a word.
    #[test]
    fn an_overview_chain_short_of_the_coarsest_level_is_refused() {
        let mut info = sk_info();
        info["bands"][0]["overviews"].as_array_mut().unwrap().pop();
        assert!(measure(&info, "sk.tif", 8).is_err());
    }

    #[test]
    fn heights_in_other_units_are_refused() {
        let mut info = sk_info();
        info["bands"][0]["scale"] = serde_json::json!(0.01);
        assert!(measure(&info, "sk.tif", 8).is_err());
    }

    /// On the z18 grid but one z18 pixel off z8's, so the z8 overview would sit
    /// across the index's pixels.
    #[test]
    fn a_raster_off_the_coarsest_grid_is_refused() {
        let mut info = sk_info();
        info["geoTransform"][0] = serde_json::json!(1_721_973.373_208_452 + level_res(18));
        assert!(measure(&info, "sk.tif", 8).is_err());
    }

    #[test]
    fn a_raster_that_does_not_halve_evenly_is_refused() {
        let mut info = sk_info();
        info["size"][0] = serde_json::json!(1_572_864u64 - 256);
        assert!(measure(&info, "sk.tif", 8).is_err());
    }
}
