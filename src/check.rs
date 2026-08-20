//! Re-measure every source and fail on any drift from sources.yaml.
//!
//! Headers only -- no pixel is read. This is the guard that keeps the config
//! honest as sources are added: it has already caught nodata sentinels that
//! differ between files of the same country.

use anyhow::{Result, bail};

use crate::config::Doc;
use crate::gdal_cli;

pub fn run(doc: &Doc) -> Result<()> {
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
                notes.push(format!(
                    "res {res_m:.4} m != yaml {}",
                    s.native_res
                ));
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
            (true, None) => notes.push(
                "yaml says nodata is declared but the dataset declares none".into(),
            ),
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
    println!("\nall {} sources agree with sources.yaml", doc.sources.len());
    Ok(())
}
