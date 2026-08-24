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

    /// Significant digits, not decimal places, for `rank`.
    ///
    /// Its scale is entirely the caller's: `dominance / distance^2` produces
    /// about 3.7e-7, and at three decimals every peak in the response
    /// serializes as `0.0` while the ordering behind it is perfectly real --
    /// which defeats the only reason the field exists. Six significant digits
    /// hold both that and the -11 960 a near subordinate top scores under the
    /// default formula.
    pub fn opt_sig<S: Serializer>(v: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(v) => s.serialize_f64(sig(*v, 6)),
            None => s.serialize_none(),
        }
    }

    fn sig(v: f64, digits: i32) -> f64 {
        if !v.is_finite() || v == 0.0 {
            return v;
        }
        let mag = v.abs().log10().floor();
        // Beyond this the scaling itself overflows, and a value that extreme
        // is already past anything a ranking distinguishes.
        let shift = digits - 1 - mag as i32;
        if !(-300..=300).contains(&shift) {
            return v;
        }
        let f = 10f64.powi(shift);
        (v * f).round() / f
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

    /// The value this peak was ordered by, so the ordering is inspectable.
    ///
    /// Whatever `peak_rank` computed, or the built-in criterion where none was
    /// given. Returned because an ordering you cannot see is an ordering you
    /// cannot debug: send a formula, read back what it produced for each peak,
    /// and the reason a summit sits where it does stops being a guess. A
    /// client that re-ranks can also reuse the number instead of recomputing
    /// it.
    ///
    /// **`null` means the formula produced nothing usable for this peak** --
    /// a null, a NaN or an infinity -- and it was sorted last. With
    /// `peak_rank: ["get", "prominence"]` that is two thirds of them, since
    /// most have no prominence. It is not "unranked": every returned peak was
    /// ranked, and this one scored bottom.
    #[serde(serialize_with = "round::opt_sig")]
    pub rank: Option<f64>,

    /// True topographic prominence, metres, or `null` where none is known.
    ///
    /// Precomputed from GEDTM30 over the whole continent and stored with the
    /// peak, so unlike `dominance` it is the same from every viewpoint. That
    /// is the point of it: measured across two Slovak viewpoints, 20% of the
    /// summits visible from both flipped the sign of their dominance and 40%
    /// of those earning a label from one were dropped by the other. Prominence
    /// does not move.
    ///
    /// It answers a different question, though, and cannot replace dominance:
    /// this says whether something is a mountain, dominance says whether it
    /// stands out from where you are standing. A summit seen end-on along its
    /// own ridge really is unremarkable from there, and saying so is correct.
    #[serde(serialize_with = "round::opt_d1")]
    pub prominence: Option<f64>,
    /// How far the DEM summit this came from sat from the OSM node, metres.
    ///
    /// Every match is a guess of some size, because 30 m data places a summit
    /// up to ~75 m from where the LiDAR and OSM agree it is. 42% land within
    /// 25 m and 74% within 50 m; treat anything past 100 m as suspect. Where
    /// two real summits sit inside the radius only the nearest keeps a value
    /// and the others come back `null`, so a missing prominence never means
    /// "not a mountain" -- only that nothing could be matched to it.
    #[serde(serialize_with = "round::opt_d1")]
    pub prom_dist_m: Option<f64>,

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
            // Absent rather than zero where the column does not exist: a
            // peaks file predating `bin/prominence-join.sh` has six fields,
            // not eight, and "no prominence known" is not "prominence 0".
            prominence: f.get(6).and_then(|s| s.parse().ok()),
            prom_dist_m: f.get(7).and_then(|s| s.parse().ok()),
            rank: None,
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

/// What the label cut orders by when the caller gives no formula.
///
/// It is the documented default expression, and it is the *only*
/// implementation of it. There used to be a second -- a Rust `label_rank`
/// with an exponent parameter -- and a test kept the two in step. The
/// documentation's promise that "sending this formula changes nothing" was
/// then true because a test said so; now it is true because there is nothing
/// else for it to differ from.
///
/// The shape it encodes, and why it is not simply `dominance`:
///
/// Dominance is in metres and metres are not a rank. A 400 m top sixty
/// kilometres out scores far above a 90 m one two kilometres away, while the
/// near one fills more of the frame and is what a viewer is actually looking
/// at. Dividing by the square root of distance restores that, gently enough
/// to keep the great distant massifs people do want named.
///
/// **The sign is a trap, and getting it backwards is worse than not weighting
/// at all.** Dominance is signed -- in ridge country most visible tops score
/// below zero -- and dividing a negative by a larger number *raises* it, so
/// the same expression that penalises a distant peak rewards a distant one as
/// soon as its dominance goes negative. Distance has to push the score down
/// on both sides of zero, which is what folding `sign` into the exponent
/// does: positive dominance divides by `distance^0.5`, negative multiplies,
/// because dividing by `distance^-0.5` is multiplying. Measured on a
/// 2631-peak Ötztal view, dividing throughout moved a near subordinate top
/// from 2471st to 2627th -- the opposite of the intent -- where this rule
/// moves it to 1966th.
///
/// `max(distance, 1)` guards a viewpoint standing on the summit it is
/// ranking, where `distance^0.5` would be zero and the division infinite.
/// Continuous through zero: `sign` is 0 there, `distance^0` is 1, 0/1 is 0.
fn default_rank() -> &'static crate::rank::Program {
    static P: std::sync::OnceLock<crate::rank::Program> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        crate::rank::Program::compile(&serde_json::json!([
            "/",
            ["get", "dominance"],
            ["^", ["max", ["get", "distance"], 1],
                  ["*", 0.5, ["sign", ["get", "dominance"]]]]
        ]))
        .expect("the built-in ranking formula must compile")
    })
}

