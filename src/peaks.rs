//! Peak projection.
//!
//! Costs no extra rays. Each candidate is projected into (azimuth, elevation
//! angle) with the same curvature and refraction terms the marcher uses, and
//! its visibility is answered by the marcher itself: the two rays bracketing
//! its bearing report the horizon they had reached by its distance, and the
//! summit is visible when it stands above that.
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
    /// Set where `depth_lift` is what put the summit in view: it is drawn,
    /// and labelled, but the eye could not see it from here.
    ///
    /// Separate from `visible` rather than folded into it because the two
    /// answer different questions and clients want both -- the label has to be
    /// placed either way, and only this says whether to mark it as inferred.
    pub revealed: bool,
    /// How far the summit stands above the terrain beside it at its own depth,
    /// in metres, negative where its own ridge stands over it. Measured from
    /// every elevation the marcher sampled, so hidden ground still counts;
    /// what it cannot see is between the bearings it cast rays along.
    ///
    /// Not called prominence, because topographic prominence is non-negative
    /// by definition and this is not: where it is positive the two agree
    /// closely, but a name that promised the textbook measure would invite
    /// comparison against published figures for tops that score below zero.
    pub dominance: f64,

    /// Sub-column the peak's bearing falls in -- a ray index, not an output
    /// column. `None` when it is out of frame or has no elevation, which is
    /// the same thing as "no ray will answer it". Not part of the API.
    #[serde(skip)]
    pub column: Option<usize>,
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
            revealed: false,
            dominance: 0.0,
            column: None,
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

/// Which summits earn a label, most dominant first.
///
/// In frame, not hidden, and standing far enough above what is around it.
/// Visibility was decided during the render, from the columns it marched
/// anyway; `column` is `Some` exactly when a ray answered the peak.
///
/// One policy, in one place. Both front-ends want the same answer, and while
/// they each spelled it out the two copies drifted -- a cast here, a clause
/// there -- and every change to the rule, including the dominance rename, was
/// two edits with nothing to catch the one you forgot.
pub fn select(peaks: &mut Vec<Peak>, min_dominance: f64, max_peaks: usize, height: usize) {
    peaks.retain(|k| {
        k.visible
            && k.column.is_some()
            && k.dominance >= min_dominance
            && k.y >= 0.0
            && k.y <= height as f64
    });
    peaks.sort_by(|a, b| b.dominance.partial_cmp(&a.dominance).unwrap());
    // After the sort, so a cap keeps the summits that dominate the view rather
    // than an arbitrary slice. Zero is no cap.
    if max_peaks > 0 {
        peaks.truncate(max_peaks);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peak(osm_id: i64, dominance: f64, visible: bool, column: Option<usize>, y: f64) -> Peak {
        Peak {
            osm_id,
            name: format!("peak {osm_id}"),
            kind: "peak".into(),
            ele_osm: None,
            lon: 20.0,
            lat: 49.0,
            ele: Some(1000.0),
            distance: 10_000.0,
            azimuth: 0.0,
            altitude: 0.0,
            x: 0.0,
            y,
            visible,
            revealed: false,
            dominance,
            column,
        }
    }

    #[test]
    fn select_keeps_only_summits_worth_a_label() {
        let mut peaks = vec![
            peak(1, 100.0, true, Some(0), 10.0),   // kept
            peak(2, 100.0, false, Some(0), 10.0),  // hidden
            peak(3, 100.0, true, None, 10.0),      // no ray answered it
            peak(4, 10.0, true, Some(0), 10.0),    // below the threshold
            peak(5, 100.0, true, Some(0), -1.0),   // above the frame
            peak(6, 100.0, true, Some(0), 601.0),  // below the frame
        ];
        select(&mut peaks, 30.0, 0, 600);
        assert_eq!(peaks.iter().map(|p| p.osm_id).collect::<Vec<_>>(), [1]);
    }

    /// Negative dominance is ordinary -- most tops in ridge country score
    /// below zero -- so a negative threshold has to work, and ordering has to
    /// hold across the sign.
    #[test]
    fn select_orders_across_the_sign() {
        let mut peaks = vec![
            peak(1, -50.0, true, Some(0), 10.0),
            peak(2, 200.0, true, Some(0), 10.0),
            peak(3, -5.0, true, Some(0), 10.0),
            peak(4, 20.0, true, Some(0), 10.0),
        ];
        select(&mut peaks, -100.0, 0, 600);
        assert_eq!(
            peaks.iter().map(|p| p.osm_id).collect::<Vec<_>>(),
            [2, 4, 3, 1]
        );
    }

    /// The cap applies after the sort, so it keeps the summits that dominate
    /// the view rather than whichever happened to be loaded first.
    #[test]
    fn the_cap_keeps_the_most_dominant() {
        let mut peaks = vec![
            peak(1, 10.0, true, Some(0), 10.0),
            peak(2, 900.0, true, Some(0), 10.0),
            peak(3, 500.0, true, Some(0), 10.0),
        ];
        select(&mut peaks, -1000.0, 2, 600);
        assert_eq!(peaks.iter().map(|p| p.osm_id).collect::<Vec<_>>(), [2, 3]);
    }

    #[test]
    fn zero_means_no_cap() {
        let mut peaks = (0..5).map(|i| peak(i, 100.0, true, Some(0), 10.0)).collect();
        select(&mut peaks, 0.0, 0, 600);
        assert_eq!(peaks.len(), 5);
    }
}
