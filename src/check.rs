//! Re-measure every source and fail on any drift from sources.yaml.
//!
//! Two comparisons, both of them the same idea: sources.yaml declares, and
//! something else is asked whether it agrees. The rasters are asked with GDAL,
//! headers only -- no pixel is read -- which has already caught nodata
//! sentinels that differ between files of the same country. The elevation
//! API's source list is asked about the values the two share, which had drifted
//! apart over a month before anything looked.

use anyhow::{Result, bail};
use std::path::Path;

use crate::config::Doc;
use crate::{credit, gdal_cli};

pub fn run(doc: &Doc, elevation_sources: &Path) -> Result<()> {
    // The list first: it opens nothing large, so a config disagreement is
    // reported in a moment rather than after every raster header has been read.
    elevation_list(doc, elevation_sources)?;

    let mut problems = 0usize;

    for s in &doc.sources {
        let mut notes: Vec<String> = Vec::new();

        if !std::path::Path::new(&s.path).exists() {
            println!("FAIL {}: missing {}", s.id, s.path);
            problems += 1;
            continue;
        }

        let info = match gdal_cli::info_json(&s.path) {
            Ok(v) => v,
            Err(e) => {
                println!("FAIL {}: {e}", s.id);
                problems += 1;
                continue;
            }
        };

        // Resolution. GEDTM30 is in degrees; convert before comparing.
        if let Some(res) = info["geoTransform"][1].as_f64() {
            let res_m = if s.crs == "EPSG:4326" {
                res * 111_320.0
            } else {
                res.abs()
            };
            if (res_m - s.native_res).abs() > 0.05 * s.native_res {
                notes.push(format!("res {res_m:.4} m != yaml {}", s.native_res));
            }
        } else {
            notes.push("no geotransform".into());
        }

        if !gdal_cli::crs_matches(&s.path, &s.crs) {
            let name = info["coordinateSystem"]["wkt"]
                .as_str()
                .and_then(|w| w.split('"').nth(1))
                .unwrap_or("?")
                .to_owned();
            notes.push(format!("CRS is {name:?}, yaml says {}", s.crs));
        }

        let declared = info["bands"][0]["noDataValue"].as_f64();
        match (s.nodata.is_declared(), declared) {
            (true, None) => {
                notes.push("yaml says nodata is declared but the dataset declares none".into())
            }
            (false, Some(d)) => {
                let want = s.nodata.value().unwrap_or(f64::NAN);
                if (d - want).abs() > 1e-6 {
                    notes.push(format!(
                        "yaml overrides nodata to {want} while the dataset \
                         declares {d}"
                    ));
                }
            }
            _ => {}
        }

        if notes.is_empty() {
            let size = format!(
                "{}x{}",
                info["size"][0].as_i64().unwrap_or(0),
                info["size"][1].as_i64().unwrap_or(0)
            );
            let nd = declared.map_or_else(
                || "none".to_owned(),
                |v| {
                    // Float-max sentinels (sk, gedtm30) are unreadable in
                    // decimal.
                    if v.abs() >= 1e6 {
                        format!("{v:e}")
                    } else {
                        v.to_string()
                    }
                },
            );
            println!("ok   {:14} {size} nodata={nd}", s.id);
        } else {
            problems += 1;
            println!("FAIL {}", s.id);
            for n in &notes {
                println!("       {n}");
            }
        }
    }

    if problems > 0 {
        bail!("\n{problems} source(s) disagree with sources.yaml");
    }
    println!(
        "\nall {} sources agree with sources.yaml",
        doc.sources.len()
    );

    Ok(())
}

