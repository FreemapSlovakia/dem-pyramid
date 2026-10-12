//! What the pyramid serves, and the levels it spans.
//!
//! Nothing here is authored. Each level of the index `bin/index.sh` writes
//! holds a row per dataset reaching it, carrying its id and priority, with the
//! raster's extent as the row's geometry; what renders read and what they
//! credit both come from there.

use anyhow::{Result, anyhow, bail};
use gdal::vector::LayerAccess;
use gdal::{Dataset, DatasetOptions, GdalOpenFlags};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::coverage::Coverage;
use crate::grid::merc_to_lonlat;
use std::sync::Arc;

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

#[derive(Debug, Serialize)]
pub struct Grid {
    pub finest_level: u32,
    pub coarsest_level: u32,
}

/// One dataset the index serves.
#[derive(Debug, Clone, Serialize)]
pub struct Source {
    /// The directory without its number, dashes as underscores: `es_30`.
    pub id: String,
    /// The directory's number, negated: higher wins.
    pub priority: i64,
    /// The raster's extent, or where it has data once `coverage` is attached.
    pub bbox: [f64; 4],
    /// Its row on the coarsest level that holds it.
    #[serde(skip)]
    pub location: String,
    /// Where it has data; `serve` attaches it, so crediting can tell a
    /// country from the tile-aligned rectangle its raster fills.
    #[serde(skip)]
    pub coverage: Option<Arc<Coverage>>,
}

#[derive(Debug, Serialize)]
pub struct Doc {
    pub grid: Grid,
    pub sources: Vec<Source>,
}

/// One row of one level's index.
#[derive(Debug, PartialEq)]
pub struct Row {
    pub dataset: String,
    pub priority: i64,
    /// What the GTI opens: the raster's overview for this level, converted.
    pub location: String,
    pub bbox: [f64; 4],
}

const HINT: &str = "Run `bin/index.sh` to write it.";

/// Where the index lives under the data root.
pub fn index_dir(root: &Path) -> PathBuf {
    root.join("index")
}

/// Level `z`'s index, as both the marcher and the loader open it.
pub fn level_path(index_dir: &Path, z: u32) -> PathBuf {
    index_dir.join(format!("z{z}.gti.gpkg"))
}

/// Every row of level `z`, or `None` where the level has no index.
fn read_level(index_dir: &Path, z: u32) -> Result<Option<Vec<Row>>> {
    let path = level_path(index_dir, z);
    if !path.exists() {
        return Ok(None);
    }
    let err = |e: &dyn std::fmt::Display| anyhow!("{}: {e}\n{HINT}", path.display());

    // As the GeoPackage it is, not as the raster the GTI driver would take it
    // for, which has no layers.
    let ds = Dataset::open_ex(
        &path,
        DatasetOptions {
            open_flags: GdalOpenFlags::GDAL_OF_VECTOR,
            allowed_drivers: Some(&["GPKG"]),
            ..Default::default()
        },
    )
    .map_err(|e| err(&e))?;
    let mut layer = ds.layer_by_name("tiles").map_err(|e| err(&e))?;

    // On the layer, not per row, so a level with no rows cannot skip it.
    let field = |name: &str| {
        layer
            .defn()
            .field_index(name)
            .map_err(|_| err(&format!("no `{name}` column")))
    };
    let (dataset_ix, priority_ix, location_ix) =
        (field("dataset")?, field("priority")?, field("location")?);

    let mut rows = Vec::new();

    for f in layer.features() {
        let dataset = f
            .field_as_string(dataset_ix)?
            .ok_or_else(|| err(&"a row without a dataset"))?;
        let priority = f
            .field_as_integer64(priority_ix)?
            .ok_or_else(|| err(&format!("{dataset} has no priority")))?;
        let location = f.field_as_string(location_ix)?.unwrap_or_default();
        let env = f
            .geometry()
            .ok_or_else(|| err(&format!("{dataset} has no extent")))?
            .envelope();
        let (west, south) = merc_to_lonlat(env.MinX, env.MinY);
        let (east, north) = merc_to_lonlat(env.MaxX, env.MaxY);

        rows.push(Row {
            dataset,
            priority,
            location,
            bbox: [west, south, east, north],
        });
    }

    // `index.sh` removes a level no dataset reaches rather than writing it
    // empty, so an empty one was not written by it.
    if rows.is_empty() {
        return Err(err(&"no rows"));
    }

    Ok(Some(rows))
}

