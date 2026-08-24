//! Ray marcher: the one piece no existing tool provides.
//!
//! Casts a ray per azimuth over the pyramid and produces a **distance buffer**
//! -- for each (azimuth, elevation-angle) cell, the distance to visible
//! terrain, or infinity for sky. Everything else (skyline, haze, silhouettes,
//! peak labels, sun path) is derived from that buffer rather than computed
//! separately.
//!
//! Marching runs near -> far keeping a running maximum elevation angle. A
//! sample is visible exactly when its angle exceeds every nearer sample's, and
//! the band of angles between the old maximum and the new one is terrain at
//! that distance. That fills each column top-down in one pass, with no
//! far-to-near overpainting.
//!
//! Level selection is per sample: panorama resolution is angular, so the cell
//! size needed grows linearly with range. Each sample uses the coarsest level
//! whose ground cell still resolves the angular step, which keeps the sample
//! count logarithmic in range rather than linear.

use anyhow::{Context, Result};
use gdal::Dataset;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::config::{Doc, ground_res};
use crate::grid::lonlat_to_merc;
use crate::peaks::Peak;

const EARTH_R: f64 = 6371000.0;
/// Refraction coefficient. The apparent horizon moves by about half a pixel at
/// 100 km across a realistic spread of k = 0.13 +/- 0.05, which is the floor on
/// how well any of this can be known.
const REFRACTION_K: f64 = 0.13;
/// Cell size needed at distance d is d * this, for a 0.05 deg/px step.
const CELL_PER_METRE: f64 = 0.00087;
/// Ground radius a summit must stand clear of to count as dominant, metres.
/// One decision, so it sets both the depth band and the angular window.
const DOMINANCE_RADIUS: f64 = 3000.0;
const BLOCK: usize = 512;

pub struct Params {
    pub lon: f64,
    pub lat: f64,
    pub eye_height: f64,
    pub az_start: f64,
    pub az_span: f64,
    pub alt_min: f64,
    pub alt_max: f64,
    /// Output degrees per pixel. Sub-steps are this divided by the
    /// supersampling factors.
    pub step_deg: f64,
    pub max_range: f64,
    /// Depth ratio above which a ridge-against-ridge edge is stroked. Set very
    /// high to stroke only true terrain-against-sky skylines.
    pub edge_ratio: f64,
    /// Hidden extent, in metres, at which a silhouette reaches full strength.
    ///
    /// Foreground slopes are full of real but trivial occlusions -- the ray
    /// clearing a terrace lip and suddenly seeing far. They are genuine
    /// silhouettes, so no threshold on depth can remove them; what separates
    /// them from a range hiding forty kilometres is how much they conceal.
    pub edge_hidden_ref: f64,
    /// Draw the eye-level line at 0 degrees.
    pub eye_level: bool,
    /// Radius, in metres, over which the viewpoint's ground elevation is taken
    /// as the local maximum rather than the value at the exact point.
    ///
    /// The pyramid stores a 6.27 m average, and averaging costs a summit more
    /// the sharper it is, so `value at the point + eye height` reliably places
    /// the eye below where a person would stand. On Gerlach that put ground
    /// 10 m away 6.6 m above the eye -- subtending +33 degrees and filling the
    /// frame with rock.
    pub eye_search_radius: f64,
    /// Rays per output pixel horizontally.
    ///
    /// At long range several DEM cells fall inside one pixel's angular
    /// footprint and a single ray picks an arbitrary one of them.
    pub supersample_x: u32,
    /// Sub-rows per output pixel vertically.
    ///
    /// Sub-pixel placement is analytic, but only for *one* edge per cell: the
    /// column holds a single surface per row, so where several ridge bands
    /// fall inside one output pixel all but the nearest are discarded and
    /// never stroked. Extra rows give each band a row to occupy.
    ///
    /// Stroke width is expressed in output pixels, so it scales with this.
    pub supersample_y: u32,

    /// How heavily ridge silhouettes are inked, as a multiplier on the
    /// strength the geometry calls for. 0 removes them entirely.
    pub ridge_strength: f64,
    /// Stroke thickness in output pixels. Independent of `ridge_strength`:
    /// interior rows of a stroke ink at the same alpha however wide it is, so
    /// widening thickens the line without darkening it.
    pub ridge_width: f64,
    /// What the silhouettes are drawn in. Black is the default and reads as
    /// shading; a lighter ink gives an engraved look, and a colour matching
    /// the ground makes ridges read as folds rather than outlines.
    pub ridge_colour: (f64, f64, f64),
    /// Colour of near terrain, before haze washes it towards the sky.
    pub ground_colour: (f64, f64, f64),
    /// Multiplier on the dither applied at the final 8-bit quantisation; see
    /// `DEFAULT_DITHER`. 0 turns it off, which is how to tell dithering apart
    /// from anything else in a gradient.
    pub dither_strength: f64,

    /// Degrees to raise terrain by, at `max_range`, growing linearly with
    /// distance. 0 is the true projection.
    ///
    /// Distance compresses a panorama into its horizon: from a summit the
    /// nearest ridge can span twenty degrees while a range four times as far
    /// away, and just as tall, gets two. Lifting by depth unfolds that, which
    /// is what hand-drawn panoramas have always done -- the price being that
    /// the picture stops being a photograph of anything. Ground the eye
    /// cannot see comes into view, and the peaks standing on it come back
    /// flagged `revealed`.
    ///
    /// It has to decide occlusion, not just placement. The lift is a warp of
    /// the world -- every point rises in proportion to its distance -- so
    /// visibility belongs to the warped world too. Displacing pixels while
    /// leaving visibility in the true world was tried and is incoherent: the
    /// lift opens a gap between a near crest and the range behind it, the
    /// band fill has nothing to put there but a stretched copy of the far
    /// surface, and distant ranges come out as flat-topped slabs with
    /// vertical sides, growing taller the more lift is asked for.
    ///
    /// Linear in distance because the alternative is worse: any curve steep
    /// near the eye tears the foreground apart, and the eye reads absolute
    /// separation between ranges rather than the rate it grows at. The
    /// horizon moves by exactly this much, so the frame usually wants
    /// `alt_max` raised to match or the far ridges climb out of it.
    pub depth_lift: f64,
}

impl Params {
    /// Degrees of lift per metre of distance.
    ///
    /// Split out from `lift_at` so the marcher can hoist the division: it asks
    /// this once per ray and multiplies per sample, over a hundred million of
    /// them in a full render.
    fn lift_per_m(&self) -> f64 {
        self.depth_lift / self.max_range
    }

    /// Degrees terrain `d` metres away is raised by.
    ///
    /// The one definition of the rule. It decides where samples are drawn,
    /// where peak labels sit, and which peaks count as visible, and those
    /// three have to agree exactly or labels drift off their summits.
    fn lift_at(&self, d: f64) -> f64 {
        self.lift_per_m() * d
    }
}

/// The horizon a probe found, twice over.
///
/// Two, because a lift makes the question ambiguous: the terrain that occludes
/// in the drawing is not the terrain that occludes in the world. `drawn`
/// decides what the render shows and `real` what the eye could actually see.
/// They are the same wherever `depth_lift` is 0.
#[derive(Clone, Copy)]
struct Horizon {
    drawn: f64,
    real: f64,
}

/// Whether `alt` clears the horizon its two bracketing rays reported.
///
/// Three states per side, and conflating any two of them is a bug. NaN: no ray
/// answered, so the peak is outside the rendered arc. NEG_INFINITY: a ray
/// answered but found no ground at all before the peak -- nodata or outside
/// coverage, not open sky. Finite: a real horizon.
///
/// Only two finite horizons may be interpolated. Lerping a real one against a
/// no-coverage ray would average a blocking ridge with a hole and let a hidden
/// peak through, so where one side has no coverage the side that does decides
/// -- and where neither does, no ground was found to block anything.
fn clears(alt: f64, h0: f64, h1: f64, t: f64) -> bool {
    match (h0.is_finite(), h1.is_finite()) {
        (true, true) => alt >= h0 + (h1 - h0) * t,
        (true, false) => alt >= h0,
        (false, true) => alt >= h1,
        (false, false) => !(h0.is_nan() && h1.is_nan()),
    }
}

/// One pyramid level, with a block cache over its GTI index.
struct Level {
    z: u32,
    ds: Dataset,
    gt: [f64; 6],
    w: usize,
    h: usize,
    /// Blocks in arrival order; `None` where the index has no tile.
    blocks: Vec<Option<Vec<u16>>>,
    /// Where each block landed, so a repeat visit finds it.
    index: HashMap<(i64, i64), usize>,
    /// The block the last lookup wanted.
    ///
    /// A ray walks continuously and bilinear reads four neighbouring corners,
    /// so successive lookups nearly always want the block the previous one
    /// did. Without this every corner of every sample hashes its key -- and
    /// the default hasher is SipHash, which is cryptographic. It measured 45%
    /// of an entire render, against 1% for all the trigonometry.
    last: Option<((i64, i64), usize)>,
}

impl Level {
    fn open(root: &Path, z: u32) -> Result<Option<Self>> {
        let path = root.join("index").join(format!("z{z}.gti.gpkg"));
        if !path.exists() {
            return Ok(None);
        }
        let ds = Dataset::open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let gt = ds.geo_transform()?;
        let (w, h) = ds.raster_size();
        Ok(Some(Self {
            z,
            ds,
            gt,
            w,
            h,
            blocks: Vec::new(),
            index: HashMap::new(),
            last: None,
        }))
    }

    /// Elevation at integer pixel coordinates, or None for nodata / off-grid.
    /// The block covering `key`, reading it in on first use.
    ///
    /// One hash at most, and usually none: the previous key is checked first,
    /// and the old code hashed twice per corner -- `contains_key` then `get` --
    /// so eight times per bilinear sample.
    fn block_at(&mut self, key: (i64, i64)) -> Option<&[u16]> {
        let slot = match self.last {
            Some((k, i)) if k == key => i,
            _ => {
                let i = match self.index.get(&key) {
                    Some(&i) => i,
                    None => {
                        let block = self.read_block(key);
                        self.blocks.push(block);
                        let i = self.blocks.len() - 1;
                        self.index.insert(key, i);
                        i
                    }
                };
                self.last = Some((key, i));
                i
            }
        };
        self.blocks[slot].as_deref()
    }

