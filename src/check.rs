//! Can the pyramid be indexed and served as things stand?
//!
//! Fails on what would stop `bin/index.sh` or `serve`: a raster the index
//! cannot read as it assumes, two datasets the mosaic cannot order, a dataset
//! the source list does not credit, or an index `serve` cannot load. Where the
//! index merely lags the source list it says so and passes, since a stale
//! index is still a fully credited one. Headers only -- no pixel is read.

use anyhow::{Result, bail};
use std::path::Path;

use crate::config::{self, Grid, Levels};
use crate::measure::Raster;
use crate::{credit, measure};

/// The levels whose rows differ from what `bin/index.sh` would write now --
/// a dataset added, removed, renumbered, relinked, rebuilt at another zoom or
/// over another extent.
fn lagging(levels: &Levels, rasters: &[Raster], g: &Grid) -> Vec<u32> {
    (g.coarsest_level..=g.finest_level)
        .rev()
        .filter(|&z| {
            let mut want = measure::rows(rasters, z);
            let mut have: Vec<&config::Row> = levels
                .iter()
                .find(|(lz, _)| *lz == z)
                .map(|(_, rows)| rows.iter().collect())
                .unwrap_or_default();

            want.sort_by(|a, b| a.id.cmp(b.id));
            have.sort_by(|a, b| a.dataset.cmp(&b.dataset));

            let same = |w: &measure::Planned, h: &&config::Row| {
                w.id == h.dataset
                    && w.priority == h.priority
                    && w.location == h.location
                    && w.bbox
                        .iter()
                        .zip(&h.bbox)
                        .all(|(a, b)| (a - b).abs() < 1e-6)
            };

            want.len() != have.len() || !want.iter().zip(&have).all(|(w, h)| same(w, h))
        })
        .collect()
}

pub fn run(index_dir: &Path, elevation_sources: &Path, g: Grid) -> Result<()> {
    let entries = credit::load_entries(elevation_sources)?;
    let (rasters, failed) = measure::rasters(&entries, g.coarsest_level);

    for r in &rasters {
        println!("ok   {:14} z{:<3} {}", r.id, r.base_level, r.path);
    }
    for (dir, why) in &failed {
        println!("FAIL {dir}: {why}");
    }

    let mut problems = failed.len();

    // Only over a complete measurement: with rasters missing, "no dataset" or
    // a credit complaint would point at the list rather than at the storage.
    if failed.is_empty() {
        let ids = rasters.iter().map(|r| r.id.as_str());
        for e in [
            measure::validate(&rasters).err(),
            credit::check_attributions(ids, &entries).err(),
        ]
        .into_iter()
        .flatten()
        {
            println!("FAIL {e:#}");
            problems += 1;
        }
    }

    // What `serve` refuses to start over, checked against what it would load.
    let levels = config::read_levels(index_dir, &g);
    let lag = match &levels {
        Ok(levels) => {
            let lag = lagging(levels, &rasters, &g);
            match config::from_levels(levels, g, 0) {
                Ok(doc) => {
                    let ids = doc.sources.iter().map(|s| s.id.as_str());
                    if let Err(e) = credit::check_attributions(ids, &entries) {
                        println!("FAIL index: {e:#}");
                        problems += 1;
                    }
                }
                Err(e) => {
                    println!("FAIL index: {e:#}");
                    problems += 1;
                }
            }
            lag
        }
        Err(e) => {
            println!("FAIL index: {e:#}");
            problems += 1;
            Vec::new()
        }
    };

    if failed.is_empty() && !lag.is_empty() {
        let levels: Vec<String> = lag.iter().map(|z| format!("z{z}")).collect();
        println!(
            "note the index lags the source list at {}: run `bin/index.sh`",
            levels.join(", ")
        );
    }

    if problems > 0 {
        bail!("\n{problems} problem(s); nothing above was changed");
    }

    println!("\n{} datasets can be indexed and served", rasters.len());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sk() -> Raster {
        Raster {
            id: "sk".into(),
            dir: "010-sk".into(),
            path: "/t/sk.tif".into(),
            priority: -10,
            base_level: 18,
            bbox: [16.8, 47.7, 22.6, 49.7],
        }
    }

    fn grid() -> Grid {
        Grid {
            finest_level: 14,
            coarsest_level: 8,
        }
    }

    /// The index exactly as `bin/index.sh` would write it for `rasters`.
    fn indexed(rasters: &[Raster]) -> Levels {
        (8..=14)
            .map(|z| {
                let rows = measure::rows(rasters, z)
                    .into_iter()
                    .map(|p| config::Row {
                        dataset: p.id.to_owned(),
                        priority: p.priority,
                        location: p.location,
                        bbox: p.bbox,
                    })
                    .collect();
                (z, rows)
            })
            .collect()
    }

    #[test]
    fn a_current_index_does_not_lag() {
        assert!(lagging(&indexed(&[sk()]), &[sk()], &grid()).is_empty());
    }

    #[test]
    fn a_renumbered_dataset_lags_on_every_level() {
        let mut now = sk();
        now.priority = -15;
        assert_eq!(lagging(&indexed(&[sk()]), &[now], &grid()).len(), 7);
    }

    /// Rebuilt in place at the same zoom: only the extent says so.
    #[test]
    fn a_raster_grown_in_place_lags() {
        let mut now = sk();
        now.bbox[2] = 23.0;
        assert_eq!(lagging(&indexed(&[sk()]), &[now], &grid()).len(), 7);
    }

    #[test]
    fn a_level_indexed_on_its_own_leaves_the_others_lagging() {
        let mut now = sk();
        now.priority = -15;
        let mut levels = indexed(&[sk()]);
        levels[6] = indexed(std::slice::from_ref(&now)).remove(6);

        assert_eq!(lagging(&levels, &[now], &grid()), [13, 12, 11, 10, 9, 8]);
    }

    #[test]
    fn a_missing_level_lags() {
        let mut levels = indexed(&[sk()]);
        levels.remove(0);
        assert_eq!(lagging(&levels, &[sk()], &grid()), [8]);
    }
}
