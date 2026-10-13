//! OSM saddles, moved onto the DTM's saddle point near them.
//!
//! A saddle is where two higher areas meet as the level drops: lowering a
//! water level over a window of ground, it is the cell that first joins two
//! separate islands of higher ground. Each such cell is a candidate, deep by
//! how far it lies below the lower of the two summits it joins. The node's
//! saddle is the nearest candidate deep enough to count: on a ridge of
//! notches the deepest within reach is often a neighbour's.

use anyhow::{Context, Result};
use gdal::Dataset;
use gdal::vector::LayerAccess;
use std::path::Path;

use crate::config::{ground_res, level_res};
use crate::grid::lonlat_to_merc;
use crate::panorama::{Pyramid, great_circle};
use crate::prominence::{Cell, Cells, Datasets, GEDTM30_LEVEL, Level, Quality, STEPS, full_window};

/// How far from its node a saddle is looked for, ground metres: on national
/// data, and where only GEDTM30 is.
const SNAP_M: f64 = 150.0;
const SNAP_COARSE_M: f64 = 300.0;

/// Below this a saddle barely dips between what it joins.
const MIN_DEPTH: f64 = 2.0;

/// An OSM saddle or mountain pass node.
pub struct Saddle {
    pub osm_id: i64,
    pub name: Option<String>,
    pub ele_osm: Option<String>,
    pub lon: f64,
    pub lat: f64,
}

/// What the DTM shows at a saddle node.
pub struct Measured {
    pub quality: Quality,
    /// The saddle's lon/lat: on the full-resolution raster where it has data.
    pub pos: Option<(f64, f64)>,
    pub dist_m: Option<f64>,
    pub ele: Option<f64>,
    /// How far below the lower of the two summits it joins, within the
    /// window: a lower bound where the ground rises on past it.
    pub depth_m: Option<f64>,
    /// Ground size of the cells it was found on, and their level.
    pub res_m: Option<f64>,
    pub z: Option<u32>,
    pub ele_source: Option<usize>,
}

impl Measured {
    pub fn only(quality: Quality) -> Self {
        Self {
            quality,
            pos: None,
            dist_m: None,
            ele: None,
            depth_m: None,
            res_m: None,
            z: None,
            ele_source: None,
        }
    }
}

/// The saddle nearest `centre` at least `min_depth` deep in a grid of
/// heights `w` wide, NaN where there are none, or the deepest where none is;
/// and its depth. Not on the grid's rim, where a gully crossing a corner
/// joins its banks only past the grid.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
pub fn saddle(h: &[f64], w: usize, centre: (f64, f64), min_depth: f64) -> Option<(usize, f64)> {
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }

    let rows = h.len() / w;
    let mut order: Vec<usize> = (0..h.len()).filter(|&i| !h[i].is_nan()).collect();
    order.sort_by(|&a, &b| h[b].total_cmp(&h[a]));
    let mut parent: Vec<usize> = (0..h.len()).collect();
    // Each island's highest height, kept at its root.
    let mut top = vec![f64::NAN; h.len()];
    let mut on = vec![false; h.len()];
    // (cell, depth, squared distance from the centre)
    let mut candidates: Vec<(usize, f64, f64)> = Vec::new();

    for &i in &order {
        let (x, y) = ((i % w) as i64, (i / w) as i64);
        let mut roots: Vec<usize> = Vec::new();
        for (dx, dy) in STEPS {
            let (nx, ny) = (x + dx, y + dy);
            if nx < 0 || ny < 0 || nx >= w as i64 || ny >= rows as i64 {
                continue;
            }
            let j = ny as usize * w + nx as usize;
            if on[j] {
                let r = find(&mut parent, j);
                if !roots.contains(&r) {
                    roots.push(r);
                }
            }
        }
        let rim = x == 0 || y == 0 || x == w as i64 - 1 || y == rows as i64 - 1;
        if roots.len() >= 2 && !rim {
            let mut tops: Vec<f64> = roots.iter().map(|&r| top[r]).collect();
            tops.sort_by(|a, b| b.total_cmp(a));
            let depth = tops[1] - h[i];
            let d2 = (x as f64 - centre.0).powi(2) + (y as f64 - centre.1).powi(2);
            candidates.push((i, depth, d2));
        }
        on[i] = true;
        top[i] = h[i];
        for r in roots {
            parent[r] = i;
            top[i] = top[i].max(top[r]);
        }
    }
    let nearest = candidates
        .iter()
        .filter(|c| c.1 >= min_depth)
        .min_by(|a, b| a.2.total_cmp(&b.2));
    let deepest = || {
        candidates
            .iter()
            .max_by(|a, b| a.1.total_cmp(&b.1).then(b.2.total_cmp(&a.2)))
    };
    nearest.or_else(deepest).map(|&(i, d, _)| (i, d))
}

