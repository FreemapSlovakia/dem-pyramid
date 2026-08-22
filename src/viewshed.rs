//! What can be seen from a point, as a map overlay.
//!
//! The same question the panorama answers, asked the other way round. A
//! panorama walks a ray per azimuth and records what the eye meets; a viewshed
//! walks the same rays and records *where* on the map those surfaces are. The
//! marching, the curvature and refraction, and the pyramid level chosen per
//! distance are all shared, so the two cannot disagree about what is visible.
//!
//! Output is a square RGBA raster in Web Mercator, centred on the viewpoint,
//! transparent where nothing can be seen. It drops straight into a Leaflet
//! `ImageOverlay` with the bounds the response carries.
//!
//! Two honest limits, both worth telling users rather than hiding. This is a
//! bare-earth model: no trees, no buildings, so in forest or town it says you
//! can see more than you can. And the answer is only as good as the DEM, which
//! outside the surveyed countries is 30 m GEDTM30.

use anyhow::{Context, Result};
use rayon::prelude::*;
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::config::{Doc, ground_res};
use crate::grid::{lonlat_to_merc, merc_to_lonlat};
use crate::panorama::{self, Cancel, Pyramid};

pub struct Params {
    pub lon: f64,
    pub lat: f64,
    /// Metres above the ground at the viewpoint.
    pub eye_height: f64,
    pub eye_search_radius: f64,
    /// How far to look, ground metres.
    pub radius: f64,
    /// Ground metres per output pixel, at the viewpoint's latitude.
    pub scale: f64,
    /// Height above ground of the thing being looked *at*. 0 asks whether the
    /// ground itself is visible; 1.7 asks whether a person standing there
    /// would be. It changes the answer noticeably at range, because the ground
    /// hides behind its own convexity long before a standing figure does.
    pub target_height: f64,
    pub colour: (u8, u8, u8),
}

pub struct Out {
    pub image: image::RgbaImage,
    /// West, south, east, north, in degrees, for a Leaflet overlay.
    pub bounds: [f64; 4],
    pub eye_elevation: f64,
    pub rays: usize,
    pub samples: usize,
}

/// Pixels across, from the radius and the scale.
pub fn extent(radius: f64, scale: f64) -> usize {
    ((2.0 * radius / scale).round() as usize).max(1)
}

