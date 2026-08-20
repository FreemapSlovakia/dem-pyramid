//! sources.yaml: model, defaults resolution and validation.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Web Mercator, full extent, in projected metres.
pub const EARTH_CIRCUMFERENCE: f64 = 2.0 * std::f64::consts::PI * 6378137.0;

/// Projected (not ground) metres per pixel at level `z`.
pub fn level_res(z: u32) -> f64 {
    EARTH_CIRCUMFERENCE / 256.0 / f64::from(1u32 << z)
}

/// Ground metres per pixel at level `z` and latitude. The distinction matters:
/// `-tr` takes projected metres, while the zoom/range table is in ground
/// metres, and at 49N they differ by a factor of cos(49) = 0.656.
pub fn ground_res(z: u32, lat_deg: f64) -> f64 {
    level_res(z) * lat_deg.to_radians().cos()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FootprintMode {
    /// Union of the VRT's source rectangles: exact, and costs no pixel reads.
    Tiles,
    /// The declared lon/lat box: conservative, for single-file sources.
    Bbox,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Nodata {
    /// An explicit override, passed to gdalwarp as -srcnodata.
    Value(f64),
    /// The literal `declared`: trust what the dataset says.
    Declared(String),
}

impl Nodata {
    pub fn is_declared(&self) -> bool {
        matches!(self, Self::Declared(_))
    }

    pub fn value(&self) -> Option<f64> {
        match self {
            Self::Value(v) => Some(*v),
            Self::Declared(_) => None,
        }
    }
}

impl std::fmt::Display for Nodata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Value(v) => write!(f, "{v}"),
            Self::Declared(s) => write!(f, "{s}"),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Grid {
    pub crs: String,
    pub tile_px: u32,
    pub block_px: u32,
    pub finest_level: u32,
    pub coarsest_level: u32,
}

#[derive(Debug, Deserialize)]
struct Defaults {
    resampling: String,
    finest_level: u32,
    nodata: Nodata,
    footprint: FootprintMode,
}

#[derive(Debug, Deserialize)]
struct RawSource {
    id: String,
    api_name: String,
    path: String,
    priority: i64,
    native_res: f64,
    crs: String,
    bbox: [f64; 4],
    nodata: Option<Nodata>,
    finest_level: Option<u32>,
    resampling: Option<String>,
    footprint: Option<FootprintMode>,
    fill_nodata_md: Option<u32>,
    rebuild_vrt: Option<bool>,
    #[allow(dead_code)]
    note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Source {
    pub id: String,
    pub api_name: String,
    pub path: String,
    pub priority: i64,
    pub native_res: f64,
    pub crs: String,
    pub bbox: [f64; 4],
    pub nodata: Nodata,
    pub finest_level: u32,
    pub resampling: String,
    pub footprint: FootprintMode,
    pub fill_nodata_md: Option<u32>,
    pub rebuild_vrt: bool,
}

#[derive(Debug, Deserialize)]
struct RawDoc {
    grid: Grid,
    defaults: Defaults,
    sources: Vec<RawSource>,
}

#[derive(Debug, Serialize)]
pub struct Doc {
    pub grid: Grid,
    pub sources: Vec<Source>,
}

pub fn load(path: &Path) -> Result<Doc> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    let raw: RawDoc = serde_yaml_ng::from_str(&text)?;

    let mut sources = Vec::with_capacity(raw.sources.len());
    let mut by_id: HashMap<&str, ()> = HashMap::new();
    let mut by_priority: HashMap<i64, String> = HashMap::new();

    for r in &raw.sources {
        if by_id.insert(&r.id, ()).is_some() {
            bail!("duplicate source id: {}", r.id);
        }
        if let Some(other) = by_priority.insert(r.priority, r.id.clone()) {
            bail!(
                "duplicate priority {}: {other} and {} -- mosaic order would be \
                 non-deterministic",
                r.priority,
                r.id
            );
        }

        let [lo_lon, lo_lat, hi_lon, hi_lat] = r.bbox;
        if lo_lon >= hi_lon || lo_lat >= hi_lat {
            bail!("{}: degenerate bbox {:?}", r.id, r.bbox);
        }

        let finest = r.finest_level.unwrap_or(raw.defaults.finest_level);

        // A source is never upsampled into a level finer than its own data --
        // that is what keeps z14/z13 sparse and stops GEDTM30 from being blown
        // up 25x into the finest level.
        let gr = ground_res(finest, 49.0);
        if gr < r.native_res * 0.75 {
            bail!(
                "{}: finest_level {finest} is {gr:.2} m ground at 49N but the \
                 source is {} m -- that upsamples; raise finest_level",
                r.id,
                r.native_res
            );
        }

        sources.push(Source {
            id: r.id.clone(),
            api_name: r.api_name.clone(),
            path: r.path.clone(),
            priority: r.priority,
            native_res: r.native_res,
            crs: r.crs.clone(),
            bbox: r.bbox,
            nodata: r.nodata.clone().unwrap_or_else(|| raw.defaults.nodata.clone()),
            finest_level: finest,
            resampling: r
                .resampling
                .clone()
                .unwrap_or_else(|| raw.defaults.resampling.clone()),
            footprint: r.footprint.unwrap_or(raw.defaults.footprint),
            fill_nodata_md: r.fill_nodata_md,
            rebuild_vrt: r.rebuild_vrt.unwrap_or(false),
        });
    }

    sources.sort_by_key(|s| -s.priority);

    Ok(Doc {
        grid: raw.grid,
        sources,
    })
}

/// Spherical area of a lon/lat box, km².
pub fn bbox_area_km2(bbox: [f64; 4]) -> f64 {
    let [lo_lon, lo_lat, hi_lon, hi_lat] = bbox;
    let r = 6371.0_f64;
    (hi_lon - lo_lon).to_radians()
        * (hi_lat.to_radians().sin() - lo_lat.to_radians().sin()).abs()
        * r
        * r
}