    fn value_at(&mut self, px: usize, py: usize) -> Option<f64> {
        if px >= self.w || py >= self.h {
            return None;
        }
        let key = ((px / BLOCK) as i64, (py / BLOCK) as i64);
        let block = self.block_at(key)?;

        let v = block[(py % BLOCK) * BLOCK + (px % BLOCK)];
        // 0 is the reserved nodata of the uint16 decimetre encoding.
        if v == 0 {
            None
        } else {
            Some(f64::from(v) * 0.1 - 500.0)
        }
    }

    /// Bilinear sample at a Web Mercator position.
    ///
    /// Nearest-neighbour staircases the skyline badly: at 2 km adjacent rays
    /// are 1.75 m apart while cells are 6.27 m, so many rays return the
    /// identical cell and the horizon comes out as flat runs with sudden steps.
    /// Interpolating removes that without touching the marching logic.
    fn sample(&mut self, x: f64, y: f64) -> Option<f64> {
        let fx = (x - self.gt[0]) / self.gt[1] - 0.5;
        let fy = (y - self.gt[3]) / self.gt[5] - 0.5;
        if fx < 0.0 || fy < 0.0 {
            return None;
        }
        let (x0, y0) = (fx.floor(), fy.floor());
        let (tx, ty) = (fx - x0, fy - y0);
        let (x0, y0) = (x0 as usize, y0 as usize);

        let v00 = self.value_at(x0, y0);
        match (
            v00,
            self.value_at(x0 + 1, y0),
            self.value_at(x0, y0 + 1),
            self.value_at(x0 + 1, y0 + 1),
        ) {
            (Some(a), Some(b), Some(c), Some(d)) => Some(
                a * (1.0 - tx) * (1.0 - ty)
                    + b * tx * (1.0 - ty)
                    + c * (1.0 - tx) * ty
                    + d * tx * ty,
            ),
            // At a coverage edge, fall back to nearest rather than losing the
            // sample entirely.
            _ => v00,
        }
    }

    fn read_block(&self, (bx, by): (i64, i64)) -> Option<Vec<u16>> {
        let x0 = bx as usize * BLOCK;
        let y0 = by as usize * BLOCK;
        let w = BLOCK.min(self.w.saturating_sub(x0));
        let h = BLOCK.min(self.h.saturating_sub(y0));
        if w == 0 || h == 0 {
            return None;
        }

        let band = self.ds.rasterband(1).ok()?;
        let buf = band
            .read_as::<u16>((x0 as isize, y0 as isize), (w, h), (w, h), None)
            .ok()?;
        let data = buf.data();

        // Always store a full BLOCK x BLOCK tile so indexing stays uniform;
        // the ragged edge is left as nodata.
        let mut out = vec![0u16; BLOCK * BLOCK];
        for row in 0..h {
            out[row * BLOCK..row * BLOCK + w]
                .copy_from_slice(&data[row * w..row * w + w]);
        }
        Some(out)
    }
}

/// The pyramid, coarsest-first so fallback is a forward scan.
pub(crate) struct Pyramid {
    levels: Vec<Level>, // ascending z
}

impl Pyramid {
    pub(crate) fn open(root: &Path, doc: &Doc) -> Result<Self> {
        let mut levels = Vec::new();
        for z in doc.grid.coarsest_level..=doc.grid.finest_level {
            if let Some(l) = Level::open(root, z)? {
                levels.push(l);
            }
        }
        anyhow::ensure!(!levels.is_empty(), "no pyramid indexes found under {}", root.display());
        Ok(Self { levels })
    }

    /// Sample at the requested level, falling back to coarser levels where the
    /// finer ones are sparse (national data absent, GEDTM30 present).
    pub(crate) fn sample(&mut self, want_z: u32, x: f64, y: f64) -> Option<f64> {
        let mut idx = self
            .levels
            .iter()
            .rposition(|l| l.z <= want_z)
            .unwrap_or(0);
        loop {
            if let Some(v) = self.levels[idx].sample(x, y) {
                return Some(v);
            }
            if idx == 0 {
                return None;
            }
            idx -= 1;
        }
    }

    /// Finest available elevation at a point, for the eye position.
    pub(crate) fn sample_finest(&mut self, x: f64, y: f64) -> Option<f64> {
        let top = self.levels.last().map(|l| l.z)?;
        self.sample(top, x, y)
    }

    fn cached_blocks(&self) -> usize {
        self.levels.iter().map(|l| l.blocks.len()).sum()
    }
}

/// Coarsest level whose ground cell still resolves the angular step at `d`.
pub(crate) fn level_for(d: f64, lat: f64, coarsest: u32, finest: u32) -> u32 {
    let target = d * CELL_PER_METRE;
    for z in coarsest..=finest {
        if ground_res(z, lat) <= target {
            return z;
        }
    }
    finest
}

/// Great-circle distance in metres, degrees in.
pub fn great_circle(lon1: f64, lat1: f64, lon2: f64, lat2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let (dp, dl) = ((lat2 - lat1).to_radians(), (lon2 - lon1).to_radians());
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * EARTH_R * a.sqrt().asin()
}

/// How far the surface falls away over `d` metres, curvature less refraction.
///
/// Written out at every site that projects terrain -- the marcher, the peak
/// geometry, the elevation the dominance walk reads back -- and those three
/// must agree or a summit is measured in a different geometry than it was
/// drawn in.
pub(crate) fn curvature_drop(d: f64) -> f64 {
    d * d * (1.0 - REFRACTION_K) / (2.0 * EARTH_R)
}

/// Initial bearing, degrees clockwise from north.
pub fn initial_bearing(lon1: f64, lat1: f64, lon2: f64, lat2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dl = (lon2 - lon1).to_radians();
    let y = dl.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
    y.atan2(x).to_degrees().rem_euclid(360.0)
}

/// Great-circle destination, degrees in and out.
pub(crate) fn destination(lon: f64, lat: f64, az_deg: f64, d: f64) -> (f64, f64) {
    let (lat1, lon1) = (lat.to_radians(), lon.to_radians());
    let theta = az_deg.to_radians();
    let delta = d / EARTH_R;
    let (sin_lat1, cos_lat1) = lat1.sin_cos();
    let (sin_d, cos_d) = delta.sin_cos();
    let lat2 = (sin_lat1 * cos_d + cos_lat1 * sin_d * theta.cos()).asin();
    let lon2 = lon1
        + (theta.sin() * sin_d * cos_lat1).atan2(cos_d - sin_lat1 * lat2.sin());
    (lon2.to_degrees(), lat2.to_degrees())
}

/// What a render cost, for the summary line.
pub struct Stats {
    pub width: usize,
    pub height: usize,
    pub eye_elevation: f64,
    pub samples: usize,
    pub blocks: usize,
    pub sky_fraction: f64,
    /// Per-pixel distance, 16-bit log-encoded, 0 for sky. Written only if
    /// asked for; see `encode_depth`.
    pub depth: image::ImageBuffer<image::Luma<u16>, Vec<u16>>,
}

/// One ray's column: distance to visible terrain per sub-row, and how much of
/// each sub-row that surface covers.
struct Column {
    dist: Vec<f64>,
    cover: Vec<f32>,
}

/// Log-spaced distance bins holding the highest ground the marcher saw in
/// each, per output column.
///
/// Dominance asks "what is the highest ground beside this summit, at its own
/// depth" -- elevation against distance, which is exactly what the marcher
/// has and used to discard. Recovering it from the rendered depth instead
/// meant inverting the projection through an angular row, and a row spans
/// `d * step` of elevation: 210 m at 60 km on the coarse tier against 52 m on
/// the fine one. Scores moved with quality enough to scramble the ranking --
/// the top twenty by dominance agreed on seven -- so labels chosen by it were
/// never going to be stable however stable visibility became.
///
/// Bins are log-spaced over the same range as the depth encoding, so a bin is
/// a fixed *ratio* of distance, matching a neighbourhood that scales with how
/// far away it is.
/// 256 spans a factor of 1.042 each. It has to beat the neighbourhood it is
/// asked about, and that narrows with distance: the band at 60 km is a factor
/// of 1.105, where 64 bins would have spanned 1.18 -- one slot wider than the
/// whole question, smearing a summit's neighbours into the range behind it.
/// 7 MB for a default 360 degrees, 18 MB at the pixel cap.
const PROFILE_BINS: usize = 256;

struct Profile {
    /// `PROFILE_BINS` per column, column-major. `NEG_INFINITY` where the
    /// marcher saw no ground at that depth.
    max_h: Vec<f32>,
}

impl Profile {
    fn new(cols: usize) -> Self {
        Self {
            max_h: vec![f32::NEG_INFINITY; cols * PROFILE_BINS],
        }
    }

    fn bin_of(d: f64) -> usize {
        let t = (d.max(DEPTH_NEAR).ln() - DEPTH_NEAR.ln()) / (DEPTH_FAR.ln() - DEPTH_NEAR.ln());
        ((t.clamp(0.0, 1.0) * (PROFILE_BINS - 1) as f64) as usize).min(PROFILE_BINS - 1)
    }

    /// Upper edge of each bin, so the marcher can walk bins instead of taking
    /// a logarithm per sample -- which is what it does millions of times.
    fn edges() -> [f64; PROFILE_BINS] {
        let mut e = [0.0; PROFILE_BINS];
        let span = DEPTH_FAR.ln() - DEPTH_NEAR.ln();
        for (i, edge) in e.iter_mut().enumerate() {
            *edge = (DEPTH_NEAR.ln() + span * (i as f64 + 1.0) / (PROFILE_BINS - 1) as f64).exp();
        }
        e
    }

    /// Record a sample, given a bin cursor the caller advances monotonically.
    ///
    /// `d` only ever grows within a ray, so the bin is found by stepping
    /// forward from wherever the last sample left off.
    fn record(&mut self, col: usize, bin: &mut usize, edges: &[f64; PROFILE_BINS], d: f64, h: f64) {
        while *bin + 1 < PROFILE_BINS && d >= edges[*bin] {
            *bin += 1;
        }
        let slot = &mut self.max_h[col * PROFILE_BINS + *bin];
        *slot = slot.max(h as f32);
    }

    /// Highest ground in `col` between two distances, or `None` if the
    /// marcher found none there.
    fn max_between(&self, col: usize, near: f64, far: f64) -> Option<f64> {
        let (a, b) = (Profile::bin_of(near), Profile::bin_of(far));
        let best = self.max_h[col * PROFILE_BINS + a..=col * PROFILE_BINS + b]
            .iter()
            .fold(f32::NEG_INFINITY, |m, &v| m.max(v));
        best.is_finite().then(|| f64::from(best))
    }
}

