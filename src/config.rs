//! The sources the pyramid is built from, and the grid it is built on.
//!
//! Nothing here is authored. The datasets come from the elevation API's source
//! list, everything about them is measured from the rasters by `refresh`, and
//! this reads back what it cached. The validation below is what the cache is
//! held to before anything builds against it.

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

/// One dataset the pyramid builds, as `refresh` measured it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Source {
    pub id: String,
    /// The directory in the elevation source list it came from, so a complaint
    /// about a value can be traced to something a person can edit.
    pub dir: String,
    pub api_name: String,
    pub path: String,
    pub priority: i64,
    pub native_res: f64,
    pub bbox: [f64; 4],
    pub nodata: Nodata,
    pub finest_level: u32,
    pub resampling: String,
    pub footprint: FootprintMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill_nodata_md: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct Doc {
    pub grid: Grid,
    pub sources: Vec<Source>,
}

/// Hold the cache to what the build assumes of it.
///
/// Measured values can still be wrong together -- two datasets at one
/// priority, a box with no area -- and every one of these would surface as
/// something far stranger during a build than a refusal here.
pub fn validate(sources: &[Source]) -> Result<()> {
    let mut by_id: HashMap<&str, ()> = HashMap::new();
    let mut by_priority: HashMap<i64, &str> = HashMap::new();

    for s in sources {
        if by_id.insert(&s.id, ()).is_some() {
            bail!("duplicate source id: {}", s.id);
        }
        if let Some(other) = by_priority.insert(s.priority, &s.id) {
            bail!(
                "duplicate priority {}: {other} and {} -- mosaic order would be \
                 non-deterministic",
                s.priority,
                s.id
            );
        }

        let [lo_lon, lo_lat, hi_lon, hi_lat] = s.bbox;
        if lo_lon >= hi_lon || lo_lat >= hi_lat {
            bail!("{}: degenerate bbox {:?}", s.id, s.bbox);
        }

        // A source is never upsampled into a level finer than its own data --
        // that is what keeps z14/z13 sparse and stops GEDTM30 from being blown
        // up 25x into the finest level.
        let gr = ground_res(s.finest_level, 49.0);
        if gr < s.native_res * 0.75 {
            bail!(
                "{}: finest_level {} is {gr:.2} m ground at 49N but the source \
                 is {} m -- that upsamples",
                s.id,
                s.finest_level,
                s.native_res
            );
        }
    }

    Ok(())
}

/// Read what `refresh` measured, newest wins over nothing: a missing cache is
/// an error naming the command that writes it, because every other failure it
/// would cause is harder to read than this one.
pub fn load(path: &Path, grid: Grid) -> Result<Doc> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        anyhow::anyhow!(
            "{}: {e}\nRun `dem-tool refresh` on the data host to measure the \
             sources and write it.",
            path.display()
        )
    })?;

    let mut sources: Vec<Source> =
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;

    // Priority order, so the first box holding a point is the one the mosaic
    // would take it from.
    sources.sort_by_key(|s| -s.priority);

    validate(&sources)?;

    Ok(Doc { grid, sources })
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
