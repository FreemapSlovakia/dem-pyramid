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
    pub az_step_deg: f64,
    pub alt_step_deg: f64,
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
    /// Vertical supersampling factor.
    ///
    /// Stroke width is expressed in *output* pixels, so it has to know this: a
    /// line one buffer-row wide at 3x becomes a third of a pixel once averaged
    /// down, and comes out at a third of its intended weight.
    pub supersample_y: f64,
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

pub struct Buffer {
    pub width: usize,
    pub height: usize,
    /// Distance to visible terrain per cell; f64::INFINITY for sky.
    pub dist: Vec<f64>,
    /// Fraction of the cell covered by that surface, 0..1. Less than 1 only on
    /// a band's top edge, which is what makes the horizon smooth.
    pub cover: Vec<f32>,
    pub eye_elevation: f64,
    pub samples: usize,
    pub blocks: usize,
}

pub fn march(root: &Path, doc: &Doc, p: &Params) -> Result<Buffer> {
    let (coarsest, finest) = (doc.grid.coarsest_level, doc.grid.finest_level);

    let (ex, ey) = lonlat_to_merc(p.lon, p.lat);
    let eye = {
        let mut pyr = Pyramid::open(root, doc)?;
        pyr.sample_finest(ex, ey)
            .context("viewpoint has no elevation data")?
            + p.eye_height
    };

    let width = (p.az_span / p.az_step_deg).round() as usize;
    let height = ((p.alt_max - p.alt_min) / p.alt_step_deg).round() as usize;

    // Columns are independent, so the only shared state is the block cache.
    // Rather than lock one, give each worker its own pyramid handle: the whole
    // cache is well under 100 MB, so a handful of copies is cheaper than
    // contention.
    let threads = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    let chunk = width.div_ceil(threads).max(1);

    let parts: Vec<(usize, usize, Vec<f64>, Vec<f32>, usize, usize)> = (0..width)
        .step_by(chunk)
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|start| march_columns(root, doc, p, eye, start, chunk.min(width - start), height, coarsest, finest))
        .collect::<Result<Vec<_>>>()?;

    let mut dist = vec![f64::INFINITY; width * height];
    let mut cover = vec![0f32; width * height];
    let mut samples = 0usize;
    let mut blocks = 0usize;
    for (start, cols, local_d, local_c, n, b) in parts {
        samples += n;
        blocks += b;
        for row in 0..height {
            dist[row * width + start..row * width + start + cols]
                .copy_from_slice(&local_d[row * cols..row * cols + cols]);
            cover[row * width + start..row * width + start + cols]
                .copy_from_slice(&local_c[row * cols..row * cols + cols]);
        }
    }

    Ok(Buffer {
        width,
        height,
        dist,
        cover,
        eye_elevation: eye,
        samples,
        blocks,
    })
}

/// March one contiguous band of columns, returning its slice of the buffer.
#[allow(clippy::too_many_arguments)]
fn march_columns(
    root: &Path,
    doc: &Doc,
    p: &Params,
    eye: f64,
    start: usize,
    cols: usize,
    height: usize,
    coarsest: u32,
    finest: u32,
) -> Result<(usize, usize, Vec<f64>, Vec<f32>, usize, usize)> {
    let mut pyr = Pyramid::open(root, doc)?;
    let mut dist = vec![f64::INFINITY; cols * height];
    let mut cover = vec![0f32; cols * height];
    let mut samples = 0usize;
    let width = cols;

    for local_col in 0..cols {
        let col = local_col;
        let az = p.az_start + ((start + local_col) as f64 + 0.5) * p.az_step_deg;
        let mut max_alpha = f64::NEG_INFINITY;
        // Row 0 is the top of the frame (alt_max), so a larger angle means a
        // smaller row index. `filled_from` is the topmost row already written;
        // a new maximum angle fills the band [row_new, filled_from) and moves
        // the boundary up. Rows are written exactly once, nearest-first.
        let mut filled_from = height;

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
                    // Everything between the old maximum and this angle is
                    // terrain first seen at this distance. The band's top edge
                    // lands at a fractional row -- keeping that fraction as
                    // coverage is what antialiases the horizon, and it costs
                    // nothing since alpha is already a float.
                    let f = ((p.alt_max - alpha) / p.alt_step_deg).max(0.0);
                    if f < filled_from as f64 {
                        let full_start = f.ceil() as usize;
                        for row in full_start..filled_from {
                            dist[row * width + col] = d;
                            cover[row * width + col] = 1.0;
                        }
                        let partial = f.floor() as usize;
                        if partial < full_start {
                            dist[partial * width + col] = d;
                            cover[partial * width + col] = (full_start as f64 - f) as f32;
                            filled_from = partial;
                        } else {
                            filled_from = full_start;
                        }
                    }
                    // Once the column is full, no farther sample can add to it.
                    if filled_from == 0 {
                        break;
                    }
                }
            }
            d += step;
        }
    }

    let blocks = pyr.cached_blocks();
    Ok((start, cols, dist, cover, samples, blocks))
}

