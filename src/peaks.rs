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

/// Rounding applied on the way out, and only on the way out.
///
/// `serde_json` writes the shortest text that round-trips an `f64` exactly,
/// which for a computed value means every digit it has:
/// `"distance": 116483.52984247255`. Seventeen significant figures describing
/// a summit whose position is known to a few metres, repeated over thousands
/// of peaks, is most of the payload -- 344 bytes each, and a 2631-peak view
/// runs to 904 KB against a 398 KB image.
///
/// It also defeats compression. Those low digits are as close to random as
/// anything in the response, so gzip cannot pack them: the same peaks
/// compress to 299 KB as sent and to **153 KB** rounded. Cutting the digits
/// is worth more after compression than before it, which is why this is not
/// made redundant by turning gzip on.
///
/// Every precision here is far below what the renderer or the DEM can
/// resolve: 6 decimals of latitude is 11 cm, `x`/`y` to 0.01 output pixels,
/// angles to 0.001 degrees -- a fiftieth of a pixel at the default `step`.
/// Nothing observable changes; the numbers stay full precision everywhere
/// inside the program.
mod round {
    use serde::Serializer;

    fn to(v: f64, decimals: i32) -> f64 {
        if !v.is_finite() {
            return v;
        }
        let m = 10f64.powi(decimals);
        (v * m).round() / m
    }

