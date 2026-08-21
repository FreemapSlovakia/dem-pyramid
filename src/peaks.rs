//! Peak projection.
//!
//! Costs no extra rays. Each candidate is projected into (azimuth, elevation
//! angle) with the same curvature and refraction terms the marcher uses, and
//! its visibility is one lookup against the distance buffer: the peak is
//! visible when nothing nearer occupies that cell.
//!
//! Identity stays with OSM. The DTM knows whether a summit can be seen and
//! where it lands in frame; it does not know what anything is called, and
//! DTM-derived maxima cannot be matched back to named peaks reliably -- they
//! sit tens of metres from hand-placed nodes and a broad summit yields several.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;

use crate::gdal_cli;

#[derive(Debug, Serialize)]
pub struct Peak {
    pub osm_id: i64,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    /// The `ele` tag as OSM has it, unparsed. Unreliable -- prefer `ele`.
    pub ele_osm: Option<String>,
    pub lon: f64,
    pub lat: f64,

    /// Elevation from the DTM, which is what the geometry uses.
    pub ele: Option<f64>,
    pub distance: f64,
    pub azimuth: f64,
    pub altitude: f64,
    /// Position in the rendered image, in output pixels.
    pub x: f64,
    pub y: f64,
    pub visible: bool,
    /// How far the summit stands above the terrain beside it at its own depth,
    /// in metres, negative where its own ridge stands over it. Measured from
    /// what the render saw, so it is a lower bound where the cols are hidden.
    pub prominence: f64,

    /// Output column, used while rendering; not part of the API.
    #[serde(skip)]
    pub column: isize,
}

/// Load candidates within `range` metres of the viewpoint.
///
/// A generous lon/lat box first -- the spatial index makes that cheap -- then
/// exact great-circle distance.
pub fn load(path: &Path, lon: f64, lat: f64, range: f64) -> Result<Vec<Peak>> {
    let dlat = range / 111_320.0;
    let dlon = dlat / lat.to_radians().cos().max(0.05);

    let csv = gdal_cli::run(
        "ogr2ogr",
        &[
            "-f",
            "CSV",
            "/vsistdout/",
            "-lco",
            "GEOMETRY=AS_XY",
            "-spat",
            &format!("{}", lon - dlon),
            &format!("{}", lat - dlat),
            &format!("{}", lon + dlon),
            &format!("{}", lat + dlat),
            path.to_str().context("non-utf8 peaks path")?,
            "peaks",
        ],
    )?;

    let mut out = Vec::new();
    for line in csv.lines().skip(1) {
        let f = split_csv(line);
        if f.len() < 6 {
            continue;
        }
        let (Ok(px), Ok(py)) = (f[0].parse::<f64>(), f[1].parse::<f64>()) else {
            continue;
        };
        if crate::panorama::great_circle(lon, lat, px, py) > range {
            continue;
        }
        out.push(Peak {
            osm_id: f[2].parse().unwrap_or(0),
            name: f[3].clone(),
            kind: f[4].clone(),
            ele_osm: (!f[5].is_empty()).then(|| f[5].clone()),
            lon: px,
            lat: py,
            ele: None,
            distance: 0.0,
            azimuth: 0.0,
            altitude: 0.0,
            x: 0.0,
            y: 0.0,
            visible: false,
            prominence: 0.0,
            column: -1,
        });
    }
    Ok(out)
}

/// Minimal CSV split honouring double quotes, which peak names need -- plenty
/// contain commas.
fn split_csv(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}
