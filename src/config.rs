//! The rasters the pyramid indexes, and the levels it spans.
//!
//! Nothing here is authored. The datasets, their order and their credits come
//! from the elevation API's source list, everything about their rasters is
//! measured by `refresh`, and this reads back what it cached.

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

#[derive(Debug, Deserialize, Serialize)]
pub struct Grid {
    pub finest_level: u32,
    pub coarsest_level: u32,
}

/// One dataset the pyramid reads, as `refresh` measured its raster.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Source {
    /// The directory without its number, `es_30`, which is also the stem of
    /// the raster terrain-tiles built for it.
    pub id: String,
    /// The directory in the elevation source list, so a complaint about a
    /// value can be traced to something a person can edit.
    pub dir: String,
    pub api_name: String,
    /// The terrain-tiles raster.
    pub path: String,
    /// The directory's number, negated: higher wins.
    pub priority: i64,
    /// Zoom of the raster's own pixels; overviews run from there down.
    pub base_level: u32,
    pub bbox: [f64; 4],
}
#[derive(Debug, Serialize)]
pub struct Doc {
    pub grid: Grid,
    pub sources: Vec<Source>,
}

/// Hold the cache to what the index assumes of it.
pub fn validate(sources: &[Source]) -> Result<()> {
    // An index over nothing renders nothing; this is a broken source list.
    if sources.is_empty() {
        bail!("no dataset in the source list links a mercator.tif");
    }

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

    let mut sources: Vec<Source> = serde_json::from_str(&text).map_err(|e| {
        anyhow::anyhow!(
            "{}: {e}\nRun `dem-tool refresh` to rewrite it in this version's format.",
            path.display()
        )
    })?;

    // Priority order, so the first box holding a point is the one the mosaic
    // would take it from.
    sources.sort_by_key(|s| -s.priority);

    validate(&sources)?;

    Ok(Doc { grid, sources })
}
