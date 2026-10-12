//! Is the cache still what the data says?
//!
//! `refresh` measures the rasters and writes what the index needs; everything
//! downstream reads that rather than the rasters. The cost is that the cache
//! can fall behind: a dataset added, renumbered or renamed in the elevation
//! source list, or its raster rebuilt at another zoom.
//!
//! So this re-measures and holds the answer against the cache. Headers only --
//! no pixel is read.

use anyhow::{Result, bail};
use std::path::Path;

use crate::config::{Doc, Source};
use crate::{credit, refresh};

/// Everything the index or a credit would read, compared field by field.
fn differences(was: &Source, now: &Source) -> Vec<String> {
    let mut notes = Vec::new();

    let mut note = |what: &str, a: String, b: String| {
        if a != b {
            notes.push(format!("{what}: cached {a}, measured {b}"));
        }
    };

    note("dir", was.dir.clone(), now.dir.clone());
    note("path", was.path.clone(), now.path.clone());
    note(
        "priority",
        was.priority.to_string(),
        now.priority.to_string(),
    );
    note(
        "base_level",
        was.base_level.to_string(),
        now.base_level.to_string(),
    );

    // A box that moved by a fraction of a degree is the same box; anything
    // more changes which renders credit the source.
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

    let (measured, unreadable) = refresh::derive_all(&entries, doc.grid.coarsest_level);

    let mut problems = unreadable.len();

    for now in &measured {
        match doc.sources.iter().find(|s| s.id == now.id) {
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
        // One already reported unreadable is not also gone from the list.
        if !measured.iter().any(|m| m.id == was.id) && !unreadable.contains(&was.id) {
            println!(
                "FAIL {}: in the cache, no longer in the source list",
                was.id
            );
            problems += 1;
        }
    }

    // After the report, not before it: a dataset renamed upstream fails the
    // licence rule too, and stopping on that first would answer a stale cache
    // with a complaint about credits and hide every other drift behind it.
    credit::check_attributions(&doc.sources, &entries)?;

    if problems > 0 {
        bail!(
            "\n{problems} disagreement(s) with the data.\n\
             Run `dem-tool refresh` to bring the cache up to date, then \
             `bin/index.sh` and restart terrain."
        );
    }

    println!("\nthe cache agrees with all {} sources", measured.len());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: &str) -> Source {
        Source {
            id: id.into(),
            dir: format!("010-{id}"),
            path: format!("/terrain/{id}.tif"),
            priority: -1,
            base_level: 18,
            bbox: [0.0, 0.0, 1.0, 1.0],
        }
    }

    #[test]
    fn an_unchanged_source_reports_nothing() {
        assert!(differences(&source("sk"), &source("sk")).is_empty());
    }

    #[test]
    fn a_moved_box_is_reported_and_noise_is_not() {
        let mut now = source("sk");
        now.bbox = [0.0, 0.0, 1.0001, 1.0];
        assert!(differences(&source("sk"), &now).is_empty());

        now.bbox = [0.0, 0.0, 1.5, 1.0];
        assert_eq!(differences(&source("sk"), &now).len(), 1);
    }

    /// Every field the index or a credit reads is compared, so a change in any
    /// one of them is caught rather than silently served.
    #[test]
    fn each_input_is_compared() {
        let was = source("sk");

        for mutate in [
            (|s: &mut Source| s.dir = "020-other".into()) as fn(&mut Source),
            |s| s.path = "/elsewhere/sk.tif".into(),
            |s| s.priority = -2,
            |s| s.base_level = 17,
        ] {
            let mut now = source("sk");
            mutate(&mut now);
            assert_eq!(differences(&was, &now).len(), 1);
        }
    }
}