impl Column {
    fn new(h: usize) -> Self {
        Self {
            dist: vec![f64::INFINITY; h],
            cover: vec![0f32; h],
        }
    }
}

/// The two rays bracketing a bearing, and how far between them it lies.
///
/// `sub_pos` is the bearing in sub-columns from the frame's start. Ray `k`
/// looks along centre `k + 0.5`, so a bearing at `u` sits between rays
/// `floor(u - 0.5)` and the next. A full circle wraps -- the ray before the
/// first is the last -- and anything else drops the ray that falls outside.
fn bracketing(sub_pos: f64, sub_cols: usize, wraps: bool) -> ([Option<usize>; 2], f64) {
    let u = sub_pos - 0.5;
    let k0 = u.floor();
    let mut rays = [None, None];
    for (slot, k) in [(0usize, k0), (1, k0 + 1.0)] {
        rays[slot] = if wraps {
            Some((k as isize).rem_euclid(sub_cols as isize) as usize)
        } else if k >= 0.0 && (k as usize) < sub_cols {
            Some(k as usize)
        } else {
            None
        };
    }
    (rays, u - k0)
}

/// March one ray and fill its column.
///
/// `probe_d` holds distances, ascending, at which the caller wants the horizon
/// -- the highest elevation angle reached by terrain strictly nearer than
/// that. `probe_h` receives one angle per probe. This is how peaks are
/// answered: the marcher is already computing the running maximum that decides
/// what the image shows, so asking it directly is both cheaper and impossible
/// to disagree with. Reading it back out of the finished column instead made
/// visibility depend on the render's own row height, which cost 36% of the
/// labels at coarse quality.
#[allow(clippy::too_many_arguments)]
fn march_ray(
    pyr: &mut Pyramid,
    p: &Params,
    eye: f64,
    az: f64,
    alt_step: f64,
    height: usize,
    coarsest: u32,
    finest: u32,
    col: &mut Column,
    probe_d: &[f64],
    probe_h: &mut Vec<Horizon>,
    profile: Option<(&mut Profile, usize)>,
) -> usize {
    let (mut profile, profile_col) = match profile {
        Some((p, c)) => (Some(p), c),
        None => (None, 0),
    };
    let profile_edges = Profile::edges();
    let mut profile_bin = 0usize;
    probe_h.clear();
    // NEG_INFINITY carries "this ray found no ground at all before here" all
    // the way to the caller, which must not confuse it with a low horizon:
    // it means no coverage, not open sky.
    probe_h.resize(
        probe_d.len(),
        Horizon {
            drawn: f64::NEG_INFINITY,
            real: f64::NEG_INFINITY,
        },
    );
    let mut probe_i = 0usize;
    let mut samples = 0usize;
    let mut max_alpha = f64::NEG_INFINITY;
    // The horizon geometry alone would give. Tracked always rather than only
    // when the lift reveals: it costs one comparison per sample, and the
    // branch it would save sits in the hottest loop in the program.
    let mut max_real = f64::NEG_INFINITY;
    let lift_per_m = p.lift_per_m();
    // Row 0 is the top of the frame, so a larger angle means a smaller row
    // index. `filled_from` is the topmost row already written; a new maximum
    // fills the band [row_new, filled_from) and moves the boundary up.
    let mut filled_from = height;
    let mut last_visible = ground_res(finest, p.lat);

    let mut d = ground_res(finest, p.lat);
    while d < p.max_range {
        let z = level_for(d, p.lat, coarsest, finest);
        let step = ground_res(z, p.lat);

        // Answer probes before sampling, and one step early: the interval that
        // brackets a summit contains the summit's own ground, and a broad top
        // read at its near lip would occlude itself. Skipping that interval is
        // a geometric statement, where the old distance comparison needed a
        // fudge factor -- which was wrong three times over.
        while probe_i < probe_d.len() && probe_d[probe_i] <= d + step {
            probe_h[probe_i] = Horizon {
                drawn: max_alpha,
                real: max_real,
            };
            probe_i += 1;
        }

        let (lon, lat) = destination(p.lon, p.lat, az, d);
        let (sx, sy) = lonlat_to_merc(lon, lat);

        if let Some(h) = pyr.sample(z, sx, sy) {
            samples += 1;
            // Every sample, not just the ones that raise the horizon: ground
            // hidden behind a nearer crest is still ground beside a summit,
            // and dominance asks about the landscape, not the silhouette.
            if let Some(profile) = profile.as_deref_mut() {
                profile.record(profile_col, &mut profile_bin, &profile_edges, d, h);
            }
            let drop = curvature_drop(d);
            let alpha = ((h - eye - drop) / d).atan().to_degrees();
            if alpha > max_real {
                max_real = alpha;
            }
            // Occlusion is decided on the lifted angle, which is what makes
            // hidden ground appear: the far surface is compared against the
            // near ridge after both have been raised, and being farther it is
            // raised more. Deciding it on `alpha` instead -- lifting only
            // where a sample lands -- opens a gap at every crest that the
            // band fill can only paper over, and the far ranges come out as
            // slabs.
            //
            // Accepted samples march monotonically up the frame, which is
            // what the band fill below relies on: the test makes `alpha +
            // lift` rise, and rows follow it.
            let drawn = alpha + lift_per_m * d;

            if drawn > max_alpha {
                max_alpha = drawn;
                // The band's top edge lands at a fractional row; keeping that
                // fraction as coverage is what antialiases the horizon, and it
                // costs nothing since alpha is already a float.
                let f = ((p.alt_max - drawn) / alt_step).max(0.0);
                if f < filled_from as f64 {
                    let full_start = f.ceil() as usize;
                    // Rows between two *consecutive* samples show terrain
                    // genuinely lying between them, so the distance ramps
                    // across the band. Shading a whole band at one distance is
                    // what facets a steep near face into flat polygons.
                    //
                    // Not across an occlusion though: when the ray clears a
                    // crest the gap holds no terrain at all, and everything
                    // above the crest is the far surface. Interpolating there
                    // would smear the silhouette over many rows.
                    let smooth = d <= last_visible * 1.05;
                    let span = (filled_from - full_start).max(1) as f64;
                    for row in full_start..filled_from {
                        col.dist[row] = if smooth {
                            let t = (row - full_start) as f64 / span;
                            d + (last_visible - d) * t
                        } else {
                            d
                        };
                        col.cover[row] = 1.0;
                    }
                    let partial = f.floor() as usize;
                    if partial < full_start {
                        col.dist[partial] = d;
                        col.cover[partial] = (full_start as f64 - f) as f32;
                        filled_from = partial;
                    } else {
                        filled_from = full_start;
                    }
                    last_visible = d;
                }
                // Once the column is full, no farther sample can add to it.
                if filled_from == 0 {
                    break;
                }
            }
        }
        d += step;
    }
    // Past the last sample, or out the early exit once the column filled:
    // whatever the horizon had reached stands for everything beyond.
    for h in &mut probe_h[probe_i..] {
        *h = Horizon {
            drawn: max_alpha,
            real: max_real,
        };
    }
    samples
}

/// Shade one column into RGB per sub-row.
///
/// Everything here is column-local -- edge detection looks only at the rows
/// above and below within the same column -- which is what lets the whole
/// render stream one output column at a time instead of materialising the
/// full supersampled buffer.
fn shade_column(col: &Column, p: &Params, alt_step: f64, height: usize) -> Vec<(f64, f64, f64)> {
    let haze = 45_000.0_f64; // e-folding distance for the atmospheric blend

    // A raw depth ratio is the wrong test for a silhouette. Terrain receding at
    // grazing incidence -- any foreground slope -- changes distance enormously
    // per row while staying perfectly continuous, so a first-difference test
    // paints false contours across it. Meanwhile genuine ridges at similar
    // ranges (40 km against 48 km) fall under the same threshold and get
    // nothing.
    //
    // What marks a silhouette is a *discontinuity*: the jump at this row is far
    // larger than the jumps just above and below it. Comparing against the
    // local gradient rather than a constant separates a step from a
    // steep-but-smooth surface, and works at any range.
    let spike = p.edge_ratio.max(1.001).ln();
    let log_d = |row: usize| -> Option<f64> { col.dist[row].is_finite().then(|| col.dist[row].ln()) };

    // Continuous strength rather than a binary test. A hard threshold makes
    // edges dash in and out wherever the measure hovers around it, and forces a
    // choice between drawing trivial edges and dropping real ones.
    //
    // Strength is the *hidden extent* -- how much terrain the silhouette
    // conceals -- gated by the discontinuity measure. Depth ratio alone is the
    // wrong driver: a terrace lip at 300 m revealing 2 km is a bigger relative
    // jump (1.9 in log space) than a ridge at 20 km revealing 60 km (1.1), so
    // ranking by ratio promotes exactly the edges worth suppressing. Absolute
    // hidden extent ranks them as the eye does: 1.7 km against 40 km.
    let edge_strength = |row: usize| -> f64 {
        let Some(here) = log_d(row) else {
            return 0.0;
        };
        if row == 0 {
            return 1.0;
        }
        let Some(above) = log_d(row - 1) else {
            return 1.0; // sky above terrain: the skyline itself
        };
        let step_up = above - here;
        if step_up <= 0.0 {
            return 0.0;
        }
        // On a continuous surface the step above matches the step below however
        // steep it is; an occlusion spikes.
        let step_down = if row + 1 < height {
            log_d(row + 1).map_or(0.0, |b| here - b).max(0.0)
        } else {
            0.0
        };
        let discontinuity = ((step_up - step_down) / spike).clamp(0.0, 1.0);

        let hidden = here.exp() * step_up.exp_m1();
        // Square root so a modest nearby ridge still registers against a
        // distant range that hides ten times more.
        let extent = (hidden / p.edge_hidden_ref).clamp(0.0, 1.0).sqrt();

        discontinuity * extent
    };

    // Colour of whatever surface a row holds, ignoring coverage.
    let surface = |row: usize| -> (f64, f64, f64) {
        let alt = p.alt_max - (row as f64 + 0.5) * alt_step;
        let sky = sky_colour(alt);
        let d = col.dist[row];
        if d.is_finite() {
            // Near terrain is dark and saturated, far terrain washes out
            // towards the sky colour.
            let t = 1.0 - (-d / haze).exp();
            let base = p.ground_colour;
            (
                lerp(base.0, sky.0, t),
                lerp(base.1, sky.1, t),
                lerp(base.2, sky.2, t),
            )
        } else {
            sky
        }
    };

    // A one-pixel-wide line centred on the edge's true sub-pixel height,
    // distributed by overlap: an edge at 20.75 puts 25% into row 20 and 75%
    // into row 21, and one landing exactly on 20.5 lands wholly in row 20.
    //
    // The ink applies to whatever the line covers, sky included -- a dark
    // outline over the horizon does darken the sky above it.
    // In output pixels, so a line looks the same thickness whatever
    // resolution was asked for -- sub-rows are only how it is drawn.
    let stroke_half_width = 0.5 * p.ridge_width * f64::from(p.supersample_y);
    let mut ink = vec![0f64; height];

    for row in 0..height {
        let s = edge_strength(row);
        if s <= 0.0 {
            continue;
        }
        let depth_fade = 0.45 + 0.4 * (1.0 - (-col.dist[row] / haze).exp());
        let amount = s * (1.0 - depth_fade) * p.ridge_strength;

        // Coverage encodes where the band's top edge actually falls: a row
        // covered `c` by its band has that edge at `row + (1 - c)`.
        let c = f64::from(col.cover[row]);
        let edge_pos = row as f64 + (1.0 - c);
        let lo = edge_pos - stroke_half_width;
        let hi = edge_pos + stroke_half_width;

        let first = lo.floor().max(0.0) as usize;
        let last = (hi.ceil() as usize).min(height);
        for y in first..last {
            let overlap = hi.min((y + 1) as f64) - lo.max(y as f64);
            if overlap > 0.0 {
                ink[y] += amount * overlap;
            }
        }
    }

    (0..height)
        .map(|row| {
            let alt = p.alt_max - (row as f64 + 0.5) * alt_step;
            let d = col.dist[row];

            // Composite the partially-covered top edge of a band over whatever
            // lies behind it -- sky on the horizon, farther terrain within a
            // ridge stack. Rounding this fraction away leaves a staircase.
            let c = f64::from(col.cover[row]);
            let front = surface(row);
            let px = if d.is_finite() && c < 0.999 {
                let back = if row > 0 {
                    surface(row - 1)
                } else {
                    sky_colour(alt)
                };
                (
                    lerp(back.0, front.0, c),
                    lerp(back.1, front.1, c),
                    lerp(back.2, front.2, c),
                )
            } else {
                front
            };

            // Blended towards the ink colour rather than scaled towards black,
            // so the default (black, full strength) is the multiply it always
            // was and any other colour composites the same way.
            let k = ink[row].clamp(0.0, 1.0);
            let px = (
                lerp(px.0, p.ridge_colour.0, k),
                lerp(px.1, p.ridge_colour.1, k),
                lerp(px.2, p.ridge_colour.2, k),
            );

            // Faint eye-level line at 0 deg.
            if p.eye_level && alt.abs() < alt_step * 0.5 {
                (px.0 * 0.75 + 60.0, px.1 * 0.75 + 60.0, px.2 * 0.75 + 60.0)
            } else {
                px
            }
        })
        .collect()
}