pub fn render(
    root: &Path,
    doc: &Doc,
    p: &Params,
    cancel: &Cancel,
    progress: Option<&crate::progress::Job>,
) -> Result<Out> {
    let (coarsest, finest) = (doc.grid.coarsest_level, doc.grid.finest_level);
    let px = extent(p.radius, p.scale);

    // Mercator metres are ground metres divided by cos(lat), and the raster is
    // in Mercator so it lines up with the map without resampling.
    //
    // The half-width comes from where the rim actually lands, not from
    // cos(lat) at the centre. Mercator stretches with latitude, so a ray of a
    // given ground length reaches further north than that assumption allows:
    // at 49 degrees and 200 km, 312 km of Mercator against a 305 km half-width,
    // which quietly clipped the outer 2% of the northern rim. Taking the
    // furthest of the four cardinals makes the disc fit whatever the latitude,
    // at the cost of `scale` being nominal rather than exact away from the
    // centre -- which it always was, Mercator being what it is.
    let (cx, cy) = lonlat_to_merc(p.lon, p.lat);
    let half = [0.0, 90.0, 180.0, 270.0]
        .into_iter()
        .map(|az| {
            let (lon, lat) = panorama::destination(p.lon, p.lat, az, p.radius);
            let (x, y) = lonlat_to_merc(lon, lat);
            (x - cx).abs().max((y - cy).abs())
        })
        .fold(0.0f64, f64::max);
    let proj = half * 2.0 / px as f64;
    let (x0, y0) = (cx - half, cy + half);

    let mut pyr = Pyramid::open(root, doc)?;
    let ground = viewpoint_ground(&mut pyr, p)?;
    let eye = ground + p.eye_height;
    drop(pyr);

    // Two rays per pixel of the rim. One is the geometric minimum and is not
    // enough in practice: a ray lands on a pixel centre only by luck, so at
    // one ray per pixel the far field comes out stippled rather than solid --
    // measured 31% of the outer annulus covered against 96% near the middle.
    let rays = ((4.0 * std::f64::consts::PI * p.radius / p.scale).ceil() as usize).max(8);
    if let Some(job) = progress {
        job.set_total(rays);
    }

    // Alpha only. Every ray that reaches a pixel proposes an opacity and the
    // brightest wins, which is what `fetch_max` gives without a lock -- rays
    // overlap heavily near the centre, so they must not race.
    let alpha: Vec<AtomicU8> = (0..px * px).map(|_| AtomicU8::new(0)).collect();

    let chunk = rays.div_ceil(std::thread::available_parallelism().map_or(4, std::num::NonZero::get));
    let samples: usize = (0..rays)
        .step_by(chunk.max(1))
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|start| -> Result<usize> {
            let mut pyr = Pyramid::open(root, doc)?;
            let mut n = 0usize;
            for i in start..(start + chunk).min(rays) {
                cancel.check()?;
                if let Some(job) = progress {
                    job.tick();
                }
                let az = 360.0 * i as f64 / rays as f64;
                n += cast(&mut pyr, p, &alpha, px, (x0, y0), proj, eye, az, coarsest, finest);
            }
            Ok(n)
        })
        .collect::<Result<Vec<_>>>()?
        .iter()
        .sum();

    // Colour everywhere, visibility in alpha alone. Leaving unseen pixels
    // black would put a hard edge along every visibility boundary in a colour
    // plane that carries no information -- bits spent on nothing, and ringing
    // that dirties the faint grazing pixels beside it once a lossy codec and
    // an un-premultiplied composite have had their turn.
    let mut image = image::RgbaImage::from_pixel(
        px as u32,
        px as u32,
        image::Rgba([p.colour.0, p.colour.1, p.colour.2, 0]),
    );
    for (i, a) in alpha.iter().enumerate() {
        let a = a.load(Ordering::Relaxed);
        if a > 0 {
            let (x, y) = ((i % px) as u32, (i / px) as u32);
            image.put_pixel(x, y, image::Rgba([p.colour.0, p.colour.1, p.colour.2, a]));
        }
    }

    let (west, north) = merc_to_lonlat(x0, y0);
    let (east, south) = merc_to_lonlat(x0 + px as f64 * proj, y0 - px as f64 * proj);
    Ok(Out {
        image,
        bounds: [west, south, east, north],
        eye_elevation: eye,
        rays,
        samples,
    })
}

/// Walk one ray, marking what it can see.
#[allow(clippy::too_many_arguments)]
fn cast(
    pyr: &mut Pyramid,
    p: &Params,
    alpha: &[AtomicU8],
    px: usize,
    (x0, y0): (f64, f64),
    proj: f64,
    eye: f64,
    az: f64,
    coarsest: u32,
    finest: u32,
) -> usize {
    // The horizon so far, as a tangent rather than an angle: the comparison is
    // the same, an arctangent being monotone, and this way it costs nothing.
    let mut horizon = f64::NEG_INFINITY;
    let mut prev: Option<(f64, f64)> = None;
    let mut prev_px: Option<(f64, f64)> = None;
    let mut samples = 0usize;

    let mut d = ground_res(finest, p.lat);
    while d <= p.radius {
        let z = panorama::level_for(d, p.lat, coarsest, finest);
        let step = ground_res(z, p.lat);
        let (lon, lat) = panorama::destination(p.lon, p.lat, az, d);
        let (sx, sy) = lonlat_to_merc(lon, lat);

        if let Some(h) = pyr.sample(z, sx, sy) {
            samples += 1;
            let drop = panorama::curvature_drop(d);
            // The ground's own tangent decides what is hidden behind it; the
            // target sits above the ground, so it can clear a horizon the
            // ground beneath it cannot.
            let ground_t = (h - eye - drop) / d;
            let target_t = (h + p.target_height - eye - drop) / d;

            let here = ((sx - x0) / proj, (y0 - sy) / proj);
            if target_t > horizon {
                let a = opacity(ground_t, prev, d, h);
                // Filled from the previous sample's pixel to this one, not
                // just at this one. The marcher steps by DEM cell, which at a
                // fine scale is more than a pixel, so plotting only where it
                // happens to land leaves the ray dotted rather than drawn.
                // Visibility and opacity both vary smoothly along a ray, so
                // the pixels in between are the same answer, not a guess.
                plot(alpha, px, prev_px.unwrap_or(here), here, a);
            }
            horizon = horizon.max(ground_t);
            prev = Some((d, h));
            prev_px = Some(here);
        }
        d += step;
    }
    samples
}

