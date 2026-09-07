//! Distance-to-colour gradient for the panorama's ground shading.
//!
//! The built-in shading is two colours mixed by Beer-Lambert extinction --
//! `ground_colour` washing towards the sky over a 45 km e-folding. That curve
//! saturates where the interesting terrain is: with the default constant a
//! ridge at 80 km and one at 300 km differ by about 23 of 255 levels per
//! channel, so the whole far field arrives as one colour. Two endpoints also
//! force a straight line through RGB, and dark green to pale blue passes
//! through grey, so the mid-field loses what chroma it had.
//!
//! This replaces both with a stop list. The distance mapping is
//!
//! ```text
//! s = 2d / (d + far)      clamped to [0, 1]
//! ```
//!
//! which puts `s = 1` exactly at `far` -- so the whole palette is spent by the
//! time the terrain runs out -- and `s = 0.5` at `far / 3`, giving the
//! foreground the compression a panorama wants without a tuning constant that
//! has to be explained. An asymptotic map onto `[0, 1)` was tried first and
//! rejected: it can never reach the last stop, so the top of the ramp is
//! structurally unreachable and the final colour never appears in the picture.
//!
//! Stops are positions in that space rather than metres. The two are the same
//! thing -- the map is invertible, so a stop wanted at 30 km of a 100 km scene
//! is `s = 0.46` -- and 0-to-1 is what makes one palette portable across
//! viewpoints, and what lets `far_distance: "auto"` and a written-out number
//! mean the same quantity.

use anyhow::{Result, bail, ensure};
use serde::Deserialize;

use crate::panorama::parse_colour;

/// Entries in the baked table. The ramp is piecewise linear over at most
/// `MAX_STOPS` segments and spans maybe 200 levels, so 1024 samples put the
/// quantisation error well under a level and nearest-neighbour lookup needs no
/// interpolation. 16 KB at `f32`, which stays in L1 across the whole render.
const LUT: usize = 1024;

/// Enough for any palette anyone will hand-write, and a bound at all because
/// the list arrives over HTTP and baking is linear in it.
const MAX_STOPS: usize = 32;

/// Distances `far_distance: "auto"` is allowed to resolve to, metres.
///
/// Auto measures the terrain, and terrain changes as the view is panned, so
/// the raw measurement would recolour the whole picture -- foreground
/// included -- every time a far range slid into frame. That is auto-exposure
/// hunting, and it is worse than the problem it solves. Rounding up to a rung
/// means the scale only moves when the scene genuinely changes depth, and a
/// client dragging the azimuth mostly stays on one.
const LADDER: [f64; 16] = [
    1_000.0, 2_000.0, 3_000.0, 5_000.0, 7_000.0, 10_000.0, 15_000.0, 20_000.0, 30_000.0, 50_000.0,
    70_000.0, 100_000.0, 150_000.0, 200_000.0, 300_000.0, 400_000.0,
];

/// Fraction of probe *bearings* that must see no farther than `far_distance`
/// when it is resolved automatically.
///
/// Over one value per ray -- the farthest terrain along it -- not over every
/// sampled depth. See `measure_depth`, where weighting by pixel gave a 360
/// frame a 15 km scale while one sector of it saw past 50 km.
///
/// Not the maximum: one gap between ridges seeing 250 km would stretch the
/// ramp for the whole picture. 0.95 leaves a couple of probe columns outside,
/// which at 256 probes is about 2% of the horizontal field -- narrow enough to
/// be a gap rather than a view.
const AUTO_PERCENTILE: f64 = 0.95;

/// What a stop paints.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Stop {
    Colour((f64, f64, f64)),
    /// The sky colour at this row, which is what the built-in shading fades
    /// to. Row-dependent, so it cannot be baked into the table like a fixed
    /// colour -- the table carries its weight instead and `Lut::at` returns it
    /// for the caller to composite.
    Sky,
}

/// Where the far end of the ramp sits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Far {
    Metres(f64),
    /// Measured from the terrain actually in frame, then rounded up to
    /// `LADDER`.
    Auto,
}

#[derive(Clone, Debug)]
pub struct Gradient {
    pub far: Far,
    /// Stop marching at `far` rather than painting everything beyond it in the
    /// last stop's colour.
    ///
    /// Only honoured with an explicit `Far::Metres`, and off by default. A
    /// setting whose job is colour must not quietly decide what the picture
    /// contains, and under `Far::Auto` that is exactly what it did: the bound
    /// came from a percentile of the frame's own terrain, so it always sat
    /// below the farthest thing in view and cut more the wider the field. With
    /// a number the caller wrote down there is no such surprise -- it is
    /// `range` for the marcher without also being `range` for `depth_lift`,
    /// which is the one thing `range` cannot express.
    pub clip: bool,
    stops: Vec<(f64, Stop)>,
}