/// Ground elevation at the viewpoint, as the local maximum over a small disc.
///
/// The pyramid stores a 6.27 m average and averaging costs a summit more the
/// sharper it is, so the value at the exact point reliably sits below where a
/// person would stand -- which puts nearby rock above the eye.
fn viewpoint_elevation(pyr: &mut Pyramid, p: &Params) -> Result<f64> {
    let (ex, ey) = lonlat_to_merc(p.lon, p.lat);
    let at_point = pyr
        .sample_finest(ex, ey)
        .context("viewpoint has no elevation data")?;

    // Mercator metres are ground metres divided by cos(lat).
    let r = p.eye_search_radius / p.lat.to_radians().cos();
    let mut best = at_point;
    if r > 0.0 {
        for k in 0..8 {
            let a = f64::from(k) * std::f64::consts::FRAC_PI_4;
            if let Some(h) = pyr.sample_finest(ex + r * a.cos(), ey + r * a.sin()) {
                best = best.max(h);
            }
        }
    }
    Ok(best)
}


/// Render the panorama, streaming one output column at a time.
///
/// Supersampling is what makes the strokes and the far skyline look right, but
/// materialising the whole supersampled buffer is what made it expensive: a
/// 360 degree frame at 9x9 needed 6.7 GB. Nothing requires that. An output
/// column depends only on its own sub-columns, and shading is column-local, so
/// each is marched, shaded, averaged and discarded in turn.
/// Encode a distance as a 16-bit depth sample.
///
/// Logarithmic, because the useful precision is relative: a metre matters at
/// 200 m and is meaningless at 200 km. Over 10 m to 400 km this holds better
/// than 0.02% everywhere, so a reading is good to ~4 m at 20 km. 0 is sky.
pub const DEPTH_NEAR: f64 = 10.0;
pub const DEPTH_FAR: f64 = 400_000.0;

pub fn encode_depth(d: f64) -> u16 {
    if !d.is_finite() {
        return 0;
    }
    let t = (d.max(DEPTH_NEAR).ln() - DEPTH_NEAR.ln()) / (DEPTH_FAR.ln() - DEPTH_NEAR.ln());
    1 + (t.clamp(0.0, 1.0) * 65_534.0).round() as u16
}

// The inverse of `encode_depth` lives in docs/API.md, where the client needs
// it. Nothing here decodes any more: dominance reads elevations the marcher
// recorded directly, rather than reconstructing them from the picture.

/// Round a depth sample down to a multiple of `step`, never onto the sentinel.
///
/// Quantising in the offset from the sentinel rather than in the raw value is
/// what makes that guarantee structural. Flooring the value itself sends
/// everything below `step` to 0 -- ground within `DEPTH_NEAR` encodes as 1, so
/// at the default step the bottom of every frame arrived as sky. That was
/// written, and then fixed, once per emitter, because both emitters open-coded
/// it; the sentinel's rule now lives beside the codec that defines it.
pub fn quantise_depth(v: u16, step: u16) -> u16 {
    match v {
        0 => 0,
        v => 1 + ((v - 1) / step.max(1)) * step.max(1),
    }
}

/// The depth channel as it goes over the wire: quantised, delta-coded along
/// each row, gzipped.
///
/// Deltas are `i16` over `u16` values, so a step between sky and distant
/// terrain overflows deliberately; readers accumulate modulo 65536. Rows reset
/// the accumulator, which costs one full-width value per row and keeps a
/// corrupt row from poisoning the rest.
pub fn depth_bytes(
    depth: &image::ImageBuffer<image::Luma<u16>, Vec<u16>>,
    step: u16,
) -> Result<Vec<u8>> {
    use std::io::Write;

    let (w, h) = (depth.width(), depth.height());
    let mut raw = Vec::with_capacity(w as usize * h as usize * 2);
    for row in 0..h {
        let mut prev = 0i32;
        for col in 0..w {
            let q = quantise_depth(depth.get_pixel(col, row).0[0], step);
            raw.extend_from_slice(&((i32::from(q) - prev) as i16).to_le_bytes());
            prev = i32::from(q);
        }
    }
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&raw)?;
    Ok(enc.finish()?)
}

/// Cooperative cancellation.
///
/// A blocking task cannot be killed from outside -- dropping its JoinHandle
/// detaches it, it does not stop it -- so a long render has to check whether
/// anyone is still waiting for it. Checked per output column, which bounds the
/// wasted work to one column rather than one panorama.
#[derive(Clone, Default)]
pub struct Cancel(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn check(&self) -> Result<()> {
        anyhow::ensure!(!self.is_cancelled(), "cancelled");
        Ok(())
    }
}

/// How far a summit rises above the terrain beside it *at its own depth*, in
/// metres.
///
/// The obvious measure -- height above the terrain *behind* the peak -- cannot
/// come from the distance buffer, because terrain behind a peak is exactly
/// what the peak hides. Measuring sideways is both computable and a truer
/// statement of what makes a summit worth labelling: one standing clear of its
/// neighbours reads as a peak, one on a long level ridge does not.
///
/// What a peak must stand clear of is its *neighbours*, not the backdrop, so
/// the profile comes from terrain at a comparable depth: a band around the
/// peak's own distance, wide enough to hold a ridge running obliquely away,
/// narrow enough to drop what stands behind. The skyline is the wrong
/// reference -- a hill with a range behind it is not on it at all.
///
/// Follows topographic prominence in shape: walk out each way to where the
/// ground rises above the summit, and take the higher of the two lowest points
/// found. In metres rather than degrees, because metres stay comparable
/// between a foreground hill and a distant range while an angle does not -- a
/// 2 km hill would otherwise outrank every summit in the Tatras.
///
/// Signed, and so deliberately *not* called prominence, which is non-negative
/// by definition. A top that never rises clear of its ridge returns how far
/// the ridge stands over it. The two halves are one continuous scale: flatten
/// a top until its col reaches the summit and it passes through zero. Zero
/// itself means only that nothing at the peak's depth was found to compare
/// against -- in ridge country most tops score below it, and clamping them
/// together left the near field unorderable.
///
/// Occlusion no longer biases it. The profile records every sample the marcher
/// took, not only the surfaces that won a row, so a col hidden behind nearer
/// ground is still read as the col -- which is what a question about the
/// landscape should do. What remains is sampling: the marcher only knows the
/// bearings it cast rays along, so a coarse render sees a sparser
/// neighbourhood and scores a little higher.
fn dominance_m(
    profile: &Profile,
    cols: usize,
    col: usize,
    elevation: f64,
    distance: f64,
    window: usize,
) -> f64 {
    let radius = DOMINANCE_RADIUS;
    // The neighbourhood is a ball of ground around the summit, so it is as
    // deep as it is wide -- the same radius the column window uses. A ratio
    // instead of a difference looks reasonable and is not: at 25 km it spans
    // 17 to 38 km, sweeping whole other ranges in as "higher ground beside
    // this peak", and every summit in the middle distance scores zero.
    //
    // Held to within a factor of two either way, because near the viewer the
    // ball reaches back past their own feet: for a hill 1.8 km off, a 3 km
    // ball includes the slope they are standing on, which is higher than the
    // hill and settles both cols at once.
    let band = (
        (distance - radius).max(distance * 0.5),
        (distance + radius).min(distance * 2.0),
    );
    let mut key_col = f64::NEG_INFINITY;
    for dir in [-1isize, 1] {
        let mut lowest = f64::INFINITY;
        let mut c = col as isize;
        for _ in 0..window {
            c += dir;
            let Ok(c) = usize::try_from(c) else { break };
            if c >= cols {
                break;
            }
            // Nothing at this depth: the ridge has ended and we are seeing
            // past it. Not a col -- keep walking, in case it resumes.
            let Some(s) = profile.max_between(c, band.0, band.1) else {
                continue;
            };
            // Recorded before the break, so a side that rises immediately
            // still contributes. `lowest` is a minimum, so this only ever
            // matters when higher ground was the *first* thing found -- a top
            // inside a massif -- and it is what lets the result go negative
            // instead of tying at zero with every other such top.
            lowest = lowest.min(s);
            if s > elevation {
                break; // higher ground at this depth: this side's col is settled
            }
        }
        if lowest.is_finite() {
            key_col = key_col.max(lowest);
        }
    }
    if key_col.is_finite() {
        elevation - key_col
    } else {
        0.0 // nothing at this peak's depth to compare it against
    }
}

