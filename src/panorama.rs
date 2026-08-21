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
}

/// One pyramid level, with a block cache over its GTI index.
struct Level {
    z: u32,
    ds: Dataset,
    gt: [f64; 6],
    w: usize,
    h: usize,
    cache: HashMap<(i64, i64), Option<Vec<u16>>>,
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
            cache: HashMap::new(),
        }))
    }

    /// Elevation at integer pixel coordinates, or None for nodata / off-grid.
    fn value_at(&mut self, px: usize, py: usize) -> Option<f64> {
        if px >= self.w || py >= self.h {
            return None;
        }
        let key = ((px / BLOCK) as i64, (py / BLOCK) as i64);
        if !self.cache.contains_key(&key) {
            let block = self.read_block(key);
            self.cache.insert(key, block);
        }
        let block = self.cache.get(&key)?.as_ref()?;

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
struct Pyramid {
    levels: Vec<Level>, // ascending z
}

impl Pyramid {
    fn open(root: &Path, doc: &Doc) -> Result<Self> {
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
    fn sample(&mut self, want_z: u32, x: f64, y: f64) -> Option<f64> {
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
    fn sample_finest(&mut self, x: f64, y: f64) -> Option<f64> {
        let top = self.levels.last().map(|l| l.z)?;
        self.sample(top, x, y)
    }

    fn cached_blocks(&self) -> usize {
        self.levels.iter().map(|l| l.cache.len()).sum()
    }
}

/// Coarsest level whose ground cell still resolves the angular step at `d`.
fn level_for(d: f64, lat: f64, coarsest: u32, finest: u32) -> u32 {
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

/// Initial bearing, degrees clockwise from north.
pub fn initial_bearing(lon1: f64, lat1: f64, lon2: f64, lat2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dl = (lon2 - lon1).to_radians();
    let y = dl.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
    y.atan2(x).to_degrees().rem_euclid(360.0)
}

/// Great-circle destination, degrees in and out.
fn destination(lon: f64, lat: f64, az_deg: f64, d: f64) -> (f64, f64) {
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

impl Column {
    fn new(h: usize) -> Self {
        Self {
            dist: vec![f64::INFINITY; h],
            cover: vec![0f32; h],
        }
    }
}

/// March one ray and fill its column.
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
) -> usize {
    let mut samples = 0usize;
    let mut max_alpha = f64::NEG_INFINITY;
    // Row 0 is the top of the frame, so a larger angle means a smaller row
    // index. `filled_from` is the topmost row already written; a new maximum
    // fills the band [row_new, filled_from) and moves the boundary up.
    let mut filled_from = height;
    let mut last_visible = ground_res(finest, p.lat);

    let mut d = ground_res(finest, p.lat);
    while d < p.max_range {
        let z = level_for(d, p.lat, coarsest, finest);
        let step = ground_res(z, p.lat);

        let (lon, lat) = destination(p.lon, p.lat, az, d);
        let (sx, sy) = lonlat_to_merc(lon, lat);

        if let Some(h) = pyr.sample(z, sx, sy) {
            samples += 1;
            let drop = d * d * (1.0 - REFRACTION_K) / (2.0 * EARTH_R);
            let alpha = ((h - eye - drop) / d).atan().to_degrees();

            if alpha > max_alpha {
                max_alpha = alpha;
                // The band's top edge lands at a fractional row; keeping that
                // fraction as coverage is what antialiases the horizon, and it
                // costs nothing since alpha is already a float.
                let f = ((p.alt_max - alpha) / alt_step).max(0.0);
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
            let base = (58.0, 74.0, 52.0);
            (
                lerp(base.0, f64::from(sky.0), t),
                lerp(base.1, f64::from(sky.1), t),
                lerp(base.2, f64::from(sky.2), t),
            )
        } else {
            (f64::from(sky.0), f64::from(sky.1), f64::from(sky.2))
        }
    };

    // A one-pixel-wide line centred on the edge's true sub-pixel height,
    // distributed by overlap: an edge at 20.75 puts 25% into row 20 and 75%
    // into row 21, and one landing exactly on 20.5 lands wholly in row 20.
    //
    // The ink applies to whatever the line covers, sky included -- a dark
    // outline over the horizon does darken the sky above it.
    let stroke_half_width = 0.5 * f64::from(p.supersample_y);
    let mut ink = vec![0f64; height];

    for row in 0..height {
        let s = edge_strength(row);
        if s <= 0.0 {
            continue;
        }
        let depth_fade = 0.45 + 0.4 * (1.0 - (-col.dist[row] / haze).exp());
        let amount = s * (1.0 - depth_fade);

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
                    let s = sky_colour(alt);
                    (f64::from(s.0), f64::from(s.1), f64::from(s.2))
                };
                (
                    lerp(back.0, front.0, c),
                    lerp(back.1, front.1, c),
                    lerp(back.2, front.2, c),
                )
            } else {
                front
            };

            let k = 1.0 - ink[row].clamp(0.0, 1.0);
            let px = (px.0 * k, px.1 * k, px.2 * k);

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

/// Project peaks and decide which are visible.
///
/// Independent of rendering -- no image is needed, so this also answers "what
/// can be seen from here" on its own. Peaks are bucketed by azimuth and one
/// ray is marched per occupied bucket, which bounds the work by the number of
/// distinct bearings rather than by the number of peaks.
pub fn resolve_peaks(root: &Path, doc: &Doc, p: &Params, peaks: &mut [Peak]) -> Result<usize> {
    let (coarsest, finest) = (doc.grid.coarsest_level, doc.grid.finest_level);
    let mut pyr = Pyramid::open(root, doc)?;
    let eye = viewpoint_elevation(&mut pyr, p)? + p.eye_height;

    // Geometry first: distance, bearing, DTM elevation, and where each lands.
    for pk in peaks.iter_mut() {
        pk.distance = great_circle(p.lon, p.lat, pk.lon, pk.lat);
        pk.azimuth = initial_bearing(p.lon, p.lat, pk.lon, pk.lat);
        let (mx, my) = lonlat_to_merc(pk.lon, pk.lat);
        pk.ele = pyr.sample_finest(mx, my);

        let h = pk.ele.unwrap_or(f64::NEG_INFINITY);
        let drop = pk.distance * pk.distance * (1.0 - REFRACTION_K) / (2.0 * EARTH_R);
        pk.altitude = ((h - eye - drop) / pk.distance).atan().to_degrees();

        let off = (pk.azimuth - p.az_start).rem_euclid(360.0);
        pk.x = off / p.step_deg;
        pk.y = (p.alt_max - pk.altitude) / p.step_deg;
        pk.column = if off <= p.az_span { pk.x.floor() as isize } else { -1 };
    }

    // Bucket by the ray we would cast, then walk each bucket's peaks outward
    // in distance against a single running maximum.
    let mut order: Vec<usize> = (0..peaks.len()).filter(|&i| peaks[i].ele.is_some()).collect();
    order.sort_by(|&a, &b| {
        peaks[a]
            .azimuth
            .partial_cmp(&peaks[b].azimuth)
            .unwrap()
            .then(peaks[a].distance.partial_cmp(&peaks[b].distance).unwrap())
    });

    // Group into rays. A bucket wider than one output pixel would merge peaks
    // that render apart, so the bucket is the pixel.
    let bucket_deg = p.step_deg;
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut i = 0;
    while i < order.len() {
        let az0 = peaks[order[i]].azimuth;
        let mut j = i;
        while j < order.len() && peaks[order[j]].azimuth - az0 < bucket_deg {
            j += 1;
        }
        groups.push(order[i..j].to_vec());
        i = j;
    }

    let geometry: Vec<(f64, f64)> = peaks.iter().map(|k| (k.azimuth, k.distance)).collect();

    // Chunked, not one task per bucket: opening a pyramid means opening seven
    // GTI datasets and starting with a cold block cache, so doing it per
    // bucket costs far more than the marching it parallelises.
    let threads = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    let chunk = groups.len().div_ceil(threads).max(1);

    let results: Vec<Vec<(usize, f64, f64)>> = groups
        .par_chunks(chunk)
        .map(|chunk_groups| -> Result<Vec<(usize, f64, f64)>> {
            let mut pyr = Pyramid::open(root, doc)?;
            let mut out = Vec::new();
            for group in chunk_groups {
            let az = geometry[group[0]].0 + bucket_deg / 2.0;

            // One pass outward: the running maximum before a peak decides
            // whether it is visible, and the maximum beyond it is what the
            // peak stands above.
            let mut max_before = vec![f64::NEG_INFINITY; group.len()];
            let mut max_after = vec![f64::NEG_INFINITY; group.len()];
            let mut running = f64::NEG_INFINITY;

            let mut d = ground_res(finest, p.lat);
            while d < p.max_range {
                let z = level_for(d, p.lat, coarsest, finest);
                let step = ground_res(z, p.lat);
                let (lon, lat) = destination(p.lon, p.lat, az, d);
                let (sx, sy) = lonlat_to_merc(lon, lat);

                if let Some(h) = pyr.sample(z, sx, sy) {
                    let drop = d * d * (1.0 - REFRACTION_K) / (2.0 * EARTH_R);
                    let alpha = ((h - eye - drop) / d).atan().to_degrees();
                    running = running.max(alpha);
                    for (k, &idx) in group.iter().enumerate() {
                        if d < geometry[idx].1 {
                            max_before[k] = running;
                        } else if d > geometry[idx].1 * 1.05 {
                            max_after[k] = max_after[k].max(alpha);
                        }
                    }
                }
                d += step;
            }

            out.extend(
                group
                    .iter()
                    .enumerate()
                    .map(|(k, &idx)| (idx, max_before[k], max_after[k])),
            );
            }
            Ok(out)
        })
        .collect::<Result<Vec<_>>>()?;

    // A tolerance of a fifth of a pixel: at its own summit the peak *is* the
    // terrain, so it ties with the running maximum rather than beating it.
    let tol = p.step_deg * 0.2;
    for group in results {
        for (idx, before, after) in group {
            peaks[idx].visible = peaks[idx].altitude + tol >= before;
            peaks[idx].prominence = (peaks[idx].altitude - after).max(0.0);
        }
    }

    Ok(groups.len())
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

/// The inverse, for reference -- clients do this to read a distance back.
pub fn decode_depth(v: u16) -> Option<f64> {
    (v > 0).then(|| {
        let t = f64::from(v - 1) / 65_534.0;
        (DEPTH_NEAR.ln() + t * (DEPTH_FAR.ln() - DEPTH_NEAR.ln())).exp()
    })
}

pub fn render(root: &Path, doc: &Doc, p: &Params) -> Result<(image::RgbImage, Stats)> {
    let (coarsest, finest) = (doc.grid.coarsest_level, doc.grid.finest_level);
    let (ssx, ssy) = (p.supersample_x.max(1), p.supersample_y.max(1));

    let eye = {
        let mut pyr = Pyramid::open(root, doc)?;
        viewpoint_elevation(&mut pyr, p)? + p.eye_height
    };

    let out_w = (p.az_span / p.step_deg).round() as usize;
    let out_h = ((p.alt_max - p.alt_min) / p.step_deg).round() as usize;
    let sub_h = out_h * ssy as usize;
    let az_step = p.step_deg / f64::from(ssx);
    let alt_step = p.step_deg / f64::from(ssy);

    let threads = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    let chunk = out_w.div_ceil(threads).max(1);

    // Chunked so each worker opens the pyramid once and keeps its block cache
    // warm across the columns it owns.
    let parts: Vec<(usize, Vec<[u8; 3]>, Vec<u16>, usize, usize, usize)> = (0..out_w)
        .step_by(chunk)
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|start| -> Result<_> {
            let cols = chunk.min(out_w - start);
            let mut pyr = Pyramid::open(root, doc)?;
            let mut pixels = vec![[0u8; 3]; cols * out_h];
            let mut depth = vec![0u16; cols * out_h];
            let mut samples = 0usize;
            let mut sky = 0usize;

            let mut column = Column::new(sub_h);
            let mut shaded: Vec<Vec<(f64, f64, f64)>> = Vec::with_capacity(ssx as usize);
            let mut nearest = vec![f64::INFINITY; out_h];

            for local in 0..cols {
                let oc = start + local;
                shaded.clear();
                nearest.fill(f64::INFINITY);
                for k in 0..ssx {
                    column.dist.fill(f64::INFINITY);
                    column.cover.fill(0.0);
                    let az = p.az_start
                        + ((oc * ssx as usize + k as usize) as f64 + 0.5) * az_step;
                    samples += march_ray(
                        &mut pyr, p, eye, az, alt_step, sub_h, coarsest, finest, &mut column,
                    );
                    sky += column.dist.iter().filter(|d| d.is_infinite()).count();

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
                    pixels[orow * cols + local] =
                        [clamp(r / n), clamp(g / n), clamp(b / n)];
                }
            }

            Ok((start, pixels, depth, samples, pyr.cached_blocks(), sky))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut img = image::RgbImage::new(out_w as u32, out_h as u32);
    let mut depth_img =
        image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::new(out_w as u32, out_h as u32);
    let mut samples = 0usize;
    let mut blocks = 0usize;
    let mut sky = 0usize;
    let mut cells = 0usize;
    for (start, pixels, depth, n, b, s) in parts {
        samples += n;
        blocks += b;
        sky += s;
        let cols = pixels.len() / out_h;
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

fn sky_colour(alt: f64) -> (u8, u8, u8) {
    // Deeper blue overhead, pale towards the horizon.
    let t = (alt / 30.0).clamp(0.0, 1.0);
    (
        lerp(196.0, 110.0, t) as u8,
        lerp(216.0, 156.0, t) as u8,
        lerp(238.0, 214.0, t) as u8,
    )
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

fn clamp(v: f64) -> u8 {
    v.clamp(0.0, 255.0) as u8
}

/// Degrees of the compass, for the summary line.
pub fn compass(az: f64) -> &'static str {
    const NAMES: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    NAMES[((az.rem_euclid(360.0) / 45.0).round() as usize) % 8]
}