/// The gradient baked against a resolved `far`, ready for the shading loop.
///
/// A table rather than the stop list evaluated per pixel because `surface()`
/// runs up to twice per sub-row per sub-column -- `supersample_x *
/// supersample_y * width * height`, comfortably 10^8 on a default render.
pub struct Lut {
    /// Non-sky contribution, premultiplied by `1 - sky`, then the sky weight.
    /// Interleaved so a lookup touches one cache line.
    table: Vec<[f32; 4]>,
    far: f64,
}

impl Lut {
    /// Terrain colour at distance `d`, as a premultiplied contribution and the
    /// weight of the sky behind it. The caller adds `weight * sky_colour(alt)`
    /// -- sky varies by row, so it cannot be folded in here.
    #[inline]
    pub fn at(&self, d: f64) -> ((f64, f64, f64), f64) {
        let s = (2.0 * d / (d + self.far)).clamp(0.0, 1.0);
        // A NaN `d` saturates to 0 rather than panicking: float-to-int casts
        // in Rust are saturating, and `clamp` passes a NaN straight through.
        let e = self.table[(s * (LUT - 1) as f64) as usize];
        (
            (f64::from(e[0]), f64::from(e[1]), f64::from(e[2])),
            f64::from(e[3]),
        )
    }
}

impl Gradient {
    /// Bake against a resolved far distance.
    pub fn bake(&self, far: f64) -> Lut {
        let table = (0..LUT)
            .map(|i| self.sample(i as f64 / (LUT - 1) as f64))
            .collect();
        Lut { table, far }
    }

    /// Colour of the ramp at position `s`, as the premultiplied pair the table
    /// stores.
    fn sample(&self, s: f64) -> [f32; 4] {
        // Premultiplied, which is what makes a colour stop and a `sky` stop
        // interpolate correctly against each other: a plain lerp would need a
        // colour for the sky stop, and there is not one until the row is
        // known. Contribution `(1 - w) * rgb` and weight `w` both lerp
        // linearly, and adding `w * sky` back afterwards reproduces exactly
        // the lerp between the two stops.
        let parts = |st: Stop| -> [f64; 4] {
            match st {
                Stop::Colour((r, g, b)) => [r, g, b, 0.0],
                Stop::Sky => [0.0, 0.0, 0.0, 1.0],
            }
        };
        let pack = |v: [f64; 4]| [v[0] as f32, v[1] as f32, v[2] as f32, v[3] as f32];

        // Clamped either side rather than extrapolated: a palette that does
        // not start at 0 or reach 1 should hold its ends, not run off into
        // colours nobody wrote down.
        let first = self.stops[0];
        if s <= first.0 {
            return pack(parts(first.1));
        }
        let last = self.stops[self.stops.len() - 1];
        if s >= last.0 {
            return pack(parts(last.1));
        }

        let i = self
            .stops
            .windows(2)
            .position(|w| s < w[1].0)
            .unwrap_or(self.stops.len() - 2);
        let (a_pos, a) = self.stops[i];
        let (b_pos, b) = self.stops[i + 1];
        // Two stops at one position are a hard band edge, deliberately
        // allowed; the guard is what stops it dividing by zero.
        let span = b_pos - a_pos;
        let t = if span > 0.0 { (s - a_pos) / span } else { 0.0 };
        let (pa, pb) = (parts(a), parts(b));
        pack(std::array::from_fn(|k| pa[k] + (pb[k] - pa[k]) * t))
    }