/// How the label cut is made.
///
/// A struct rather than five positional arguments, two of which are `f64` and
/// one a bare `bool`: transposing any pair compiles and changes which summits
/// come back.
pub struct Selection {
    /// Drop summits scoring below this, metres, signed.
    pub min_dominance: f64,
    /// Keep at most this many, highest-ranked first. 0 is no cap.
    pub max_peaks: usize,
    /// Frame height in output pixels; peaks outside it are not in the picture.
    pub height: usize,
    /// Whether summits only `depth_lift` brought into view may take slots.
    pub keep_revealed: bool,
    /// A caller-supplied formula, used instead of [`default_rank`].
    ///
    /// Here rather than in the client because `max_peaks` truncates: a cut
    /// made on our criterion throws away exactly what a client ranking on its
    /// own criterion would have kept, and no amount of re-ranking afterwards
    /// gets it back.
    pub rank: Option<crate::rank::Program>,
    /// Which peaks survive at all, as an expression over the same properties.
    ///
    /// It supersedes `min_dominance` and `keep_revealed`, which can only ever
    /// see one field each. Those still work and are applied alongside it --
    /// existing clients depend on them -- but everything they express is
    /// expressible here, and more: "a real mountain however it reads from
    /// here, or anything standing out locally" needs two properties and a
    /// disjunction, which no numeric threshold can say.
    pub filter: Option<crate::rank::Program>,
}

