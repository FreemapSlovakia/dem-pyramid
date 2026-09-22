//! Is the cache still what the data says?
//!
//! `refresh` measures the sources and writes what the build needs; everything
//! downstream reads that rather than the rasters, which is what keeps a build
//! from opening a 44 MB VRT once per tile just to ask its resolution. The cost
//! is that the cache can fall behind: a source regenerated upstream, a dataset
//! added to the list, a raster whose nodata changed.
//!
//! So this re-measures and holds the answer against the cache. Headers only --
//! no pixel is read. It has already caught nodata sentinels that differ
//! between files of the same country.

use anyhow::{Result, bail};
use std::path::Path;

use crate::config::{Doc, Source};
use crate::{credit, refresh};

/// Everything a build would read, compared field by field.
fn differences(was: &Source, now: &Source) -> Vec<String> {
    let mut notes = Vec::new();

    let mut note = |what: &str, a: String, b: String| {
        if a != b {
            notes.push(format!("{what}: cached {a}, measured {b}"));
        }
    };

    // Both are path components for norm/ and footprints/, so a rename
    // silently repoints every tile the source has already built.
    note("id", was.id.clone(), now.id.clone());
    note("dir", was.dir.clone(), now.dir.clone());
    note("api_name", was.api_name.clone(), now.api_name.clone());
    note(
        "priority",
        was.priority.to_string(),
        now.priority.to_string(),
    );
    note("nodata", was.nodata.to_string(), now.nodata.to_string());
    note(
        "finest_level",
        was.finest_level.to_string(),
        now.finest_level.to_string(),
    );
    note("resampling", was.resampling.clone(), now.resampling.clone());
    note(
        "footprint",
        format!("{:?}", was.footprint),
        format!("{:?}", now.footprint),
    );
    note(
        "fill_nodata_md",
        format!("{:?}", was.fill_nodata_md),
        format!("{:?}", now.fill_nodata_md),
    );

    // Measured, so a hair of float drift between GDAL versions is not news.
    if (was.native_res - now.native_res).abs() > 0.05 * now.native_res {
        notes.push(format!(
            "native_res: cached {:.4} m, measured {:.4} m",
            was.native_res, now.native_res
        ));
    }

    // A box that moved by a fraction of a degree is the same box; anything
    // more changes which tiles the source is asked for.
    let moved = was
        .bbox
        .iter()
        .zip(&now.bbox)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0_f64, f64::max);

    if moved > 0.001 {
        notes.push(format!(
            "bbox: cached {:?}, measured {:?}",
            was.bbox, now.bbox
        ));
    }

    notes
}

pub fn run(doc: &Doc, elevation_sources: &Path) -> Result<()> {
    let entries = credit::load_entries(elevation_sources)?;

    // The licence rule first: it costs nothing and is the one failure here
    // that is not merely a stale number.
    credit::check_attributions(&doc.sources, &entries)?;

    let (measured, unreadable) =
        refresh::derive_all(&entries, doc.grid.coarsest_level, doc.grid.finest_level);

    let mut problems = unreadable;

    for now in &measured {
        match doc.sources.iter().find(|s| s.path == now.path) {
            None => {
                println!(
                    "FAIL {}: in the source list, missing from the cache",
                    now.id
                );
                problems += 1;
            }
            Some(was) => {
                let notes = differences(was, now);

                if notes.is_empty() {
                    println!("ok   {:14} {}", now.id, now.path);
                } else {
                    println!("FAIL {}", now.id);
                    for n in &notes {
                        println!("       {n}");
                    }
                    problems += notes.len();
                }
            }
        }
    }

    for was in &doc.sources {
        if !measured.iter().any(|m| m.path == was.path) {
            println!(
                "FAIL {}: in the cache, no longer built by the source list",
                was.id
            );
            problems += 1;
        }
    }

    if problems > 0 {
        bail!(
            "\n{problems} disagreement(s) with the data.\n\
             Run `dem-tool refresh` to bring the cache up to date, then rebuild \
             whatever the changed values affect."
        );
    }

    println!("\nthe cache agrees with all {} sources", measured.len());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FootprintMode, Nodata};

    fn source(id: &str, native_res: f64) -> Source {
        Source {
            id: id.into(),
            dir: format!("010-{id}"),
            api_name: id.into(),
            path: format!("/dtm/{id}.tif"),
            priority: 100,
            native_res,
            bbox: [0.0, 0.0, 1.0, 1.0],
            nodata: Nodata::Declared("declared".into()),
            finest_level: 14,
            resampling: "average".into(),
            footprint: FootprintMode::Bbox,
            fill_nodata_md: None,
        }
    }

    #[test]
    fn an_unchanged_source_reports_nothing() {
        assert!(differences(&source("sk", 1.0), &source("sk", 1.0)).is_empty());
    }

    /// Float noise between GDAL versions is not a change; a real resolution
    /// change is.
    #[test]
    fn resolution_tolerates_noise_but_not_a_change() {
        assert!(differences(&source("sk", 1.0), &source("sk", 1.0000001)).is_empty());
        assert_eq!(differences(&source("sk", 1.0), &source("sk", 2.0)).len(), 1);
    }

    #[test]
    fn a_moved_box_is_reported() {
        let was = source("sk", 1.0);
        let mut now = source("sk", 1.0);
        now.bbox = [0.0, 0.0, 1.5, 1.0];

        assert_eq!(differences(&was, &now).len(), 1);
    }

    /// Every field the build reads is compared, so a change in any one of them
    /// is caught rather than silently built against.
    #[test]
    fn each_build_input_is_compared() {
        let was = source("sk", 1.0);

        for mutate in [
            (|s: &mut Source| s.id = "other".into()) as fn(&mut Source),
            |s| s.dir = "020-other".into(),
            |s| s.api_name = "other".into(),
            |s| s.priority = 1,
            |s| s.nodata = Nodata::Value(-9999.0),
            |s| s.finest_level = 12,
            |s| s.resampling = "bilinear".into(),
            |s| s.footprint = FootprintMode::Tiles,
            |s| s.fill_nodata_md = Some(5),
        ] {
            let mut now = source("sk", 1.0);
            mutate(&mut now);
            assert_eq!(differences(&was, &now).len(), 1);
        }
    }
}