/// Each indexed level's rows, coarsest first.
pub type Levels = Vec<(u32, Vec<Row>)>;

pub fn read_levels(index_dir: &Path, grid: &Grid) -> Result<Levels> {
    let mut levels = Vec::new();

    for z in grid.coarsest_level..=grid.finest_level {
        if let Some(rows) = read_level(index_dir, z)? {
            levels.push((z, rows));
        }
    }

    if levels.is_empty() {
        bail!("{}: no index\n{HINT}", index_dir.display());
    }

    Ok(levels)
}

/// The datasets the index serves, highest priority first: every dataset on
/// any level, since a render reads them all, and a level indexed on its own
/// may hold one the others do not yet. Each level orders its own pixels by
/// its own rows, so priorities that differ or collide between levels -- an
/// index run cut short across a renumbering -- cost only the order credits
/// are listed in, which takes the coarsest level's; `check` reports the lag.
pub fn from_levels(levels: &Levels, grid: Grid) -> Result<Doc> {
    let mut sources: Vec<Source> = Vec::new();

    for (_, rows) in levels {
        for r in rows {
            if !sources.iter().any(|s| s.id == r.dataset) {
                sources.push(Source {
                    id: r.dataset.clone(),
                    priority: r.priority,
                    bbox: r.bbox,
                    location: r.location.clone(),
                    coverage: None,
                });
            }
        }
    }

    if sources.is_empty() {
        bail!("the index holds no dataset\n{HINT}");
    }

    // Priority order, so the first box holding a point is the one the mosaic
    // would take it from.
    sources.sort_by_key(|s| -s.priority);

    Ok(Doc { grid, sources })
}

pub fn load(index_dir: &Path, grid: Grid) -> Result<Doc> {
    from_levels(&read_levels(index_dir, &grid)?, grid)
}

/// Datasets the mosaic can order: no id twice and no priority shared, given as
/// (id, priority, what to name it by in a complaint).
pub fn check_distinct<'a>(items: impl IntoIterator<Item = (&'a str, i64, &'a str)>) -> Result<()> {
    let mut ids: HashMap<&str, &str> = HashMap::new();
    let mut priorities: HashMap<i64, &str> = HashMap::new();

    for (id, priority, label) in items {
        if let Some(other) = ids.insert(id, label) {
            bail!("{other} and {label} are both dataset {id}");
        }
        if let Some(other) = priorities.insert(priority, label) {
            bail!(
                "{other} and {label} share priority {priority}, so the mosaic order \
                 between them is undefined"
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(dataset: &str, priority: i64) -> Row {
        Row {
            dataset: dataset.into(),
            priority,
            location: format!("vrt:///t/{dataset}.tif"),
            bbox: [0.0, 0.0, 1.0, 1.0],
        }
    }

    fn grid() -> Grid {
        Grid {
            finest_level: 14,
            coarsest_level: 8,
        }
    }

    /// A dataset on a fine level only is still served: renders read it.
    #[test]
    fn every_level_contributes_its_datasets() {
        let levels = vec![
            (8, vec![row("sk", -10)]),
            (14, vec![row("sk", -10), row("de_by", -245)]),
        ];
        let ids: Vec<String> = from_levels(&levels, grid())
            .unwrap()
            .sources
            .into_iter()
            .map(|s| s.id)
            .collect();

        assert_eq!(ids, ["sk", "de_by"]);
    }

    /// Levels from two index runs across a renumbering disagree, even to the
    /// point of two datasets sharing a number; serving goes on regardless.
    #[test]
    fn levels_that_disagree_on_priority_still_load() {
        let levels = vec![
            (8, vec![row("sk", -10), row("cz", -20)]),
            (14, vec![row("sk", -20), row("cz", -10), row("pl", -10)]),
        ];
        let doc = from_levels(&levels, grid()).unwrap();

        assert_eq!(doc.sources[0].id, "sk");
        assert_eq!(doc.sources.len(), 3);
    }
}