/// Haze-shaded render. Colour comes from the distance value alone -- the
/// nested-ridge look is emergent, not segmented.
/// Box-downsample by an integer factor.
///
/// Supersampling is what makes the strokes look right, and it removes the need
/// for per-pixel rules about which cell a line belongs in. Averaging several
/// rays per output column also gives horizontal antialiasing, which one ray
/// per column cannot: where adjacent rays hit nearly the same DEM cells the
/// edge height is quantised, and a line that should drift smoothly instead
/// jumps in whole steps.
pub fn downsample(img: &image::RgbImage, fx: u32, fy: u32) -> image::RgbImage {
    if fx <= 1 && fy <= 1 {
        return img.clone();
    }
    let (w, h) = (img.width() / fx, img.height() / fy);
    let mut out = image::RgbImage::new(w, h);
    let n = f64::from(fx * fy);
    for y in 0..h {
        for x in 0..w {
            let (mut r, mut g, mut b) = (0.0, 0.0, 0.0);
            for dy in 0..fy {
                for dx in 0..fx {
                    let px = img.get_pixel(x * fx + dx, y * fy + dy);
                    r += f64::from(px[0]);
                    g += f64::from(px[1]);
                    b += f64::from(px[2]);
                }
            }
            out.put_pixel(x, y, image::Rgb([clamp(r / n), clamp(g / n), clamp(b / n)]));
        }
    }
    out
}