/// Find the saddle near one node: on the finest level whose window around it
/// holds data and meets no edge of it, then on the full-resolution raster
/// around the cell found.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn measure(
    pyr: &mut Pyramid,
    ds: &Datasets,
    s: &Saddle,
    finest: u32,
    coarsest: u32,
) -> Measured {
    let (x, y) = lonlat_to_merc(s.lon, s.lat);
    // Coarser than GEDTM30's finest level a window is too coarse to place a
    // saddle, and every level up to it covers all land.
    for z in (GEDTM30_LEVEL.max(coarsest)..=finest).rev() {
        let m = if z > GEDTM30_LEVEL {
            SNAP_M
        } else {
            SNAP_COARSE_M
        };
        let r = ((m / ground_res(z, s.lat)).round() as i64).max(2);
        let Some((cx, cy)) = pyr.cell_of(z, x, y) else {
            continue;
        };
        let side = 2 * r + 1;
        let mut level = Level { pyr, z, coarsest };
        let mut h = Vec::with_capacity((side * side) as usize);
        let mut edge = false;
        for dy in -r..=r {
            for dx in -r..=r {
                h.push(match level.cell(cx + dx, cy + dy) {
                    Cell::Height(v) => v,
                    Cell::Sea => f64::NAN,
                    Cell::Edge => {
                        edge = true;
                        f64::NAN
                    }
                });
            }
        }
        // Past national data the ground is unknown, and a coarser level
        // sees all of the window.
        if edge || h.iter().all(|v| v.is_nan()) {
            continue;
        }

        let side = side as usize;
        let Some((i, depth)) = saddle(&h, side, (r as f64, r as f64), MIN_DEPTH) else {
            return Measured::only(Quality::Edge);
        };
        let (gx, gy) = (i % side, i / side);
        let Some((sx, sy)) = pyr.cell_centre(z, cx + gx as i64 - r, cy + gy as i64 - r) else {
            return Measured::only(Quality::None);
        };

        // The cell and those beside it, at full resolution: the saddle point
        // lies within the cell the level averaged it into, or just beside it.
        let notch = full_window(ds, sx, sy, 2.0 * level_res(z)).and_then(|win| {
            let rows = win.h.len() / win.w;
            #[allow(clippy::cast_precision_loss)]
            let centre = ((win.w as f64 - 1.0) / 2.0, (rows as f64 - 1.0) / 2.0);
            // The deepest: within a cell or two, the notch itself.
            let (j, _) = saddle(&win.h, win.w, centre, f64::INFINITY)?;
            let (px, py) = win.centre(j);
            Some((win.h[j], px, py, win.source))
        });
        // No notch at full resolution, where averaging made the saddle: the
        // height at the cell's centre.
        let full = notch.or_else(|| {
            let win = full_window(ds, sx, sy, level_res(z) / 2.0)?;
            let (j, _) = win
                .h
                .iter()
                .enumerate()
                .filter(|(_, v)| !v.is_nan())
                .map(|(j, _)| {
                    let (px, py) = win.centre(j);
                    (j, (px - sx).hypot(py - sy))
                })
                .min_by(|a, b| a.1.total_cmp(&b.1))?;
            Some((win.h[j], sx, sy, win.source))
        });
        let (px, py) = full.map_or((sx, sy), |(_, px, py, _)| (px, py));
        let pos = crate::grid::merc_to_lonlat(px, py);

        return Measured {
            quality: if depth < MIN_DEPTH {
                Quality::Weak
            } else {
                Quality::Found
            },
            pos: Some(pos),
            dist_m: Some(great_circle(s.lon, s.lat, pos.0, pos.1)),
            ele: full.map(|(e, ..)| e),
            depth_m: Some(depth),
            res_m: Some(ground_res(z, s.lat).round()),
            z: Some(z),
            ele_source: full.map(|(.., si)| si),
        };
    }
    Measured::only(Quality::None)
}