/// Mark every pixel on the segment from `a` to `b`, brightest proposal wins.
fn plot(alpha: &[AtomicU8], px: usize, a: (f64, f64), b: (f64, f64), value: u8) {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let steps = dx.abs().max(dy.abs()).ceil().max(1.0);
    for i in 0..=(steps as usize) {
        let t = i as f64 / steps;
        let (x, y) = ((a.0 + dx * t).floor(), (a.1 + dy * t).floor());
        if x >= 0.0 && y >= 0.0 {
            let (x, y) = (x as usize, y as usize);
            if x < px && y < px {
                alpha[y * px + x].fetch_max(value, Ordering::Relaxed);
            }
        }
    }
}

/// How much of a visible surface the eye actually gets, 0..=255.
///
/// A slope seen edge-on is visible and yet almost nothing of it reaches the
/// eye, so a flat "visible or not" overlay overstates the far field badly.
/// Opacity is the sine of the angle between the line of sight and the surface:
/// face-on is full, grazing fades to nothing.
///
/// Measured in the vertical plane of the ray only, from the previous sample's
/// height. Cross-slope is ignored, which makes a ridge running across the view
/// read slightly more strongly than it should -- cheap, and the error is in
/// the direction of showing rather than hiding.
fn opacity(ray_t: f64, prev: Option<(f64, f64)>, d: f64, h: f64) -> u8 {
    let Some((pd, ph)) = prev else {
        return u8::MAX;
    };
    let run = d - pd;
    if run <= 0.0 {
        return u8::MAX;
    }
    // Angle of the line of sight, and of the ground along it.
    let sight = ray_t.atan();
    let slope = ((h - ph) / run).atan();
    let incidence = (slope - sight).abs().min(std::f64::consts::PI - (slope - sight).abs());
    let v = incidence.sin().clamp(0.0, 1.0);
    // Never fully transparent where something is genuinely visible: a grazing
    // surface is faint, not absent, and absent is what "not visible" means.
    (v * 254.0).round() as u8 + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every bearing must land inside the raster, and in the quadrant it
    /// belongs to. A viewshed that silently drops three quarters of the
    /// compass looks like terrain occlusion until you plot the coverage.
    #[test]
    fn every_bearing_lands_where_it_should() {
        // Far north and a large radius, because that is where Mercator's
        // stretch is worst and where a half-width taken from cos(lat) at the
        // centre used to push the rim out of the raster.
        for (lon, lat, radius, scale) in [
            (20.133_f64, 49.164_f64, 20_000.0_f64, 50.0_f64),
            (20.0, 69.0, 200_000.0, 100.0),
        ] {
        let px = extent(radius, scale);
        let (cx, cy) = lonlat_to_merc(lon, lat);
        let half = [0.0, 90.0, 180.0, 270.0]
            .into_iter()
            .map(|az| {
                let (plon, plat) = panorama::destination(lon, lat, az, radius);
                let (x, y) = lonlat_to_merc(plon, plat);
                (x - cx).abs().max((y - cy).abs())
            })
            .fold(0.0f64, f64::max);
        let proj = half * 2.0 / px as f64;
        let (x0, y0) = (cx - half, cy + half);
        let centre = px as f64 / 2.0;

        for (az, name) in [(0.0, "N"), (90.0, "E"), (180.0, "S"), (270.0, "W")] {
            for d in [scale * 4.0, radius * 0.5, radius] {
                let (plon, plat) = panorama::destination(lon, lat, az, d);
                let (sx, sy) = lonlat_to_merc(plon, plat);
                let ix = (sx - x0) / proj;
                let iy = (y0 - sy) / proj;
                assert!(
                    ix >= 0.0 && iy >= 0.0 && ix < px as f64 && iy < px as f64,
                    "{name} at {d} m fell outside the raster: {ix},{iy} of {px}"
                );
                match name {
                    "N" => assert!(iy < centre, "{name} at {d} m mapped south"),
                    "S" => assert!(iy > centre, "{name} at {d} m mapped north"),
                    "E" => assert!(ix > centre, "{name} at {d} m mapped west"),
                    _ => assert!(ix < centre, "{name} at {d} m mapped east"),
                }
            }
        }
        }
    }
}

fn viewpoint_ground(pyr: &mut Pyramid, p: &Params) -> Result<f64> {
    let (ex, ey) = lonlat_to_merc(p.lon, p.lat);
    let at_point = pyr
        .sample_finest(ex, ey)
        .context("viewpoint has no elevation data")?;
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