/// Compare sources.yaml against the elevation API's own list, which lives in
/// its own repository and is a checkout on the serving host.
///
/// Only the values both consumers hold: the file each source reads, the model
/// it is reported under, and the order they are tried in. Everything else in
/// sources.yaml is how the pyramid is built, which is no concern of the API's.
fn elevation_list(doc: &Doc, dir: &Path) -> Result<()> {
    let entries = credit::load_entries(dir)?;
    let mut problems = 0usize;

    println!("against the elevation source list at {}", dir.display());

    // By file rather than by name: a name is a model and several datasets share
    // one, but the file is the dataset.
    let mut placed = Vec::new();

    for source in &doc.sources {
        let Some(entry) = entries.iter().find(|e| e.file == source.path) else {
            println!("FAIL {}: {} is in no source.json", source.id, source.path);
            problems += 1;
            continue;
        };

        if entry.name != source.api_name {
            println!(
                "FAIL {}: api_name {} here, {} in {}",
                source.id, source.api_name, entry.name, entry.dir
            );
            problems += 1;
            continue;
        }

        placed.push((source.id.as_str(), entry.dir.as_str(), source.priority));
    }

    // Both lists are precedence orders written in opposite directions:
    // priority descending here, directory ascending there. Only the pyramid's
    // own sources are compared -- the list is a superset, and the ones it
    // holds that the pyramid does not build cannot reorder anything here.
    let mut by_priority = placed.clone();
    by_priority.sort_by_key(|&(_, _, priority)| -priority);

    let mut by_dir = placed.clone();
    by_dir.sort_by_key(|&(_, dir, _)| dir);

    for (a, b) in by_priority.iter().zip(&by_dir) {
        if a.0 != b.0 {
            println!(
                "FAIL {}: sits where {} does in the elevation list ({} vs {})",
                a.0, b.0, a.1, b.1
            );
            problems += 1;
        }
    }

    if problems > 0 {
        bail!(
            "\n{problems} disagreement(s) with the elevation source list.\n\
             Change whichever side is stale: sources.yaml here, or the entry in\n\
             github.com/FreemapSlovakia/elevation-sources."
        );
    }

    // The one rule that is a licence requirement rather than a consistency
    // one, so it is worth saying it passed.
    credit::check_attributions(doc, &entries)?;

    println!(
        "ok   all {} sources agree with the elevation list, and each has a credit",
        placed.len()
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FootprintMode, Grid, Nodata, Source};

    fn source(id: &str, api_name: &str, priority: i64, path: &str) -> Source {
        Source {
            id: id.into(),
            api_name: api_name.into(),
            path: path.into(),
            priority,
            native_res: 1.0,
            crs: "EPSG:4326".into(),
            bbox: [0.0, 0.0, 1.0, 1.0],
            nodata: Nodata::Declared("declared".into()),
            finest_level: 14,
            resampling: "average".into(),
            footprint: FootprintMode::Bbox,
            fill_nodata_md: None,
            rebuild_vrt: false,
        }
    }

    fn doc(mut sources: Vec<Source>) -> Doc {
        sources.sort_by_key(|s| -s.priority);

        Doc {
            grid: Grid {
                crs: "EPSG:3857".into(),
                tile_px: 512,
                block_px: 512,
                finest_level: 14,
                coarsest_level: 8,
            },
            sources,
        }
    }

    /// Writes a tree shaped like the real one: a directory per dataset, named
    /// so the prefix orders them.
    fn tree(dir: &Path, entries: &[(&str, &str, &str)]) {
        for (sub, name, file) in entries {
            let d = dir.join(sub);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("source.json"),
                format!(r#"{{"name":"{name}","file":"{file}","attributions":[{{"name":"c"}}]}}"#),
            )
            .unwrap();
        }
    }

    fn tmp(label: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dem-check-{label}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn an_agreeing_list_passes() {
        let d = tmp("agree");
        tree(
            &d,
            &[
                ("010-sk", "sk", "/dtm/sk.tif"),
                ("999-g", "g", "/dtm/g.tif"),
            ],
        );

        let doc = doc(vec![
            source("sk", "sk", 230, "/dtm/sk.tif"),
            source("g", "g", 0, "/dtm/g.tif"),
        ]);

        assert!(elevation_list(&doc, &d).is_ok());
        std::fs::remove_dir_all(&d).ok();
    }

    /// The lu drift: sources.yaml read a VRT wrapper while the list read the
    /// raster underneath it.
    #[test]
    fn a_file_the_list_does_not_name_is_caught() {
        let d = tmp("file");
        tree(&d, &[("010-lu", "lu", "/dtm/lu/luxembourg.tif")]);

        let doc = doc(vec![source("lu", "lu", 167, "/dtm/lu/lu.vrt")]);

        assert!(elevation_list(&doc, &d).is_err());
        std::fs::remove_dir_all(&d).ok();
    }

    /// The be/it/en drift: same datasets, opposite precedence.
    #[test]
    fn a_reordered_list_is_caught() {
        let d = tmp("order");
        tree(
            &d,
            &[
                ("210-it", "it", "/dtm/it.tif"),
                ("240-be", "be", "/dtm/be.vrt"),
            ],
        );

        // Here be outranks it; in the list it outranks be.
        let doc = doc(vec![
            source("be", "be", 168, "/dtm/be.vrt"),
            source("it", "it", 165, "/dtm/it.tif"),
        ]);

        assert!(elevation_list(&doc, &d).is_err());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_mismatched_api_name_is_caught() {
        let d = tmp("name");
        tree(&d, &[("010-sk", "slovakia", "/dtm/sk.tif")]);

        let doc = doc(vec![source("sk", "sk", 230, "/dtm/sk.tif")]);

        assert!(elevation_list(&doc, &d).is_err());
        std::fs::remove_dir_all(&d).ok();
    }

    /// The list is a superset: entries the pyramid does not build are not a
    /// disagreement, and cannot reorder the ones it does.
    #[test]
    fn entries_the_pyramid_does_not_build_are_ignored() {
        let d = tmp("superset");
        tree(
            &d,
            &[
                ("010-sk", "sk", "/dtm/sk.tif"),
                ("500-sonny-de", "sonny", "/dtm/sonny/de.tif"),
                ("999-g", "g", "/dtm/g.tif"),
            ],
        );

        let doc = doc(vec![
            source("sk", "sk", 230, "/dtm/sk.tif"),
            source("g", "g", 0, "/dtm/g.tif"),
        ]);

        assert!(elevation_list(&doc, &d).is_ok());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_source_with_no_credit_is_caught() {
        let d = tmp("credit");
        std::fs::create_dir_all(d.join("010-sk")).unwrap();
        std::fs::write(
            d.join("010-sk/source.json"),
            r#"{"name":"sk","file":"/dtm/sk.tif","attributions":[]}"#,
        )
        .unwrap();

        let doc = doc(vec![source("sk", "sk", 230, "/dtm/sk.tif")]);

        assert!(elevation_list(&doc, &d).is_err());
        std::fs::remove_dir_all(&d).ok();
    }
}