/// The saddles in the input's `saddles` layer, if it has one.
pub fn read(path: &Path) -> Result<Vec<Saddle>> {
    let ds = Dataset::open(path).with_context(|| format!("{}", path.display()))?;
    let Ok(mut layer) = ds.layer_by_name("saddles") else {
        return Ok(Vec::new());
    };
    let field = |f: &gdal::vector::Feature, name: &str| -> Option<String> {
        f.field_index(name)
            .ok()
            .and_then(|i| f.field_as_string(i).ok().flatten())
    };
    let mut out = Vec::new();
    // Built with a unique index on osm_id.
    let mut ids = rustc_hash::FxHashSet::default();
    for f in layer.features() {
        let (Some(g), Ok(Some(osm_id))) = (
            f.geometry(),
            f.field_index("osm_id")
                .and_then(|i| f.field_as_integer64(i)),
        ) else {
            continue;
        };
        if !ids.insert(osm_id) {
            continue;
        }
        let (lon, lat, _) = g.get_point(0);
        out.push(Saddle {
            osm_id,
            name: field(&f, "name"),
            ele_osm: field(&f, "ele_osm"),
            lon,
            lat,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(rows: &[&str]) -> (Vec<f64>, usize) {
        let w = rows[0].len();
        let h = rows
            .iter()
            .flat_map(|r| {
                r.chars()
                    .map(|c| c.to_digit(10).map_or(f64::NAN, f64::from))
            })
            .collect();
        (h, w)
    }

    /// Two summits, 9 and 8, joined across a pass at 5: the pass, 3 deep.
    #[test]
    fn the_pass_between_two_summits() {
        let (h, w) = grid(&["1111111", "1956781", "1111111"]);
        let (i, depth) = saddle(&h, w, (3.0, 1.0), 2.0).unwrap();
        assert_eq!((i % w, i / w), (2, 1));
        assert_eq!(depth, 3.0);
    }

    /// A gully across the grid's corner joins its banks only past the rim.
    #[test]
    fn no_saddle_on_the_rim() {
        let (h, w) = grid(&["55355", "55255", "55155"]);
        assert_eq!(saddle(&h, w, (2.0, 1.0), 0.0), None);
    }

    /// A slope rising one way has no saddle on it.
    #[test]
    fn a_slope_is_no_saddle() {
        let (h, w) = grid(&["12345", "12345", "12345"]);
        assert_eq!(saddle(&h, w, (2.0, 1.0), 2.0), None);
    }

    /// Of the notches deep enough, the nearest, though another is deeper.
    #[test]
    fn the_nearest_deep_enough_notch_wins() {
        let (h, w) = grid(&["111111111", "197969891", "111111111"]);
        let (i, depth) = saddle(&h, w, (2.0, 1.0), 2.0).unwrap();
        assert_eq!((i % w, depth), (2, 2.0));
    }

    /// Where none is deep enough, the deepest.
    #[test]
    fn the_deepest_where_none_is_deep_enough() {
        let (h, w) = grid(&["111111111", "197969891", "111111111"]);
        let (i, depth) = saddle(&h, w, (6.0, 1.0), 5.0).unwrap();
        assert_eq!((i % w, depth), (4, 3.0));
    }
}