pub fn render(
    root: &Path,
    doc: &Doc,
    p: &Params,
    cancel: &Cancel,
    peaks: &mut [Peak],
    progress: Option<&crate::progress::Job>,
) -> Result<(image::RgbImage, Stats)> {
    let (coarsest, finest) = (doc.grid.coarsest_level, doc.grid.finest_level);
    let (ssx, ssy) = (p.supersample_x.max(1), p.supersample_y.max(1));
    let want_peaks = !peaks.is_empty();

    let out_w = (p.az_span / p.step_deg).round() as usize;
    let out_h = ((p.alt_max - p.alt_min) / p.step_deg).round() as usize;
    // Columns, because the marcher already pauses at each one to check for
    // cancellation. They are not equal work -- a column of sky is cheaper than
    // one of near terrain -- so the percentage drifts a little from the truth
    // and is honest about being a percentage rather than a time.
    if let Some(job) = progress {
        job.set_total(out_w);
    }
    let sub_h = out_h * ssy as usize;
    let az_step = p.step_deg / f64::from(ssx);
    let alt_step = p.step_deg / f64::from(ssy);

    // Eye elevation, and the peak geometry that depends on it, before any
    // marching. Peaks are then answered from the columns the render produces
    // anyway -- resolving them with their own rays cost more than the whole
    // image did.
    let eye = {
        let mut pyr = Pyramid::open(root, doc)?;
        let eye = viewpoint_elevation(&mut pyr, p)? + p.eye_height;
        for pk in peaks.iter_mut() {
            pk.distance = great_circle(p.lon, p.lat, pk.lon, pk.lat);
            pk.azimuth = initial_bearing(p.lon, p.lat, pk.lon, pk.lat);
            let (mx, my) = lonlat_to_merc(pk.lon, pk.lat);

            // At the level the marcher uses for that distance, not the finest.
            // Correctness first: a summit read at z14 but compared against
            // terrain the ray sampled at z9 looks artificially sharp and can
            // report as visible when it is not. It is also far cheaper -- tens
            // of thousands of scattered peaks each pull their own z14 block,
            // where coarse levels cover enough ground to share them.
            let z = level_for(pk.distance, p.lat, coarsest, finest);
            pk.ele = pyr.sample(z, mx, my);

            let h = pk.ele.unwrap_or(f64::NEG_INFINITY);
            let drop = curvature_drop(pk.distance);
            pk.altitude = ((h - eye - drop) / pk.distance).atan().to_degrees();

            let off = (pk.azimuth - p.az_start).rem_euclid(360.0);
            pk.x = off / p.step_deg;
            // Lifted by the same rule the marcher lifts its samples by, or the
            // labels float free of the summits they name. `altitude` itself
            // stays the true elevation angle -- it is a fact about the
            // landscape, where `y` is a position in a picture.
            pk.y = (p.alt_max - (pk.altitude + p.lift_at(pk.distance))) / p.step_deg;
            // Sub-column, not output column, and bounded against the ray count
            // rather than az_span: out_w rounds, so the image can cover
            // slightly more or less than the requested fov, and only the rays
            // themselves say which bearings will actually be answered. Also
            // guarantees pk.x < out_w.
            let sub = (off / az_step).floor();
            let sub_cols = (out_w * ssx as usize) as f64;
            pk.column = (sub >= 0.0 && sub < sub_cols && pk.ele.is_some()).then_some(sub as usize);
        }
        eye
    };

    // Each peak is answered by the two rays that *bracket* its bearing, not by
    // the one whose cell it happens to fall in. A single ray sits up to half a
    // ray-spacing off the true bearing -- 210 m of ground at 60 km on the
    // coarse tier -- and tests a line of sight that is not the peak's. Both
    // rays are marched anyway, so sandwiching the bearing and interpolating
    // between their horizons costs one extra bucket entry and a lerp.
    //
    // Ray k looks along sub-column centre k + 0.5, so a peak at fractional
    // position u lies between rays floor(u - 0.5) and that plus one.
    let sub_cols = out_w * ssx as usize;
    let mut by_sub: HashMap<usize, Vec<(f64, usize, u8)>> = HashMap::new();
    // Interpolation weight between the two bracketing rays, per peak.
    let mut blend = vec![0.0f64; peaks.len()];
    // A full circle has no edge: the ray before the first is the last one.
    // Without this a peak in the first or last half-ray falls back to a single
    // ray, reintroducing the very bearing error this bracketing removes, and
    // doing it at the seam of the default render.
    let wraps = (p.az_span - 360.0).abs() < 1e-9;
    for (i, pk) in peaks.iter().enumerate() {
        if pk.column.is_none() {
            continue;
        }
        // pk.x is fractional output columns; sub-columns are ssx times finer.
        let (rays, t) = bracketing(pk.x * f64::from(ssx), sub_cols, wraps);
        blend[i] = t;
        for (slot, ray) in rays.iter().enumerate() {
            if let Some(k) = ray {
                by_sub
                    .entry(*k)
                    .or_default()
                    .push((pk.distance, i, slot as u8));
            }
        }
    }
    // The marcher answers probes in the order it reaches them.
    for v in by_sub.values_mut() {
        v.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    }
    let by_sub = Arc::new(by_sub);

    let threads = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    let chunk = out_w.div_ceil(threads).max(1);

    // Chunked so each worker opens the pyramid once and keeps its block cache
    // warm across the columns it owns.
    /// One worker's slice of the image. Named rather than a tuple because
    /// three of its fields are consecutive counters: as positional elements,
    /// swapping two of them compiles and silently reports wrong stats.
    struct ChunkOut {
        start: usize,
        pixels: Vec<[u8; 3]>,
        depth: Vec<u16>,
        samples: usize,
        blocks: usize,
        sky: usize,
        /// Peak index, which bracketing ray this was, and that ray's horizon
        /// angle at the peak's distance.
        answers: Vec<(usize, u8, Horizon)>,
        profile: Profile,
    }
    let parts: Vec<ChunkOut> = (0..out_w)
        .step_by(chunk)
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|start| -> Result<_> {
            let cols = chunk.min(out_w - start);
            let mut pyr = Pyramid::open(root, doc)?;
            let mut pixels = vec![[0u8; 3]; cols * out_h];
            let mut depth = vec![0u16; cols * out_h];
            let mut peak_results: Vec<(usize, u8, Horizon)> = Vec::new();
            let mut probe_d: Vec<f64> = Vec::new();
            let mut probe_h: Vec<Horizon> = Vec::new();
            // Only dominance reads it, so a plain image render neither
            // allocates it nor pays a bin lookup per marched sample.
            let mut profile = Profile::new(if want_peaks { cols } else { 0 });
            let mut samples = 0usize;
            let mut sky = 0usize;

            let mut column = Column::new(sub_h);
            let mut shaded: Vec<Vec<(f64, f64, f64)>> = Vec::with_capacity(ssx as usize);
            let mut nearest = vec![f64::INFINITY; out_h];

            for local in 0..cols {
                cancel.check()?;
                if let Some(job) = progress {
                    job.tick();
                }
                let oc = start + local;
                shaded.clear();
                nearest.fill(f64::INFINITY);
                for k in 0..ssx {
                    column.dist.fill(f64::INFINITY);
                    column.cover.fill(0.0);
                    let sub = oc * ssx as usize + k as usize;
                    let az = p.az_start + (sub as f64 + 0.5) * az_step;

                    // Peaks this ray brackets, and where along it they sit.
                    let probes = by_sub.get(&sub).map_or(&[][..], Vec::as_slice);
                    probe_d.clear();
                    probe_d.extend(probes.iter().map(|&(d, _, _)| d));

                    // One profile per output column: the ssx rays inside it
                    // all contribute, so a narrow gully seen by one ray is not
                    // lost when its neighbour misses it.
                    samples += march_ray(
                        &mut pyr, p, eye, az, alt_step, sub_h, coarsest, finest, &mut column,
                        &probe_d, &mut probe_h, want_peaks.then_some((&mut profile, local)),
                    );
                    sky += column.dist.iter().filter(|d| d.is_infinite()).count();

                    for (&(_, i, slot), &h) in probes.iter().zip(probe_h.iter()) {
                        peak_results.push((i, slot, h));
                    }

                    // Depth takes the nearest sub-sample rather than the mean:
                    // averaging across a silhouette would report a distance at
                    // which there is no terrain at all.
                    for (orow, near) in nearest.iter_mut().enumerate() {
                        for sy in 0..ssy as usize {
                            let d = column.dist[orow * ssy as usize + sy];
                            if d < *near {
                                *near = d;
                            }
                        }
                    }
                    shaded.push(shade_column(&column, p, alt_step, sub_h));
                }
                for (orow, near) in nearest.iter().enumerate() {
                    depth[orow * cols + local] = encode_depth(*near);
                }

                // Box-average the ssx x ssy block behind each output pixel.
                let n = f64::from(ssx * ssy);
                for orow in 0..out_h {
                    let (mut r, mut g, mut b) = (0.0, 0.0, 0.0);
                    for sc in &shaded {
                        for sy in 0..ssy as usize {
                            let (pr, pg, pb) = sc[orow * ssy as usize + sy];
                            r += pr;
                            g += pg;
                            b += pb;
                        }
                    }
                    // Dither by absolute position, not by position within the
                    // chunk, or the pattern restarts at every worker boundary.
                    let (dx, dy) = (start + local, orow);
                    let ds = p.dither_strength;
                    pixels[orow * cols + local] = [
                        clamp(r / n, dx, dy, ds),
                        clamp(g / n, dx, dy, ds),
                        clamp(b / n, dx, dy, ds),
                    ];
                }
            }

            Ok(ChunkOut {
                start,
                pixels,
                depth,
                samples,
                blocks: pyr.cached_blocks(),
                sky,
                answers: peak_results,
                profile,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let mut img = image::RgbImage::new(out_w as u32, out_h as u32);
    let mut depth_img =
        image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::new(out_w as u32, out_h as u32);
    let mut samples = 0usize;
    let mut blocks = 0usize;
    let mut sky = 0usize;
    let mut cells = 0usize;
    // Horizon angle from each bracketing ray, per peak.
    let nowhere = Horizon {
        drawn: f64::NAN,
        real: f64::NAN,
    };
    let mut horizon = vec![[nowhere; 2]; peaks.len()];
    let mut profile = Profile::new(if want_peaks { out_w } else { 0 });
    for part in parts {
        let ChunkOut {
            start,
            pixels,
            depth,
            ..
        } = &part;
        samples += part.samples;
        blocks += part.blocks;
        sky += part.sky;
        for &(i, slot, h) in &part.answers {
            horizon[i][slot as usize] = h;
        }
        let cols = pixels.len() / out_h;
        if want_peaks {
            profile.max_h[start * PROFILE_BINS..(start + cols) * PROFILE_BINS]
                .copy_from_slice(&part.profile.max_h);
        }
        cells += cols * sub_h * ssx as usize;
        for orow in 0..out_h {
            for local in 0..cols {
                let x = (start + local) as u32;
                img.put_pixel(x, orow as u32, image::Rgb(pixels[orow * cols + local]));
                depth_img.put_pixel(
                    x,
                    orow as u32,
                    image::Luma([depth[orow * cols + local]]),
                );
            }
        }
    }

    // A summit is visible when it stands above the horizon along the line of
    // sight to it. The two bracketing rays give the horizon either side of
    // that bearing; interpolating between them lands on the bearing itself,
    // and where a peak sits at the frame edge only one ray exists to use.
    //
    // Being in frame is a separate question, and `peaks::select` asks it.
    for (i, pk) in peaks.iter_mut().enumerate() {
        let [h0, h1] = horizon[i];
        let t = blend[i];
        // Against the same angle the marcher occluded by, so a peak is kept
        // exactly when the render drew its summit.
        let lifted = pk.altitude + p.lift_at(pk.distance);
        pk.visible = clears(lifted, h0.drawn, h1.drawn, t);
        // Asked of the true geometry, so it answers the question a reader of
        // the picture would ask: is that summit actually in sight from here,
        // or has the drawing lifted it out from behind something? Always false
        // without a lift, since the two horizons are then the same.
        pk.revealed = pk.visible && !clears(pk.altitude, h0.real, h1.real, t);
    }

    // Dominance reads across columns, so it waits until they are merged.
    // Bounded by ground distance rather than by angle: the question is whether
    // a summit dominates its surroundings, and a fixed angle would ask that
    // over 170 m of ground for a near hill and 12 km for a far one. Clamped
    // because at close range the equivalent angle runs to tens of degrees.
    // Parallel like the marching it follows. Peaks are independent -- each
    // writes only its own dominance and reads only the shared profile -- and the
    // 20-degree window applies to everything within 8 km, so a viewpoint
    // ringed by close summits can otherwise spend longer here, on one core,
    // than the whole render took on all of them. It holds the render permit
    // throughout, so its cost is charged to every queued request, not just
    // this one.
    peaks.par_iter_mut().try_for_each(|pk| -> Result<()> {
        cancel.check()?;
        pk.dominance = match (pk.column, pk.ele) {
            (Some(column), Some(ele)) if pk.visible => {
                let out_col = (column / ssx as usize).min(out_w.saturating_sub(1));
                let span = (DOMINANCE_RADIUS / pk.distance)
                    .atan()
                    .to_degrees()
                    .clamp(1.0, 20.0);
                let window = (span / p.step_deg).round() as usize;
                dominance_m(&profile, out_w, out_col, ele, pk.distance, window)
            }
            _ => 0.0,
        };
        Ok(())
    })?;

    Ok((
        img,
        Stats {
            width: out_w,
            height: out_h,
            eye_elevation: eye,
            samples,
            blocks,
            sky_fraction: sky as f64 / cells.max(1) as f64,
            depth: depth_img,
        },
    ))
}

/// Kept in floating point all the way to the final quantisation. Rounding to
/// bytes here and then blending haze against the result banded the sky twice
/// over -- and `as u8` truncates rather than rounds, widening every band by a
/// level.
fn sky_colour(alt: f64) -> (f64, f64, f64) {
    // Deeper blue overhead, pale towards the horizon.
    let t = (alt / 30.0).clamp(0.0, 1.0);
    (
        lerp(196.0, 110.0, t),
        lerp(216.0, 156.0, t),
        lerp(238.0, 214.0, t),
    )
}

/// Triangular dither of one level either way, from a hash of the position.
///
/// Scaled by `Params::dither_strength` at the point of use, so the amplitude
/// that actually ships is `DEFAULT_DITHER` levels, not one -- see there for
/// why one is not enough on a gradient this slow.
///
/// A sky gradient crosses few levels over many pixels -- blue runs 238 to 214
/// across the whole frame, so one level lasts 25 rows at the default step and
/// the eye reads the steps as stripes.
///
/// Two properties matter, and an ordered matrix at half a level gives only the
/// first. Zero mean removes the banding's *position*; a triangular
/// distribution one level wide removes its *visibility*, because the
/// proportion of pixels that round up then varies smoothly with the value
/// instead of switching on and off as the ramp crosses each boundary. That
/// residual switching is noise modulation, and it is what a Bayer dither
/// leaves behind as softened steps rather than no steps.
///
/// Hashed rather than tiled because an 8x8 matrix repeats often enough for the
/// eye to find the grid and read it as structure. Deterministic all the same,
/// so a given pixel always dithers identically and renders stay reproducible.
fn dither(x: usize, y: usize) -> f64 {
    // Interleaved gradient noise: cheap, and its energy sits at high spatial
    // frequencies where the eye is least sensitive. A plain hash is white
    // noise, which carries just as much energy at low frequencies -- and
    // low-frequency noise laid over a slow gradient reads as soft mottling,
    // which is banding by another name.
    fn ign(x: f64, y: f64) -> f64 {
        let v = 0.067_110_56 * x + 0.005_837_15 * y;
        (52.982_918_9 * v.fract()).fract()
    }
    // Differenced against a second sample to make it triangular, which is what
    // stops the visible dither strength pulsing as the ramp crosses each level
    // boundary. The axes are swapped and rescaled rather than offset: IGN is a
    // function of one linear combination of x and y, so shifting both merely
    // slides the same noise and the difference collapses towards zero.
    let (x, y) = (x as f64, y as f64);
    ign(x, y) - ign(y * 1.7 + 11.0, x * 0.9 + 23.0)
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

/// Parse `#rrggbb`, or `rrggbb`, or the three-digit short form.
///
/// Hex because the caller is a web client and this is what it already has;
/// the renderer works in floating point, so the value is widened once here
/// rather than being quantised on the way in.
pub fn parse_colour(s: &str) -> Result<(f64, f64, f64)> {
    let h = s.trim().trim_start_matches('#');
    let digits: Vec<u32> = h
        .chars()
        .map(|c| c.to_digit(16).with_context(|| format!("bad colour {s:?}")))
        .collect::<Result<_>>()?;
    let ch = |hi: u32, lo: u32| f64::from(hi * 16 + lo);
    match digits[..] {
        [r, g, b] => Ok((ch(r, r), ch(g, g), ch(b, b))),
        [r0, r1, g0, g1, b0, b1] => Ok((ch(r0, r1), ch(g0, g1), ch(b0, b1))),
        _ => anyhow::bail!("colour {s:?} wants 3 or 6 hex digits"),
    }
}

/// Widest stroke the renderer will draw, output pixels.
///
/// Unlike `ridge_strength`, width has a cost: every stroke inks a band of
/// rows, so an unbounded value would have one request painting the full
/// column height for every edge in the frame.
pub const MAX_RIDGE_WIDTH: f64 = 20.0;

/// Most the far horizon may be lifted by, degrees.
///
/// Well past useful -- a few degrees already reads as a strong effect against
/// a thirty-degree frame -- and it is a bound rather than a taste: the band
/// fill assumes drawn angles rise with distance, which holds for any lift that
/// does not fall, and the frame has to stay somewhere near ninety degrees for
/// the row arithmetic to mean anything.
pub const MAX_DEPTH_LIFT: f64 = 45.0;

/// Reject styling the renderer cannot draw, before it reaches the hot loop.
///
/// Shared so the CLI cannot do what the server refuses: `--ridge-width 1e9`
/// used to make the ink pass span the whole column for every edge, which reads
/// as a hang and paints a solid slab, while the same value over HTTP was a
/// clean 400.
pub fn validate_style(ridge_strength: f64, ridge_width: f64, depth_lift: f64) -> Result<()> {
    for (name, v) in [
        ("ridge_strength", ridge_strength),
        ("ridge_width", ridge_width),
        ("depth_lift", depth_lift),
    ] {
        anyhow::ensure!(v.is_finite(), "{name} must be a finite number");
    }
    anyhow::ensure!(
        ridge_strength >= 0.0,
        "ridge_strength must not be negative"
    );
    anyhow::ensure!(
        (0.0..=MAX_RIDGE_WIDTH).contains(&ridge_width),
        "ridge_width must lie within 0..{MAX_RIDGE_WIDTH}"
    );
    // Negative is refused rather than clamped because it is not merely
    // useless: a lift that shrinks with distance can place a farther sample
    // *below* the nearer one it stands over, and the band fill -- which walks
    // the frame upwards and never revisits a row -- would drop it, leaving
    // sky where terrain is.
    anyhow::ensure!(
        (0.0..=MAX_DEPTH_LIFT).contains(&depth_lift),
        "depth_lift must lie within 0..{MAX_DEPTH_LIFT}"
    );
    Ok(())
}

/// How much dither the 8-bit output needs, in levels either way.
///
/// The textbook amount is 1 and it is not enough here: this sky crosses one
/// level per twenty rows, and where the true value sits near a whole number a
/// one-level dither almost never flips it. The picture then alternates between
/// flat stretches and dithered ones, which is what banding looks like once you
/// have half-fixed it. Measured on a real sky, the longest flat run is 25 px
/// undithered, 18 at 1, 3 at 1.5 and 2 at 2 -- while the encoded size runs
/// 216 KB, 292 KB and 760 KB, since noise is precisely what compresses worst.
/// 1.5 is where the curve turns.
pub const DEFAULT_DITHER: f64 = 1.5;

/// Terrain as it renders without haze: a muted green.
pub const DEFAULT_GROUND: (f64, f64, f64) = (58.0, 74.0, 52.0);
/// Silhouettes darken what they cross, which is a multiply towards black.
pub const DEFAULT_RIDGE: (f64, f64, f64) = (0.0, 0.0, 0.0);

/// Quantise to a byte, dithered by position and rounded rather than truncated.
fn clamp(v: f64, x: usize, y: usize, strength: f64) -> u8 {
    (v + dither(x, y) * strength).clamp(0.0, 255.0).round() as u8
}

/// Degrees of the compass, for the summary line.
pub fn compass(az: f64) -> &'static str {
    const NAMES: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    NAMES[((az.rem_euclid(360.0) / 45.0).round() as usize) % 8]
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- depth codec ------------------------------------------------------
    //
    // 0 means sky and only sky. Ground was floored onto that sentinel twice,
    // in two separately written emitters, and each time it surfaced as holes
    // in the bottom of the client's depth buffer rather than as an error.

    #[test]
    fn sky_is_the_only_zero() {
        assert_eq!(encode_depth(f64::INFINITY), 0);
        assert_eq!(encode_depth(f64::NAN), 0);
        // Ground nearer than the encodable range saturates rather than
        // falling through to the sentinel.
        for d in [0.0, 0.5, 5.0, DEPTH_NEAR] {
            assert_eq!(encode_depth(d), 1, "d = {d}");
        }
        assert_eq!(encode_depth(DEPTH_FAR), u16::MAX);
        assert_eq!(encode_depth(DEPTH_FAR * 10.0), u16::MAX);
    }

    #[test]
    fn quantising_never_reaches_the_sentinel() {
        for step in [1u16, 2, 3, 4, 7, 16, 64, 255, 4096] {
            assert_eq!(quantise_depth(0, step), 0, "sky must stay sky");
            for v in [1u16, 2, 3, 4, 5, 100, 30_000, u16::MAX] {
                let q = quantise_depth(v, step);
                assert_ne!(q, 0, "v = {v} step = {step} landed on the sky sentinel");
                assert!(q <= v, "v = {v} step = {step} quantised upward to {q}");
                assert!(
                    u32::from(v) - u32::from(q) < u32::from(step),
                    "v = {v} step = {step} lost more than one step"
                );
            }
        }
    }

    #[test]
    fn encoding_is_monotone_in_distance() {
        let mut prev = 0u16;
        let mut d = DEPTH_NEAR;
        while d < DEPTH_FAR {
            let v = encode_depth(d);
            assert!(v >= prev, "encoding went backwards at {d}");
            prev = v;
            d *= 1.05;
        }
    }

    /// The wire format the client decodes: quantise, delta along each row,
    /// gzip. Deltas are signed 16-bit over unsigned values, so the reader has
    /// to accumulate modulo 65536 -- this pins that contract.
    #[test]
    fn depth_bytes_round_trip() {
        use std::io::Read;

        let (w, h) = (7u32, 3u32);
        let mut img = image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::new(w, h);
        // Sky next to near ground, and a jump big enough to wrap an i16.
        let vals = [
            [0u16, 1, 2, 3, 65_535, 0, 40_000],
            [0, 0, 0, 0, 0, 0, 0],
            [65_535, 1, 65_535, 1, 12_345, 6, 7],
        ];
        for y in 0..h {
            for x in 0..w {
                img.put_pixel(x, y, image::Luma([vals[y as usize][x as usize]]));
            }
        }

        for step in [1u16, 4, 97] {
            let gz = depth_bytes(&img, step).unwrap();
            let mut raw = Vec::new();
            flate2::read::GzDecoder::new(&gz[..])
                .read_to_end(&mut raw)
                .unwrap();
            assert_eq!(raw.len(), (w * h) as usize * 2);

            let mut i = 0;
            for row in vals.iter() {
                let mut acc = 0i32;
                for &want in row.iter() {
                    let delta = i16::from_le_bytes([raw[i], raw[i + 1]]);
                    i += 2;
                    acc += i32::from(delta);
                    let got = (acc as u32 & 0xffff) as u16;
                    assert_eq!(got, quantise_depth(want, step), "step {step}, value {want}");
                }
            }
        }
    }

    // ---- elevation profile ------------------------------------------------

    #[test]
    fn bins_are_monotone_and_clamped() {
        assert_eq!(Profile::bin_of(0.0), 0);
        assert_eq!(Profile::bin_of(DEPTH_NEAR), 0);
        assert_eq!(Profile::bin_of(DEPTH_FAR), PROFILE_BINS - 1);
        assert_eq!(Profile::bin_of(f64::MAX), PROFILE_BINS - 1);
        let mut prev = 0;
        let mut d = 1.0;
        while d < DEPTH_FAR * 2.0 {
            let b = Profile::bin_of(d);
            assert!(b >= prev && b < PROFILE_BINS, "bin went backwards at {d}");
            prev = b;
            d *= 1.01;
        }
    }

    /// The marcher walks bins forward with a cursor instead of taking a
    /// logarithm per sample. That shortcut is only sound while it agrees with
    /// the direct computation for every distance a ray visits.
    #[test]
    fn walked_bins_match_computed_bins() {
        let edges = Profile::edges();
        let mut profile = Profile::new(1);
        let mut cursor = 0usize;
        let mut d = DEPTH_NEAR;
        while d < DEPTH_FAR {
            profile.record(0, &mut cursor, &edges, d, 0.0);
            assert_eq!(cursor, Profile::bin_of(d), "cursor drifted at d = {d}");
            d *= 1.003;
        }
    }

    #[test]
    fn profile_reports_the_highest_ground_in_a_band() {
        let edges = Profile::edges();
        let mut profile = Profile::new(2);
        let mut cursor = 0usize;
        for (d, h) in [(1_000.0, 500.0), (5_000.0, 900.0), (50_000.0, 1_500.0)] {
            profile.record(0, &mut cursor, &edges, d, h);
        }
        assert_eq!(profile.max_between(0, 900.0, 1_100.0), Some(500.0));
        assert_eq!(profile.max_between(0, 900.0, 6_000.0), Some(900.0));
        assert_eq!(profile.max_between(0, 900.0, 60_000.0), Some(1_500.0));
        // Nothing recorded there, and nothing recorded in that column at all.
        assert_eq!(profile.max_between(0, 100_000.0, 200_000.0), None);
        assert_eq!(profile.max_between(1, 900.0, 60_000.0), None);
    }

    /// `max_between` slices `a..=b` and panics if the band comes out
    /// backwards. `dominance_m` builds that band from a distance, so no
    /// distance may produce one.
    #[test]
    fn dominance_bands_are_never_inverted() {
        let profile = Profile::new(1);
        let mut d = 0.001;
        while d < DEPTH_FAR * 4.0 {
            let near = (d - DOMINANCE_RADIUS).max(d * 0.5);
            let far = (d + DOMINANCE_RADIUS).min(d * 2.0);
            assert!(near <= far, "band inverted at d = {d}: {near}..{far}");
            // Must not panic.
            let _ = profile.max_between(0, near, far);
            d *= 1.07;
        }
    }

    // ---- dominance --------------------------------------------------------

    /// Build a profile where every column holds one summit-height reading at
    /// the same distance, so the walk sees a pure skyline of elevations.
    fn ridge(heights: &[f64], d: f64) -> Profile {
        let edges = Profile::edges();
        let mut profile = Profile::new(heights.len());
        for (c, &h) in heights.iter().enumerate() {
            let mut cursor = 0usize;
            profile.record(c, &mut cursor, &edges, d, h);
        }
        profile
    }

    #[test]
    fn a_summit_standing_clear_scores_its_height_above_the_cols() {
        let d = 10_000.0;
        let p = ridge(&[500.0, 600.0, 1_000.0, 600.0, 500.0], d);
        assert_eq!(dominance_m(&p, 5, 2, 1_000.0, d, 5), 500.0);
    }

    /// A top its own ridge stands over scores negative -- the whole near field
    /// used to clamp to zero here and become unorderable.
    ///
    /// The score is set by the nearest higher ground on each side, taking the
    /// higher of the two (1300, not the 1500 further along), because that is
    /// the col you would have to cross.
    #[test]
    fn a_top_inside_a_massif_scores_negative() {
        let d = 10_000.0;
        let p = ridge(&[1_400.0, 1_200.0, 1_000.0, 1_300.0, 1_500.0], d);
        assert_eq!(dominance_m(&p, 5, 2, 1_000.0, d, 5), -300.0);
    }

    /// The two halves are one continuous scale: lower a summit past its
    /// neighbours and the score passes through zero rather than jumping.
    ///
    /// The ridge rises again at both ends, which is what bounds the search --
    /// without that the walk runs to the window edge and finds the valley
    /// floor, which is a different and much larger number.
    #[test]
    fn dominance_is_continuous_through_zero() {
        let d = 10_000.0;
        for (summit, want) in [(1_150.0, 50.0), (1_100.0, 0.0), (1_050.0, -50.0)] {
            let p = ridge(&[1_200.0, 1_100.0, summit, 1_100.0, 1_200.0], d);
            assert_eq!(dominance_m(&p, 5, 2, summit, d, 5), want, "summit {summit}");
        }
    }

    #[test]
    fn nothing_at_the_peaks_depth_scores_zero() {
        let p = ridge(&[500.0, 600.0, 1_000.0, 600.0, 500.0], 10_000.0);
        // Same columns, but asked about a depth where nothing was recorded.
        assert_eq!(dominance_m(&p, 5, 2, 1_000.0, 200_000.0, 5), 0.0);
    }

    // ---- bracketing rays --------------------------------------------------

    #[test]
    fn a_bearing_is_bracketed_by_the_rays_either_side() {
        // Dead on ray 3's centre: both slots are ray 3's neighbours at t = 0.
        let (rays, t) = bracketing(3.5, 10, false);
        assert_eq!(rays, [Some(3), Some(4)]);
        assert!((t - 0.0).abs() < 1e-12);
        // Halfway between rays 3 and 4.
        let (rays, t) = bracketing(4.0, 10, false);
        assert_eq!(rays, [Some(3), Some(4)]);
        assert!((t - 0.5).abs() < 1e-12);
    }

    /// A full circle has no edge. Without wrapping, a peak in the first or
    /// last half-ray falls back to a single ray and takes back the bearing
    /// error the bracketing exists to remove -- at the seam of the default
    /// 360-degree render.
    #[test]
    fn a_full_circle_wraps_at_the_seam() {
        let (rays, t) = bracketing(0.25, 8, true);
        assert_eq!(rays, [Some(7), Some(0)]);
        assert!((t - 0.75).abs() < 1e-12);

        let (rays, _) = bracketing(7.9, 8, true);
        assert_eq!(rays, [Some(7), Some(0)]);
    }

    #[test]
    fn a_narrow_view_drops_rays_past_its_edges() {
        assert_eq!(bracketing(0.25, 8, false).0, [None, Some(0)]);
        assert_eq!(bracketing(7.9, 8, false).0, [Some(7), None]);
    }

    // ---- dithering --------------------------------------------------------

    /// Dither must not shift the picture, only spread each step's boundary.
    #[test]
    fn dither_has_no_bias() {
        let vals: Vec<f64> = (0..200)
            .flat_map(|y| (0..200).map(move |x| dither(x, y)))
            .collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        assert!(mean.abs() < 0.005, "dither is biased by {mean}");
        assert!(
            vals.iter().all(|d| d.abs() <= 1.0),
            "dither exceeds one level"
        );
        // Triangular, not uniform: values bunch towards zero, so the middle
        // half of the range holds well over half the samples.
        let near = vals.iter().filter(|d| d.abs() < 0.5).count();
        let frac = near as f64 / vals.len() as f64;
        assert!((0.7..0.8).contains(&frac), "not triangular: {frac} within +-0.5");
    }

    /// Deterministic: the same pixel must dither the same way every render,
    /// or repeat requests would differ and caching would be unsound.
    #[test]
    fn dither_is_reproducible() {
        for (x, y) in [(0, 0), (7, 3), (1024, 768), (12_345, 54_321)] {
            assert_eq!(dither(x, y), dither(x, y));
        }
    }

    /// A gradient crossing one level over many pixels must not step all at
    /// once. Blue spans 24 levels across the whole sky, so a run of a single
    /// value tens of rows long is exactly what banded the picture.
    #[test]
    fn a_slow_gradient_does_not_band() {
        // One level over 32 rows, the shallowest the sky ever gets. Undithered
        // that is a flat run of 32; at the textbook strength of 1 it is still
        // 7, which on a real sky reads as the alternating flat-then-dithered
        // look it had. The default is higher for that reason, and this pins
        // it -- it drops to 3.
        let value = |row: usize| 238.0 - row as f64 / 32.0;
        let column: Vec<u8> = (0..64)
            .map(|row| clamp(value(row), 0, row, DEFAULT_DITHER))
            .collect();
        let longest = column
            .chunk_by(|a, b| a == b)
            .map(<[u8]>::len)
            .max()
            .unwrap();
        assert!(longest <= 6, "a flat run of {longest} rows is a visible band");
        // And it still tracks the underlying ramp.
        let mean = column.iter().map(|&v| f64::from(v)).sum::<f64>() / 64.0;
        let want = (0..64).map(value).sum::<f64>() / 64.0;
        assert!((mean - want).abs() < 0.5, "mean drifted: {mean} vs {want}");
    }

    // ---- colours ----------------------------------------------------------

    #[test]
    fn colours_parse_in_the_forms_a_web_client_has() {
        assert_eq!(parse_colour("#3a4a34").unwrap(), (58.0, 74.0, 52.0));
        assert_eq!(parse_colour("3a4a34").unwrap(), (58.0, 74.0, 52.0));
        assert_eq!(parse_colour("  #FFF  ").unwrap(), (255.0, 255.0, 255.0));
        assert_eq!(parse_colour("#000").unwrap(), (0.0, 0.0, 0.0));
        for bad in ["", "#12", "#12345", "#gg0000", "#1234567"] {
            assert!(parse_colour(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    /// The defaults have to be exactly the colours that were hard-coded, or
    /// making them configurable would quietly restyle every existing client.
    #[test]
    fn defaults_match_the_previous_hard_coded_colours() {
        assert_eq!(DEFAULT_GROUND, (58.0, 74.0, 52.0));
        assert_eq!(DEFAULT_RIDGE, (0.0, 0.0, 0.0));
        // Inking towards black is the multiply the old code did.
        for k in [0.0, 0.25, 1.0] {
            assert_eq!(lerp(200.0, DEFAULT_RIDGE.0, k), 200.0 * (1.0 - k));
        }
    }

    // ---- geometry ---------------------------------------------------------

    #[test]
    fn curvature_drop_matches_the_textbook_figure() {
        assert_eq!(curvature_drop(0.0), 0.0);
        // d^2 (1 - k) / 2R at 10 km, k = 0.13.
        assert!((curvature_drop(10_000.0) - 6.83).abs() < 0.01);
        // Quadratic: four times the distance drops sixteen times as far.
        assert!((curvature_drop(40_000.0) / curvature_drop(10_000.0) - 16.0).abs() < 1e-9);
    }

    #[test]
    fn great_circle_and_bearing_agree_with_known_values() {
        // Krompachy to Gerlachovsky stit: ~63.5 km on a bearing near 300.
        let d = great_circle(20.888781, 48.878479, 20.133, 49.164);
        assert!((d - 63_500.0).abs() < 1_500.0, "got {d} m");
        let az = initial_bearing(20.888781, 48.878479, 20.133, 49.164);
        assert!((az - 300.0).abs() < 3.0, "got {az} degrees");
        // Due north and due east from anywhere sensible.
        assert!((initial_bearing(20.0, 49.0, 20.0, 50.0) - 0.0).abs() < 1e-9);
        assert!((initial_bearing(20.0, 49.0, 21.0, 49.0) - 90.0).abs() < 0.5);
    }

    #[test]
    fn level_selection_never_asks_for_more_detail_than_exists() {
        for d in [10.0, 500.0, 7_000.0, 20_000.0, 100_000.0, 300_000.0] {
            let z = level_for(d, 49.0, 8, 14);
            assert!((8..=14).contains(&z), "d = {d} chose z{z}");
        }
        // Coarser with distance, never finer.
        let mut prev = 14;
        let mut d = 100.0;
        while d < 300_000.0 {
            let z = level_for(d, 49.0, 8, 14);
            assert!(z <= prev, "level went finer at {d}");
            prev = z;
            d *= 1.2;
        }
    }

    // ---- depth lift -------------------------------------------------------

    /// The band fill walks the frame upwards and never revisits a row, so a
    /// sample accepted after another must also *draw* above it. Occlusion
    /// guarantees the true angle rises; this asserts the lift cannot undo
    /// that, which is the whole reason a falling lift is refused.
    #[test]
    fn lift_preserves_the_order_the_band_fill_needs() {
        let max_range = 300_000.0;
        for lift in [0.0, 0.5, 3.0, MAX_DEPTH_LIFT] {
            let per_m = lift / max_range;
            // A ray that keeps just barely clearing its own horizon: the
            // hardest case, since a lift only has to survive ties.
            let mut prev = f64::NEG_INFINITY;
            let mut d = 10.0;
            let mut alpha = -5.0;
            while d < max_range {
                let drawn = alpha + per_m * d;
                assert!(
                    drawn > prev,
                    "lift {lift} put d = {d} at {drawn}, below {prev}"
                );
                prev = drawn;
                alpha += 1e-9;
                d *= 1.05;
            }
        }
    }

    /// Zero must be the true projection, bit for bit -- the default has to
    /// leave every existing render untouched.
    #[test]
    fn zero_lift_moves_nothing() {
        let per_m = 0.0 / 300_000.0;
        for d in [10.0, 5_000.0, 120_000.0, 300_000.0] {
            assert_eq!(3.25 + per_m * d, 3.25, "zero lift moved d = {d}");
        }
    }

    /// The horizon rises by exactly `depth_lift` and the eye by nothing, which
    /// is what the caller is told when deciding how much `alt_max` to add.
    #[test]
    fn lift_spans_zero_to_the_full_amount() {
        let (lift, max_range): (f64, f64) = (4.0, 250_000.0);
        let per_m = lift / max_range;
        assert!((per_m * max_range - lift).abs() < 1e-12);
        assert!(per_m * 0.0 == 0.0);
        // Linear, so the halfway distance gets exactly half.
        assert!((per_m * (max_range / 2.0) - lift / 2.0).abs() < 1e-12);
    }

    /// Three states per side, and the reason `clears` exists is that the
    /// visible and revealed answers must not spell them out twice.
    #[test]
    fn a_missing_ray_is_not_a_low_horizon() {
        let n = f64::NEG_INFINITY;
        // No coverage either side: nothing was found to block anything.
        assert!(clears(-9.0, n, n, 0.5));
        // Out of frame entirely: no ray answered, so nothing is visible.
        assert!(!clears(90.0, f64::NAN, f64::NAN, 0.5));
        // One side has coverage; it decides alone rather than being averaged
        // against a hole.
        assert!(!clears(1.0, 5.0, n, 0.5));
        assert!(clears(6.0, 5.0, n, 0.5));
        // Two real horizons interpolate.
        assert!(clears(3.1, 2.0, 4.0, 0.5));
        assert!(!clears(2.9, 2.0, 4.0, 0.5));
    }

    /// A falling lift would leave sky where terrain is; it has to be refused
    /// rather than clamped, and both front-ends share the rule.
    #[test]
    fn a_lift_that_falls_with_distance_is_refused() {
        assert!(validate_style(1.0, 1.0, 0.0).is_ok());
        assert!(validate_style(1.0, 1.0, MAX_DEPTH_LIFT).is_ok());
        assert!(validate_style(1.0, 1.0, -0.1).is_err());
        assert!(validate_style(1.0, 1.0, MAX_DEPTH_LIFT + 0.1).is_err());
        assert!(validate_style(1.0, 1.0, f64::NAN).is_err());
        assert!(validate_style(1.0, 1.0, f64::INFINITY).is_err());
    }
}