    /// Parse the request shape:
    ///
    /// ```json
    /// { "far_distance": 120000, "clip": true,
    ///   "stops": [[0, "#3a4a34"], [0.4, "#6f89a0"], [1.0, "sky"]] }
    /// ```
    ///
    /// `far_distance` may also be `"auto"`, and defaults to it.
    pub fn parse(v: &serde_json::Value) -> Result<Self> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum FarSpec {
            // Numbers first: untagged tries in order, and a bare number must
            // not be offered to the string arm.
            Metres(f64),
            Named(String),
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Spec {
            #[serde(default)]
            far_distance: Option<FarSpec>,
            #[serde(default)]
            clip: bool,
            stops: Vec<(f64, String)>,
        }

        // Unknown keys are rejected here where they are ignored elsewhere in
        // the request: a misspelled `stops` would otherwise render a silently
        // ungradiented picture, which reads as the feature not working.
        let spec: Spec = serde_json::from_value(v.clone())
            .map_err(|e| anyhow::anyhow!("ground_gradient: {e}"))?;

        let far = match spec.far_distance {
            None => Far::Auto,
            Some(FarSpec::Named(s)) => {
                ensure!(
                    s.eq_ignore_ascii_case("auto"),
                    "ground_gradient: far_distance is a number of metres or \"auto\", not {s:?}"
                );
                Far::Auto
            }
            Some(FarSpec::Metres(m)) => {
                ensure!(
                    m.is_finite() && m > 0.0,
                    "ground_gradient: far_distance must be a positive number of metres"
                );
                Far::Metres(m)
            }
        };

        ensure!(
            spec.stops.len() >= 2,
            "ground_gradient: needs at least two stops"
        );
        ensure!(
            spec.stops.len() <= MAX_STOPS,
            "ground_gradient: at most {MAX_STOPS} stops, got {}",
            spec.stops.len()
        );

        let mut stops = Vec::with_capacity(spec.stops.len());
        let mut prev = f64::NEG_INFINITY;
        for (pos, colour) in &spec.stops {
            ensure!(
                pos.is_finite() && (0.0..=1.0).contains(pos),
                "ground_gradient: stop positions run 0 to 1, got {pos}"
            );
            ensure!(
                *pos >= prev,
                "ground_gradient: stops must not go backwards ({prev} then {pos})"
            );
            prev = *pos;
            let stop = if colour.eq_ignore_ascii_case("sky") {
                Stop::Sky
            } else {
                Stop::Colour(
                    parse_colour(colour).map_err(|e| anyhow::anyhow!("ground_gradient: {e}"))?,
                )
            };
            stops.push((*pos, stop));
        }

        Ok(Gradient {
            far,
            clip: spec.clip,
            stops,
        })
    }
}

/// Round a measured depth up to the next rung. See `LADDER`.
pub fn ladder_up(d: f64) -> f64 {
    LADDER
        .iter()
        .copied()
        .find(|&rung| rung >= d)
        .unwrap_or(LADDER[LADDER.len() - 1])
}

/// The depth `far_distance: "auto"` resolves to, from how far each probe ray
/// saw -- one entry per bearing, not per pixel. Empty means a frame with no
/// terrain in it at all.
pub fn auto_far(mut sightlines: Vec<f64>, fallback: f64) -> f64 {
    if sightlines.is_empty() {
        return ladder_up(fallback);
    }
    let k = (((sightlines.len() - 1) as f64) * AUTO_PERCENTILE).round() as usize;
    let (_, nth, _) = sightlines.select_nth_unstable_by(k, f64::total_cmp);
    ladder_up(*nth)
}