pub fn render_image(buf: &Buffer, p: &Params) -> image::RgbImage {
    let mut img = image::RgbImage::new(buf.width as u32, buf.height as u32);
    let haze = 45_000.0_f64; // e-folding distance for the atmospheric blend

    // Silhouette pass: a depth discontinuity in the distance buffer is a ridge
    // seen against something farther away. Stroking those edges is what makes
    // nested ridges legible; without it the near field reads as one flat mass.
    // No ridge segmentation is involved -- it falls out of the buffer.
    // A raw depth ratio is the wrong test. Terrain receding at grazing
    // incidence -- any foreground slope -- changes distance enormously per row
    // while staying perfectly continuous, so a first-difference test paints
    // false contours across it. Meanwhile genuine ridges at similar ranges (40
    // km against 48 km) fall under the same threshold and get nothing.
    //
    // What marks a silhouette is a *discontinuity*: the jump at this row is far
    // larger than the jumps just above and below it. Comparing against the
    // local gradient rather than against a constant separates a step from a
    // steep-but-smooth surface, and works at any range.
    let spike = p.edge_ratio.max(1.001).ln();
    let log_d = |row: usize, col: usize| -> Option<f64> {
        let d = buf.dist[row * buf.width + col];
        d.is_finite().then(|| d.ln())
    };

    // Continuous strength rather than a binary test. A hard threshold makes
    // edges dash in and out wherever the measure hovers around it, and forces
    // a choice between drawing trivial edges and dropping real ones.
    //
    // Strength is the *hidden extent* -- how much terrain the silhouette
    // conceals -- gated by the discontinuity measure. Depth ratio alone is the
    // wrong driver: a terrace lip at 300 m revealing 2 km is a bigger relative
    // jump (1.9 in log space) than a ridge at 20 km revealing 60 km (1.1), so
    // ranking by ratio promotes exactly the edges worth suppressing. Absolute
    // hidden extent ranks them the way the eye does: 1.7 km against 40 km.
    let edge_strength = |row: usize, col: usize| -> f64 {
        let Some(here) = log_d(row, col) else {
            return 0.0;
        };
        if row == 0 {
            return 1.0;
        }
        let Some(above) = log_d(row - 1, col) else {
            return 1.0; // sky above terrain: the skyline itself
        };
        let step_up = above - here;
        if step_up <= 0.0 {
            return 0.0;
        }
        // On a continuous surface the step above matches the step below
        // however steep it is; an occlusion spikes.
        let below = if row + 1 < buf.height {
            log_d(row + 1, col)
        } else {
            None
        };
        let step_down = below.map_or(0.0, |b| here - b).max(0.0);
        let discontinuity = ((step_up - step_down) / spike).clamp(0.0, 1.0);

        let near = here.exp();
        let hidden = near * step_up.exp_m1();
        // Square root so a modest nearby ridge still registers against a
        // distant range that hides ten times more.
        let extent = (hidden / p.edge_hidden_ref).clamp(0.0, 1.0).sqrt();

        discontinuity * extent
    };

    // Colour of whatever surface a cell holds, ignoring coverage.
    let surface = |row: usize, col: usize| -> (f64, f64, f64) {
        let alt = p.alt_max - (row as f64 + 0.5) * p.alt_step_deg;
        let d = buf.dist[row * buf.width + col];
        let sky = sky_colour(alt);
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
    // into row 21, and an edge landing exactly on 20.5 lands wholly in row 20.
    //
    // The ink applies to whatever the line covers, sky included -- a dark
    // outline drawn over the horizon does darken the sky above it, and
    // withholding that half is what stopped the stroke influencing two rows.
    let stroke_half_width = 0.5 * p.supersample_y;
    let mut ink = vec![0f64; buf.width * buf.height];

    for row in 0..buf.height {
        for col in 0..buf.width {
            let s = edge_strength(row, col);
            if s <= 0.0 {
                continue;
            }
            let d = buf.dist[row * buf.width + col];
            let depth_fade = 0.45 + 0.4 * (1.0 - (-d / haze).exp());
            let amount = s * (1.0 - depth_fade);

            // Coverage encodes where the band's top edge actually falls: a
            // cell covered `c` by its band has that edge at `row + (1 - c)`.
            let c = f64::from(buf.cover[row * buf.width + col]);
            let edge_pos = row as f64 + (1.0 - c);
            let lo = edge_pos - stroke_half_width;
            let hi = edge_pos + stroke_half_width;

            let first = lo.floor().max(0.0) as usize;
            let last = (hi.ceil() as usize).min(buf.height);
            for y in first..last {
                let overlap = hi.min((y + 1) as f64) - lo.max(y as f64);
                if overlap > 0.0 {
                    ink[y * buf.width + col] += amount * overlap;
                }
            }
        }
    }

    for row in 0..buf.height {
        let alt = p.alt_max - (row as f64 + 0.5) * p.alt_step_deg;
        for col in 0..buf.width {
            let idx = row * buf.width + col;
            let d = buf.dist[idx];

            // Composite the partially-covered top edge of a band over whatever
            // lies behind it -- sky on the horizon, farther terrain within a
            // ridge stack. Rounding this fraction away is what leaves a
            // staircase.
            let c = f64::from(buf.cover[idx]);
            let front = surface(row, col);
            let px = if d.is_finite() && c < 0.999 {
                let back = if row > 0 {
                    surface(row - 1, col)
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

            let k = 1.0 - ink[idx].clamp(0.0, 1.0);
            let px = (px.0 * k, px.1 * k, px.2 * k);

            // Faint eye-level line at 0 deg.
            let on_eye_level = p.eye_level && alt.abs() < p.alt_step_deg * 0.5;
            let (r, g, b) = if on_eye_level {
                (px.0 * 0.75 + 60.0, px.1 * 0.75 + 60.0, px.2 * 0.75 + 60.0)
            } else {
                px
            };

            img.put_pixel(
                col as u32,
                row as u32,
                image::Rgb([clamp(r), clamp(g), clamp(b)]),
            );
        }
    }

    img
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