/// The properties an expression sees for one peak.
fn vars(k: &Peak) -> crate::rank::Vars {
    crate::rank::Vars {
        dominance: k.dominance,
        distance: k.distance,
        altitude: k.altitude,
        ele: k.ele,
        x: k.x,
        y: k.y,
        revealed: k.revealed,
        prominence: k.prominence,
        prom_dist: k.prom_dist_m,
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
        // Structural first -- in frame, answered by a ray, actually seen --
        // then the caller's opinion. `min_dominance` and `keep_revealed` are
        // the older, single-field way of saying what `filter` says in
        // general; all three apply, so a client sending both gets the
        // intersection rather than a surprise.
        k.visible
            && k.column.is_some()
            && k.y >= 0.0
            && k.y <= s.height as f64
            && (s.keep_revealed || !k.revealed)
            && k.dominance >= s.min_dominance
            && s.filter.as_ref().is_none_or(|f| f.keeps(&vars(k)))
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
    // Computed once per peak and kept, rather than recomputed inside the
    // comparator -- which is both cheaper and the only way the value can be
    // returned to the caller. An ordering nobody can see is one nobody can
    // debug.
    let program = s.rank.as_ref().unwrap_or_else(|| default_rank());
    for k in peaks.iter_mut() {
        k.rank = Some(program.rank(&vars(k)));
    }
    // `total_cmp`, so a formula producing NaN sorts somewhere definite rather
    // than wherever the comparison happens to leave it.
    peaks.sort_by(|a, b| {
        b.rank
            .unwrap_or(f64::NEG_INFINITY)
            .total_cmp(&a.rank.unwrap_or(f64::NEG_INFINITY))
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
            prominence: None,
            prom_dist_m: None,
            rank: None,
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
            rank: None,
            filter: None,
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

    /// A filter sees every property, which is the point of it: no numeric
    /// threshold can say "a real mountain however it reads from here, or
    /// anything standing out locally", because that needs two properties and
    /// a disjunction.
    #[test]
    fn a_filter_can_say_what_min_dominance_cannot() {
        use serde_json::json;
        let mut far_but_major = peak(1, 40.0, true, Some(0), 10.0);
        far_but_major.prominence = Some(2355.0);
        let mut near_and_local = peak(2, 200.0, true, Some(0), 10.0);
        near_and_local.prominence = None;
        let mut neither = peak(3, 5.0, true, Some(0), 10.0);
        neither.prominence = Some(20.0);

        let f = crate::rank::Program::compile(&json!([
            "case",
            [">", ["coalesce", ["get", "prominence"], 0], 300], 1,
            [">", ["get", "dominance"], 100]
        ]))
        .unwrap();

        let mut peaks = vec![far_but_major, near_and_local, neither];
        select(
            &mut peaks,
            &Selection {
                filter: Some(f),
                // Wide open, so only the expression decides.
                min_dominance: f64::NEG_INFINITY,
                ..sel(0.0, 0)
            },
        );
        assert_eq!(peaks.iter().map(|p| p.osm_id).collect::<Vec<_>>(), [2, 1]);
    }

    /// The legacy thresholds and the expression both apply, so a client
    /// sending both gets the intersection rather than a surprise.
    #[test]
    fn the_old_filters_still_bite_alongside_the_new_one() {
        use serde_json::json;
        let keep_everything = crate::rank::Program::compile(&json!(1)).unwrap();
        let mut peaks = vec![
            peak(1, 100.0, true, Some(0), 10.0),
            peak(2, 5.0, true, Some(0), 10.0),
        ];
        select(
            &mut peaks,
            &Selection {
                filter: Some(keep_everything),
                ..sel(30.0, 0)
            },
        );
        assert_eq!(peaks.iter().map(|p| p.osm_id).collect::<Vec<_>>(), [1]);
    }

    /// The ordering value is returned, so an ordering can be inspected rather
    /// than inferred.
    #[test]
    fn the_rank_used_for_ordering_comes_back() {
        let mut peaks = vec![
            peak(1, 100.0, true, Some(0), 10.0),
            peak(2, 900.0, true, Some(0), 10.0),
        ];
        select(&mut peaks, &sel(0.0, 0));
        assert_eq!(peaks[0].osm_id, 2);
        for p in &peaks {
            let want = default_rank().rank(&vars(p));
            assert!((p.rank.expect("rank is returned") - want).abs() < 1e-9);
        }
    }

    /// The sign rule, pinned with the numbers that exposed it.
    ///
    /// Both of these are real summits from a 2631-peak Ötztal view: a near
    /// subordinate top the client wants labelled, and the peak that sat on
    /// the `max_peaks: 2000` boundary. Dividing throughout -- the obvious
    /// reading of "discount by distance" -- ranks the near one *below* the
    /// boundary, which is how the bug reached production in the first place.
    ///
    /// It now tests the compiled default rather than a second Rust
    /// implementation of it, because there is no longer a second one.
    #[test]
    fn distance_pushes_a_score_down_on_both_sides_of_zero() {
        let score = |dominance: f64, distance: f64| {
            let mut k = peak(1, dominance, true, Some(0), 10.0);
            k.distance = distance;
            default_rank().rank(&vars(&k))
        };
        let (near, far) = ((-259.9f64, 2117.7f64), (-60.4f64, 45_000.0f64));

        // A distant negative is worse than a near one, even a deeper one.
        assert!(score(near.0, near.1) > score(far.0, far.1));
        // Naive division inverts exactly that, which is the trap.
        assert!(near.0 / near.1.sqrt() < far.0 / far.1.sqrt());

        // Positives are discounted too, or a distant massif outranks
        // everything near it.
        assert!(score(100.0, 2_000.0) > score(400.0, 60_000.0));

        // Monotone in distance on both sides of zero.
        for dominance in [-500.0, -1.0, 0.0, 1.0, 500.0] {
            let (n, f) = (score(dominance, 1_000.0), score(dominance, 100_000.0));
            assert!(n >= f, "{dominance}: near {n} ranked below far {f}");
        }

        // Continuous through zero, and the one-metre guard holds where a
        // viewpoint stands on the summit it is ranking.
        assert_eq!(score(0.0, 12_345.0), 0.0);
        assert!(score(100.0, 0.0).is_finite());
    }
}