/// Reject a gradient whose numbers would render, but not into a picture --
/// checked before the march rather than after, like the other style knobs.
pub fn validate(g: &Gradient, max_range: f64) -> Result<()> {
    if let Far::Metres(m) = g.far
        && m > max_range
    {
        bail!(
            "ground_gradient: far_distance {m} m is beyond range {max_range} m, so the \
             gradient would end past anything the render can draw"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn grad(v: serde_json::Value) -> Gradient {
        Gradient::parse(&v).expect("test gradient parses")
    }

    /// The property the whole design turns on: the palette is spent by the
    /// time the terrain runs out. An asymptotic map cannot do this, which is
    /// why this one is not asymptotic.
    #[test]
    fn the_ramp_reaches_its_end_exactly_at_far() {
        let g = grad(json!({"far_distance": 10_000, "stops": [[0, "#000000"], [1, "#ffffff"]]}));
        let lut = g.bake(10_000.0);
        let (rgb, _) = lut.at(10_000.0);
        assert!((rgb.0 - 255.0).abs() < 0.5, "got {rgb:?}");
        // And half of it inside the first third, which is the foreground
        // compression that replaces a tuning constant.
        let (mid, _) = lut.at(10_000.0 / 3.0);
        assert!((mid.0 - 127.5).abs() < 1.5, "got {mid:?}");
    }

    #[test]
    fn beyond_far_holds_the_last_colour() {
        let g = grad(json!({"far_distance": 1_000, "stops": [[0, "#000000"], [1, "#ffffff"]]}));
        let lut = g.bake(1_000.0);
        assert_eq!(lut.at(1e9).0.0, 255.0);
    }

    /// A `sky` stop cannot carry a colour, so it has to interpolate as a
    /// weight. Halfway between a black stop and a sky stop must be half the
    /// black and half of whatever sky the row turns out to hold.
    #[test]
    fn a_sky_stop_interpolates_as_a_weight() {
        let g = grad(json!({"far_distance": 1_000, "stops": [[0, "#000000"], [1, "sky"]]}));
        let lut = g.bake(1_000.0);
        // s = 0.5 lands at far/3.
        let (rgb, w) = lut.at(1_000.0 / 3.0);
        assert!((w - 0.5).abs() < 0.01, "sky weight {w}");
        assert!(rgb.0 < 0.01, "black contributes nothing but stays premultiplied");

        let g = grad(json!({"far_distance": 1_000, "stops": [[0, "#ffffff"], [1, "sky"]]}));
        let lut = g.bake(1_000.0);
        let (rgb, w) = lut.at(1_000.0 / 3.0);
        assert!((w - 0.5).abs() < 0.01);
        // Premultiplied: half the white, so adding half the sky reconstructs
        // the plain lerp between them.
        assert!((rgb.0 - 127.5).abs() < 1.5, "got {rgb:?}");
    }

    #[test]
    fn ends_clamp_rather_than_extrapolate() {
        let g = grad(json!({"far_distance": 1_000, "stops": [[0.25, "#000000"], [0.75, "#ffffff"]]}));
        let lut = g.bake(1_000.0);
        assert_eq!(lut.at(0.0).0.0, 0.0);
        assert_eq!(lut.at(1e9).0.0, 255.0);
    }

    /// Two stops at one position are the way to ask for a hard band edge, and
    /// the span guard is what stops that dividing by zero.
    #[test]
    fn stops_may_share_a_position() {
        let g = grad(
            json!({"far_distance": 1_000, "stops": [[0, "#000000"], [0.5, "#000000"], [0.5, "#ffffff"], [1, "#ffffff"]]}),
        );
        let lut = g.bake(1_000.0);
        assert_eq!(lut.at(1_000.0 / 3.0 * 0.9).0.0, 0.0);
        assert_eq!(lut.at(1_000.0).0.0, 255.0);
    }

    #[test]
    fn auto_rounds_up_to_a_rung_so_panning_does_not_recolour() {
        assert_eq!(ladder_up(95_000.0), 100_000.0);
        assert_eq!(ladder_up(100_000.0), 100_000.0);
        assert_eq!(ladder_up(1e9), 400_000.0);
        // A percentile, not the maximum: one far sliver must not stretch the
        // ramp for the whole picture.
        let mut d: Vec<f64> = (0..1000).map(|i| f64::from(i) * 10.0).collect();
        d.push(300_000.0);
        assert_eq!(auto_far(d, 1.0), 10_000.0);
    }

    /// The bug this cost a wrong panorama to find: a 360 view ringed by close
    /// hills, with one sector that sees a long way. Weighted per pixel the far
    /// sector is a fraction of a percent of the frame and the percentile threw
    /// it away -- 15 km for a view that reached past 50 -- and with `clip` on
    /// those hills were then not rendered at all. One entry per bearing is what
    /// makes the sector count for its width rather than its screen area.
    #[test]
    fn a_narrow_sector_that_sees_far_still_sets_the_scale() {
        // 90% of bearings stop at 8 km, 10% of them see 60 km.
        let mut sightlines = vec![8_000.0; 230];
        sightlines.extend(std::iter::repeat_n(60_000.0, 26));
        assert_eq!(auto_far(sightlines, 1.0), 70_000.0);

        // Still robust to a genuine outlier: two bearings out of 256 slipping
        // through a col are a gap, not a view.
        let mut sightlines = vec![8_000.0; 254];
        sightlines.extend([250_000.0, 250_000.0]);
        assert_eq!(auto_far(sightlines, 1.0), 10_000.0);
    }

    #[test]
    fn bad_gradients_are_refused() {
        for bad in [
            json!({"stops": [[0, "#000"]]}),
            json!({"stops": [[0, "#000"], [1.5, "#fff"]]}),
            json!({"stops": [[0.5, "#000"], [0.25, "#fff"]]}),
            json!({"stops": [[0, "#000"], [1, "notacolour"]]}),
            json!({"far_distance": "nearest", "stops": [[0, "#000"], [1, "#fff"]]}),
            json!({"far_distance": -5, "stops": [[0, "#000"], [1, "#fff"]]}),
            json!({"stpos": [[0, "#000"], [1, "#fff"]]}),
        ] {
            assert!(Gradient::parse(&bad).is_err(), "{bad} should be rejected");
        }
    }

    /// Clip is off unless asked for, because it decides what the picture
    /// contains and the rest of this struct only decides its colour.
    #[test]
    fn far_distance_defaults_to_auto_and_clip_to_off() {
        let g = grad(json!({"stops": [[0, "#000"], [1, "#fff"]]}));
        assert_eq!(g.far, Far::Auto);
        assert!(!g.clip);
    }
}