    macro_rules! at {
        ($name:ident, $decimals:expr) => {
            pub fn $name<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_f64(to(*v, $decimals))
            }
        };
    }

    at!(d1, 1);
    at!(d2, 2);
    at!(d3, 3);
    at!(d6, 6);

    /// `ele` is optional, and `None` has to stay `null` rather than become 0.
    pub fn opt_d1<S: Serializer>(v: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(v) => s.serialize_f64(to(*v, 1)),
            None => s.serialize_none(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Peak {
    pub osm_id: i64,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    /// The `ele` tag as OSM has it, unparsed. Unreliable -- prefer `ele`.
    pub ele_osm: Option<String>,
    #[serde(serialize_with = "round::d6")]
    pub lon: f64,
    #[serde(serialize_with = "round::d6")]
    pub lat: f64,

    /// Elevation from the DTM, which is what the geometry uses.
    #[serde(serialize_with = "round::opt_d1")]
    pub ele: Option<f64>,
    #[serde(serialize_with = "round::d1")]
    pub distance: f64,
    #[serde(serialize_with = "round::d3")]
    pub azimuth: f64,
    #[serde(serialize_with = "round::d3")]
    pub altitude: f64,
    /// Position in the rendered image, in output pixels.
    #[serde(serialize_with = "round::d2")]
    pub x: f64,
    #[serde(serialize_with = "round::d2")]
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
    #[serde(serialize_with = "round::d1")]
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

/// Distance exponent the label cut uses unless asked otherwise.
///
/// Gentle: it keeps the great distant massifs, which people do want named,
/// while letting near subordinate tops through. The client re-ranks whatever
/// it receives, so this only has to make the *cut* land in a sensible place.
pub const DEFAULT_RANK_POWER: f64 = 0.5;

/// Largest exponent accepted. Well past useful -- at 1 the score is already
/// dominance per metre of distance -- and a bound at all because the value is
/// an exponent applied to distances up to 400 km.
pub const MAX_RANK_POWER: f64 = 4.0;

/// How the label cut is made.
///
/// A struct rather than five positional arguments, two of which are `f64` and
/// one a bare `bool`: transposing any pair compiles and changes which summits
/// come back.
pub struct Selection {
    /// Drop summits scoring below this, metres, signed.
    pub min_dominance: f64,
    /// Keep at most this many, highest [`label_rank`] first. 0 is no cap.
    pub max_peaks: usize,
    /// Frame height in output pixels; peaks outside it are not in the picture.
    pub height: usize,
    /// Whether summits only `depth_lift` brought into view may take slots.
    pub keep_revealed: bool,
    /// Exponent on distance in [`label_rank`]. 0 ranks on dominance alone.
    pub rank_power: f64,
}

/// How label-worthy a summit is: dominance, discounted by distance.
///
/// Dominance is in metres and metres are not a rank. A 400 m top sixty
/// kilometres out scores far above a 90 m one two kilometres away, while the
/// near one fills more of the frame and is what a viewer is actually looking
/// at. Dividing by a power of distance restores that; 0.5 is gentle enough to
/// keep the great distant massifs, which people do want named.
///
/// **The sign is a trap, and getting it backwards is worse than not weighting
/// at all.** Dominance is signed -- in ridge country most visible tops score
/// below zero -- and dividing a negative by a larger number *raises* it, so
/// the same expression that penalises a distant peak rewards a distant one as
/// soon as its dominance goes negative. Distance has to push the score down
/// on both sides of zero, which means dividing where it is positive and
/// multiplying where it is negative. Measured on a 2631-peak Ötztal view,
/// dividing throughout moved a near subordinate top from 2471st to 2627th --
/// the opposite of the intent -- where the rule below moves it to 1966th.
///
/// Continuous through zero: both branches give 0 at 0.
pub fn label_rank(dominance: f64, distance: f64, power: f64) -> f64 {
    // Guards a viewpoint sitting on the summit itself, where the scale would
    // otherwise be zero and divide a positive dominance to infinity.
    let scale = distance.max(1.0).powf(power);
    if dominance >= 0.0 {
        dominance / scale
    } else {
        dominance * scale
    }
}

/// Which summits earn a label, most label-worthy first.
///
/// In frame, not hidden, and standing far enough above what is around it.
/// Visibility was decided during the render, from the columns it marched
/// anyway; `column` is `Some` exactly when a ray answered the peak.
///
/// Ordered by [`label_rank`], not by dominance: see there for why metres are
/// not a rank, and for the sign trap in fixing that.
///
/// One policy, in one place. Both front-ends want the same answer, and while
/// they each spelled it out the two copies drifted -- a cast here, a clause
/// there -- and every change to the rule, including the dominance rename, was
/// two edits with nothing to catch the one you forgot.
///
/// `keep_revealed` decides whether summits that only `depth_lift` brought into
/// view may take label slots. It has to be settled here rather than left to
/// the caller, because `max_peaks` truncates: filter afterwards and a request
/// for twenty labels returns however many of its twenty happened to be real,
/// with the near summits it dropped unrecoverable. Revealed peaks are distant
/// by construction, being the ones that were behind something.
pub fn select(peaks: &mut Vec<Peak>, s: &Selection) {
    peaks.retain(|k| {
        k.visible
            && (s.keep_revealed || !k.revealed)
            && k.column.is_some()
            && k.dominance >= s.min_dominance
            && k.y >= 0.0
            && k.y <= s.height as f64
    });
    // `min_dominance` still filters on raw dominance, deliberately: it is a
    // statement about the landscape -- "nothing flatter than this is a summit"
    // -- and would stop meaning anything in metres if distance entered it.
    // Only the cut, which is about label density in a picture, is ranked.
    // `total_cmp`, not `partial_cmp().unwrap()`. Nothing reachable produces a
    // NaN rank today -- both front-ends check `rank_power`, and dominance is
    // guarded finite where it is computed -- but `label_rank` and `Selection`
    // are public with public fields, so that guarantee lives entirely in
    // validators in other files. This costs the same and cannot panic.
    peaks.sort_by(|a, b| {
        let rank = |k: &Peak| label_rank(k.dominance, k.distance, s.rank_power);
        rank(b).total_cmp(&rank(a))
    });
    // After the sort, so a cap keeps the summits worth labelling rather than
    // an arbitrary slice. Zero is no cap.
    if s.max_peaks > 0 {
        peaks.truncate(s.max_peaks);
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

    /// The frame and the revealed policy are fixed for most of these; only
    /// the threshold and the cap vary.
    fn sel(min_dominance: f64, max_peaks: usize) -> Selection {
        Selection {
            min_dominance,
            max_peaks,
            height: 600,
            keep_revealed: true,
            rank_power: DEFAULT_RANK_POWER,
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
        select(&mut peaks, &sel(30.0, 0));
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
        select(&mut peaks, &sel(-100.0, 0));
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
        select(&mut peaks, &sel(-1000.0, 2));
        assert_eq!(peaks.iter().map(|p| p.osm_id).collect::<Vec<_>>(), [2, 3]);
    }

    #[test]
    fn zero_means_no_cap() {
        let mut peaks = (0..5).map(|i| peak(i, 100.0, true, Some(0), 10.0)).collect();
        select(&mut peaks, &sel(0.0, 0));
        assert_eq!(peaks.len(), 5);
    }

    /// The reason `keep_revealed` is decided inside `select` rather than left
    /// to the caller: dominance is in metres, so the distant summits a lift
    /// brings out outrank near ones, and the cap runs after the sort. Filter
    /// afterwards and a request for two labels returns one.
    #[test]
    fn revealed_summits_do_not_take_slots_from_visible_ones() {
        let revealed = |id, dom| {
            let mut k = peak(id, dom, true, Some(0), 10.0);
            k.revealed = true;
            k
        };
        let candidates = || {
            vec![
                revealed(1, 900.0),
                revealed(2, 800.0),
                peak(3, 400.0, true, Some(0), 10.0),
                peak(4, 300.0, true, Some(0), 10.0),
            ]
        };

        // Kept: the lift was asked for, so what it brought out gets labelled.
        let mut all = candidates();
        select(&mut all, &sel(0.0, 2));
        assert_eq!(all.iter().map(|p| p.osm_id).collect::<Vec<_>>(), [1, 2]);

        // Refused: the cap now spends both slots on summits actually in sight,
        // rather than returning two labels of which none are.
        let mut real = candidates();
        select(
            &mut real,
            &Selection {
                keep_revealed: false,
                ..sel(0.0, 2)
            },
        );
        assert_eq!(real.iter().map(|p| p.osm_id).collect::<Vec<_>>(), [3, 4]);
    }

    /// The sign rule, pinned with the numbers that exposed it.
    ///
    /// Both of these are real summits from a 2631-peak Ötztal view: a near
    /// subordinate top the client wants labelled, and the peak that sat on
    /// the `max_peaks: 2000` boundary. Dividing throughout -- the obvious
    /// reading of "discount by distance" -- ranks the near one *below* the
    /// boundary, which is how the bug reached production in the first place.
    #[test]
    fn distance_pushes_a_score_down_on_both_sides_of_zero() {
        let (near, far) = ((-259.9, 2117.7), (-60.4, 45_000.0));

        // The rule: a distant negative is worse than a near one of the same
        // depth, and worse than a shallower near one that is far enough out.
        assert!(label_rank(near.0, near.1, 0.5) > label_rank(far.0, far.1, 0.5));
        // Naive division inverts exactly that, which is the trap.
        assert!(near.0 / near.1.sqrt() < far.0 / far.1.sqrt());

        // Positives are discounted too, or a distant massif outranks
        // everything near it.
        assert!(label_rank(100.0, 2_000.0, 0.5) > label_rank(400.0, 60_000.0, 0.5));

        // Monotone in distance on both sides, for any exponent.
        for power in [0.25, 0.5, 1.0, MAX_RANK_POWER] {
            for dominance in [-500.0, -1.0, 0.0, 1.0, 500.0] {
                let (near, far) = (
                    label_rank(dominance, 1_000.0, power),
                    label_rank(dominance, 100_000.0, power),
                );
                assert!(near >= far, "{dominance} at power {power}: {near} < {far}");
            }
        }

        // Zero exponent is the old behaviour exactly, both signs.
        for dominance in [-500.0, 0.0, 500.0] {
            assert_eq!(label_rank(dominance, 12_345.0, 0.0), dominance);
        }
    }
}
