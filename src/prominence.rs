//! Topographic prominence of OSM peaks, measured on the pyramid: each peak is
//! moved to the highest cell near its OSM node and flooded from, always taking
//! the highest cell on the flood's edge, so the lowest height passed before
//! reaching higher ground is the key col. Floods start on the finest level and
//! move to coarser ones as they grow; a col found on a coarse level is then
//! looked for again on the finer ones, along the route the coarse flood took.

use anyhow::{Context, Result};
use gdal::vector::{Feature, FieldDefn, Geometry, LayerAccess, LayerOptions, OGRFieldType};
use gdal::{Dataset, DriverManager};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering as Atomic};
use std::sync::{Mutex, OnceLock};

use crate::config::{Doc, Source, ground_res, level_res};
use crate::credit::{Credits, holds};
use crate::grid::lonlat_to_merc;
use crate::panorama::{Pyramid, great_circle};

/// Levels a flood moves through, and the cells it may visit on each before
/// moving on; then the pyramid's coarsest level, where it may visit up to
/// `CONTINENT` -- a backstop no European landmass reaches.
const STAGES: [(u32, usize); 3] = [(14, 200_000), (12, 200_000), (10, 400_000)];
const CONTINENT: usize = 50_000_000;

/// Held by a flood on the coarsest level: a continent's worth of cells, so
/// one at a time, however many threads reach that stage together.
static CONTINENTAL: Mutex<()> = Mutex::new(());

/// How far from its OSM node a summit is looked for, ground metres: OSM and a
/// national DTM disagree about where a top is by tens of metres, GEDTM30 --
/// 30 m data, all that covers some countries -- by up to a couple of hundred.
const SNAP_M: f64 = 100.0;
const SNAP_COARSE_M: f64 = 250.0;

/// The finest level GEDTM30 alone reaches; a summit found no finer than this
/// has no national data under it.
const GEDTM30_LEVEL: u32 = 13;

/// What a cell of one level holds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cell {
    Height(f64),
    /// No data where GEDTM30 covers all land: the sea, which every flood may
    /// reach, and where an enclosed one ends.
    Sea,
    /// Land past the national data of a level finer than GEDTM30's, or past
    /// the pyramid's extent: ground this level cannot say anything about.
    Edge,
}

/// One level's grid of cells.
pub trait Cells {
    fn cell(&mut self, x: i64, y: i64) -> Cell;
}

struct Level<'a> {
    pyr: &'a mut Pyramid,
    z: u32,
    coarsest: u32,
}

impl Cells for Level<'_> {
    fn cell(&mut self, x: i64, y: i64) -> Cell {
        if let Some(h) = self.pyr.cell(self.z, x, y) {
            return Cell::Height(h);
        }
        let Some((cx, cy)) = self.pyr.cell_centre(self.z, x, y) else {
            return Cell::Edge;
        };
        let Some((px, py)) = self.pyr.cell_of(self.coarsest, cx, cy) else {
            return Cell::Edge;
        };
        if !self.pyr.contains(self.coarsest, px, py) {
            return Cell::Edge;
        }
        // Up to GEDTM30's finest level every land cell has data. Finer, its
        // level tells the sea beside a coast from land the national data
        // stops short of; the coarsest level cannot, as a coastal cell there
        // averages in the land.
        let land = self.z > GEDTM30_LEVEL
            && self
                .pyr
                .cell_of(GEDTM30_LEVEL, cx, cy)
                .is_some_and(|(gx, gy)| self.pyr.cell(GEDTM30_LEVEL, gx, gy).is_some());
        if land { Cell::Edge } else { Cell::Sea }
    }
}

/// A cell on the flood's edge, ordered by height so the heap pops the highest.
struct Edge(f64, i64, i64);

impl PartialEq for Edge {
    fn eq(&self, o: &Self) -> bool {
        self.0.total_cmp(&o.0) == Ordering::Equal
    }
}
impl Eq for Edge {}
impl PartialOrd for Edge {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Edge {
    fn cmp(&self, o: &Self) -> Ordering {
        self.0.total_cmp(&o.0)
    }
}

#[derive(Debug, PartialEq)]
pub enum Flood {
    /// Reached ground higher than the summit: the key col's height, and the
    /// higher cell reached.
    Col(f64, (i64, i64)),
    /// Shut in by nodata first: the summit is its landmass's highest point.
    Island,
    /// Outgrew its budget on this level.
    TooBig,
    /// Reached the edge of this level's coverage.
    Edge,
}

const STEPS: [(i64, i64); 8] = [
    (-1, -1),
    (0, -1),
    (1, -1),
    (-1, 0),
    (1, 0),
    (-1, 1),
    (0, 1),
    (1, 1),
];

/// The flood's start, which no step leads to.
const ROOT: u8 = 8;

#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn key((x, y): (i64, i64)) -> u64 {
    (u64::from(x as u32) << 32) | u64::from(y as u32)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn unkey(k: u64) -> (i64, i64) {
    (
        i64::from((k >> 32) as u32 as i32),
        i64::from(k as u32 as i32),
    )
}

/// Every cell a flood looked at, with the step it was reached by.
#[derive(Default)]
pub struct Tree(FxHashMap<u64, u8>);

impl Tree {
    /// The route from `end` back to the flood's start. Each cell was reached
    /// from the highest cell then on the flood's edge, so the lowest cell on
    /// the route to higher ground is the key col.
    pub fn route(&self, mut at: (i64, i64)) -> Vec<(i64, i64)> {
        let mut out = vec![at];
        while let Some(&step) = self.0.get(&key(at)) {
            let Some((dx, dy)) = STEPS.get(usize::from(step)) else {
                break;
            };
            at = (at.0 - dx, at.1 - dy);
            out.push(at);
        }
        out
    }

    pub fn cells(&self) -> impl Iterator<Item = (i64, i64)> + '_ {
        self.0.keys().map(|&k| unkey(k))
    }
}

/// Flood from `start` until ground higher than `top`, the summit's height.
pub fn flood(cells: &mut impl Cells, start: (i64, i64), top: f64, budget: usize) -> (Flood, Tree) {
    let mut heap = BinaryHeap::from([Edge(top, start.0, start.1)]);
    // The flood's hot path, so a cheap hash, not SipHash.
    let mut tree = Tree::default();
    tree.0.insert(key(start), ROOT);
    let mut col = top;
    let mut visited = 0usize;

    while let Some(Edge(h, x, y)) = heap.pop() {
        if h > top {
            return (Flood::Col(col, (x, y)), tree);
        }
        col = col.min(h);
        visited += 1;
        if visited > budget {
            return (Flood::TooBig, tree);
        }
        let mut edge = false;
        for (step, (dx, dy)) in (0u8..).zip(STEPS) {
            let n = (x + dx, y + dy);
            match tree.0.entry(key(n)) {
                std::collections::hash_map::Entry::Vacant(v) => v.insert(step),
                std::collections::hash_map::Entry::Occupied(_) => continue,
            };
            match cells.cell(n.0, n.1) {
                Cell::Height(hn) => heap.push(Edge(hn, n.0, n.1)),
                Cell::Sea => {}
                Cell::Edge => edge = true,
            }
        }
        // Past an edge the ground is unknown, so the flood stops there --
        // unless higher ground beside it is what it would take next.
        if edge {
            let result = match heap.peek() {
                Some(e) if e.0 > top => Flood::Col(col, (e.1, e.2)),
                _ => Flood::Edge,
            };
            return (result, tree);
        }
    }

    (Flood::Island, tree)
}

/// The key col's cell: the lowest on the route, the nearest the summit of
/// equals.
fn col_cell(cells: &mut impl Cells, route: &[(i64, i64)]) -> Option<(i64, i64)> {
    let mut best: Option<((i64, i64), f64)> = None;
    for &c in route.iter().rev() {
        if let Cell::Height(h) = cells.cell(c.0, c.1)
            && best.is_none_or(|(_, b)| h < b)
        {
            best = Some((c, h));
        }
    }
    best.map(|(c, _)| c)
}

/// How many of a coarser level's cells either side of its route a finer
/// flood may stray: averaging moves a narrow pass by a cell or so.
const CORRIDOR: i64 = 2;

/// A finer level, walled in to the cells under a coarser level's route and
/// those beside it.
struct Corridor<'a> {
    level: Level<'a>,
    z: u32,
    cells: FxHashSet<u64>,
}

impl<'a> Corridor<'a> {
    fn new(level: Level<'a>, z: u32, route: &[(i64, i64)]) -> Self {
        let cells = route
            .iter()
            .flat_map(|&(x, y)| {
                (-CORRIDOR..=CORRIDOR)
                    .flat_map(move |dy| (-CORRIDOR..=CORRIDOR).map(move |dx| key((x + dx, y + dy))))
            })
            .collect();
        Self { level, z, cells }
    }
}

impl Cells for Corridor<'_> {
    fn cell(&mut self, x: i64, y: i64) -> Cell {
        let pyr = &self.level.pyr;
        let inside = pyr
            .cell_centre(self.level.z, x, y)
            .and_then(|(cx, cy)| pyr.cell_of(self.z, cx, cy))
            .is_some_and(|c| self.cells.contains(&key(c)));
        if inside {
            self.level.cell(x, y)
        } else {
            Cell::Sea
        }
    }
}

/// The highest cell within `radius` cells of `at`, the nearest of equals,
/// and its height.
pub fn snap(cells: &mut impl Cells, at: (i64, i64), radius: i64) -> Option<((i64, i64), f64)> {
    snap_far(cells, at, radius).map(|(c, h, _)| (c, h))
}

/// As `snap`, with the squared distance in cells the highest cell lies at.
fn snap_far(cells: &mut impl Cells, at: (i64, i64), radius: i64) -> Option<((i64, i64), f64, i64)> {
    let mut best: Option<((i64, i64), f64, i64)> = None;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let d2 = dx * dx + dy * dy;
            if d2 > radius * radius {
                continue;
            }
            let c = (at.0 + dx, at.1 + dy);
            match cells.cell(c.0, c.1) {
                Cell::Height(h) if best.is_none_or(|(_, b, bd)| h > b || (h == b && d2 < bd)) => {
                    best = Some((c, h, d2));
                }
                // Coverage ending inside the window: the summit may lie past
                // it, so this level cannot say where it is.
                Cell::Edge => return None,
                _ => {}
            }
        }
    }
    best
}

/// One OSM peak.
struct Node {
    osm_id: i64,
    name: Option<String>,
    kind: Option<String>,
    ele_osm: Option<String>,
    lon: f64,
    lat: f64,
}

/// Below this a summit barely rises from the ground around it -- a shoulder
/// or a spur somebody named -- and the node keeps its own position.
const MIN_PROMINENCE: f64 = 5.0;

/// What the DTM shows at a node.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Quality {
    /// A summit near the node, standing out on its own.
    Found,
    /// A top near the node, rising less than `MIN_PROMINENCE` from its
    /// surroundings.
    Weak,
    /// The highest ground near the node on the edge of the search: the
    /// terrain goes on rising past it, so the node is on a slope.
    Edge,
    /// A nearer node owns the same summit.
    Shared,
    /// No data around the node.
    None,
}

impl Quality {
    fn name(self) -> &'static str {
        match self {
            Self::Found => "found",
            Self::Weak => "weak",
            Self::Edge => "edge",
            Self::Shared => "shared",
            Self::None => "none",
        }
    }
}

/// What was measured for a node.
struct Measured {
    quality: Quality,
    /// The level and cell its summit was taken from.
    summit: Option<(u32, i64, i64)>,
    /// The summit's lon/lat: the highest full-resolution pixel.
    pos: Option<(f64, f64)>,
    dist_m: Option<f64>,
    prominence: Option<f64>,
    /// Ground size of the cells the key col was found on.
    prom_res_m: Option<f64>,
    /// The summit's elevation on the full-resolution raster.
    ele: Option<f64>,
    /// Ground metres to the nearest higher ground.
    isolation_m: Option<f64>,
    /// The key col's height and lon/lat, the centre of its cell; none for an
    /// island, whose col is the sea.
    col: Option<(f64, f64, f64)>,
    /// Indices into `Datasets`: where `ele` was read, and what the values
    /// were computed from.
    ele_source: Option<usize>,
    prominence_sources: Vec<usize>,
    isolation_sources: Vec<usize>,
    route: Vec<(f64, f64)>,
    /// Index of the OSM saddle at the col.
    saddle: Option<usize>,
}

impl Measured {
    fn only(quality: Quality) -> Self {
        Self {
            quality,
            summit: None,
            pos: None,
            dist_m: None,
            prominence: None,
            prom_res_m: None,
            ele: None,
            isolation_m: None,
            col: None,
            ele_source: None,
            prominence_sources: Vec::new(),
            isolation_sources: Vec::new(),
            route: Vec::new(),
            saddle: None,
        }
    }
}

/// How far out, in cells, isolation is searched on each level before moving
/// to a coarser one: ~2.5 km on z14, ~10 km on z12, ~40 km on z10; then the
/// coarsest level, out to `ISOLATION_RINGS` cells -- some 1000 km.
const ISOLATION_STAGES: [(u32, i64); 3] = [(14, 400), (12, 400), (10, 400)];
const ISOLATION_RINGS: i64 = 2500;

/// Ground metres from the summit to the nearest ground higher than `top`:
/// rings of cells outwards, each level carrying on from where the finer one
/// stopped, against `top` on every level -- a coarse cell above it proves
/// higher ground within it -- and on past the first find until no nearer cell
/// can remain. None where unknown ground might hide a nearer one.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn isolation(
    pyr: &mut Pyramid,
    (sx, sy): (f64, f64),
    top: f64,
    zs: u32,
    coarsest: u32,
) -> Option<f64> {
    let (slon, slat) = crate::grid::merc_to_lonlat(sx, sy);
    let stages = ISOLATION_STAGES
        .into_iter()
        .filter(|(z, _)| *z > coarsest)
        .chain([(coarsest, ISOLATION_RINGS)]);

    // Ground metres already searched, on finer levels.
    let mut searched = 0.0;
    let mut best: Option<f64> = None;
    // Whether the last level searched stopped at unknown ground, and how far
    // out along its axes it searched.
    let (mut cut, mut covered) = (false, 0.0);
    'level: for (z, rings) in stages.filter(|(z, _)| *z <= zs) {
        cut = false;
        let Some((cx, cy)) = pyr.cell_of(z, sx, sy) else {
            continue;
        };
        let res = ground_res(z, slat);
        let from = ((searched / res) as i64).max(1);
        let mut level = Level { pyr, z, coarsest };

        for d in from..=rings {
            // No cell on ring d is nearer than d - 1 cells, of the size cells
            // shrink to poleward of the summit by then.
            let far_lat = (slat.abs() + d as f64 * res / 111_320.0).min(85.0);
            let near = (d - 1) as f64 * ground_res(z, far_lat);
            if best.is_some_and(|b| near > b) {
                return best;
            }
            for (x, y) in ring(cx, cy, d) {
                match level.cell(x, y) {
                    Cell::Height(h) if h > top => {
                        let (hx, hy) = level.pyr.cell_centre(z, x, y)?;
                        let (hlon, hlat) = crate::grid::merc_to_lonlat(hx, hy);
                        let dist = great_circle(slon, slat, hlon, hlat);
                        best = Some(best.map_or(dist, |b| b.min(dist)));
                    }
                    Cell::Height(_) | Cell::Sea => {}
                    // Unknown ground, maybe nearer than any find: a coarser
                    // level carries on from here.
                    Cell::Edge => {
                        (searched, cut) = (near, true);
                        continue 'level;
                    }
                }
            }
        }
        searched = rings as f64 * res;
        let far_lat = (slat.abs() + searched / 111_320.0).min(85.0);
        covered = rings as f64 * ground_res(z, far_lat);
    }

    // Out of rings: a find no farther than the rings reach along their axes
    // has nothing nearer left unsearched.
    if cut {
        None
    } else {
        best.filter(|&b| b <= covered)
    }
}

/// The cells at Chebyshev distance `d` from (`cx`, `cy`).
fn ring(cx: i64, cy: i64, d: i64) -> impl Iterator<Item = (i64, i64)> {
    let side = -d..=d;
    let rows = side
        .clone()
        .flat_map(move |dx| [(cx + dx, cy - d), (cx + dx, cy + d)]);
    let cols = (-d + 1..d).flat_map(move |dy| [(cx - d, cy + dy), (cx + d, cy + dy)]);
    rows.chain(cols)
}

/// The datasets behind the index, highest priority first, named as renders
/// credit them.
pub struct Datasets<'a> {
    sources: &'a [Source],
    names: Vec<String>,
    /// Each full-resolution raster, opened once for every thread: one with
    /// tens of millions of tiles holds hundreds of MB of tile index open.
    rasters: Vec<OnceLock<Option<Mutex<Dataset>>>>,
}

/// Datasets are told apart on the coverage masks' cells, z7 pixels.
const PIXEL_LEVEL: u32 = 7;

type Pixels = FxHashSet<(i64, i64)>;

#[allow(clippy::cast_possible_truncation)]
fn pixel(x: f64, y: f64) -> (i64, i64) {
    let (half, res) = (level_res(0) * 128.0, level_res(PIXEL_LEVEL));
    (
        ((x + half) / res).floor() as i64,
        ((half - y) / res).floor() as i64,
    )
}

#[allow(clippy::cast_precision_loss)]
fn pixel_centre((c, r): (i64, i64)) -> (f64, f64) {
    let (half, res) = (level_res(0) * 128.0, level_res(PIXEL_LEVEL));
    (
        -half + (c as f64 + 0.5) * res,
        half - (r as f64 + 0.5) * res,
    )
}

impl<'a> Datasets<'a> {
    pub fn new(doc: &'a Doc, credits: &Credits) -> Self {
        let names = doc
            .sources
            .iter()
            .map(|s| {
                credits
                    .get(&s.id)
                    .map_or_else(|| s.id.clone(), |c| c.source.clone())
            })
            .collect();
        Self {
            sources: &doc.sources,
            names,
            rasters: doc.sources.iter().map(|_| OnceLock::new()).collect(),
        }
    }

    fn raster(&self, i: usize) -> Option<std::sync::MutexGuard<'_, Dataset>> {
        self.rasters[i]
            .get_or_init(|| {
                let path = raster_path(&self.sources[i])?;
                Dataset::open(path).ok().map(Mutex::new)
            })
            .as_ref()
            .map(|m| m.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// The datasets a mosaic of these pixels takes data from: the highest
    /// with data in each, and those below it where it may not fill the pixel
    /// -- on its coverage's edge.
    fn used(&self, pixels: &Pixels) -> Vec<usize> {
        let at = |p: (i64, i64)| {
            let (x, y) = pixel_centre(p);
            ((crate::grid::merc_to_lonlat(x, y)), (x, y))
        };
        let mut used = vec![false; self.sources.len()];
        for &(c, r) in pixels {
            let (ll, xy) = at((c, r));
            for (i, s) in self.sources.iter().enumerate() {
                if !holds(s, ll, xy) {
                    continue;
                }
                used[i] = true;
                let solid = STEPS.iter().all(|(dx, dy)| {
                    let (ll, xy) = at((c + dx, r + dy));
                    holds(s, ll, xy)
                });
                if solid {
                    break;
                }
            }
        }
        (0..used.len()).filter(|&i| used[i]).collect()
    }

    fn names(&self, used: &[usize]) -> Option<String> {
        (!used.is_empty()).then(|| {
            used.iter()
                .map(|&i| self.names[i].as_str())
                .collect::<Vec<_>>()
                .join(",")
        })
    }
}

/// A source's full-resolution raster: its `vrt://` location without the
/// overview and conversion.
fn raster_path(s: &Source) -> Option<&Path> {
    let path = s.location.strip_prefix("vrt://")?.split('?').next()?;
    Some(Path::new(path))
}

/// The pixels a flood's cells lie in.
fn pixels_of(pyr: &Pyramid, z: u32, tree: &Tree, out: &mut Pixels) {
    for (x, y) in tree.cells() {
        if let Some((mx, my)) = pyr.cell_centre(z, x, y) {
            out.insert(pixel(mx, my));
        }
    }
}

/// The pixels within `radius` ground metres of a Web Mercator point.
#[allow(clippy::cast_possible_truncation)]
fn disc((x, y): (f64, f64), radius: f64) -> Pixels {
    let lat = crate::grid::merc_to_lonlat(x, y).1;
    // Web Mercator stretches ground by 1/cos(lat), more so poleward.
    let far_lat = (lat.abs() + radius / 111_320.0).min(85.0);
    let r = radius / far_lat.to_radians().cos() + level_res(PIXEL_LEVEL);
    let (c0, r0) = pixel(x - r, y + r);
    let (c1, r1) = pixel(x + r, y - r);
    let mut out = Pixels::default();
    for row in r0..=r1 {
        for col in c0..=c1 {
            let (px, py) = pixel_centre((col, row));
            if (px - x).hypot(py - y) <= r {
                out.insert((col, row));
            }
        }
    }
    out
}

/// The summit's elevation and Web Mercator position, and the source it was
/// read from: the highest valid pixel within `radius` Web Mercator metres of
/// (`x`, `y`) on the highest priority raster with data there, the one the
/// mosaic takes the summit from.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn summit_ele(datasets: &Datasets, x: f64, y: f64, radius: f64) -> Option<(f64, f64, f64, usize)> {
    let lonlat = crate::grid::merc_to_lonlat(x, y);
    for (si, s) in datasets.sources.iter().enumerate() {
        if !holds(s, lonlat, (x, y)) {
            continue;
        }
        let Some(ds) = datasets.raster(si) else {
            continue;
        };
        let Ok(gt) = ds.geo_transform() else { continue };
        let (wid, hgt) = ds.raster_size();
        let px = |v: f64| ((v - gt[0]) / gt[1]).floor().clamp(0.0, wid as f64) as usize;
        let py = |v: f64| ((v - gt[3]) / gt[5]).floor().clamp(0.0, hgt as f64) as usize;
        let (c0, c1, r0, r1) = (
            px(x - radius),
            px(x + radius) + 1,
            py(y + radius),
            py(y - radius) + 1,
        );
        let (c1, r1) = (c1.min(wid), r1.min(hgt));
        if c0 >= c1 || r0 >= r1 {
            continue;
        }
        let Ok(buf) = ds.rasterband(1).and_then(|b| {
            b.read_as::<i32>(
                (c0 as isize, r0 as isize),
                (c1 - c0, r1 - r0),
                (c1 - c0, r1 - r0),
                None,
            )
        }) else {
            continue;
        };
        // terrain-tiles' nodata, and its 2 mm steps.
        let w = c1 - c0;
        if let Some((i, top)) = buf
            .data()
            .iter()
            .copied()
            .enumerate()
            .filter(|&(_, v)| v != i32::MIN)
            .max_by_key(|&(_, v)| v)
        {
            let (px, py) = ((c0 + i % w) as f64 + 0.5, (r0 + i / w) as f64 + 0.5);
            return Some((
                f64::from(top) * 0.002,
                gt[0] + px * gt[1],
                gt[3] + py * gt[5],
                si,
            ));
        }
    }
    None
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn measure(pyr: &mut Pyramid, ds: &Datasets, n: &Node, finest: u32, coarsest: u32) -> Measured {
    let (x, y) = lonlat_to_merc(n.lon, n.lat);
    // Cells within `m` ground metres. Under a cell it rounds to 0, so a flood
    // restarting on a coarser level starts from the summit's own cell.
    let reach = |z: u32, m: f64| (m / ground_res(z, n.lat)).round() as i64;

    // The finest level holding the summit: z14 where national data is,
    // coarser where only GEDTM30 is.
    let Some((zs, summit, top, far, r)) = (coarsest..=finest).rev().find_map(|z| {
        let at = pyr.cell_of(z, x, y)?;
        let m = if z > GEDTM30_LEVEL {
            SNAP_M
        } else {
            SNAP_COARSE_M
        };
        let r = reach(z, m).max(1);
        let (c, h, far) = snap_far(&mut Level { pyr, z, coarsest }, at, r)?;
        Some((z, c, h, far, r))
    }) else {
        return Measured::only(Quality::None);
    };
    // Highest on the search's rim: the ground rises on past it.
    if r >= 3 && far >= (r - 1) * (r - 1) {
        return Measured::only(Quality::Edge);
    }

    let Some((sx, sy)) = pyr.cell_centre(zs, summit.0, summit.1) else {
        return Measured::only(Quality::None);
    };
    // The summit cell and its neighbours: the full-resolution peak lies
    // within the cell the level averaged it into, or just beside it.
    let full = summit_ele(ds, sx, sy, 1.5 * level_res(zs));
    let (px, py) = full.map_or((sx, sy), |(_, px, py, _)| (px, py));
    let pos = crate::grid::merc_to_lonlat(px, py);
    // Higher ground is ground above the summit's true height on every level.
    // A coarse cell is an average, never above the ground it averages, so one
    // above it proves higher ground there, and the summit's own massif --
    // averaged lower -- never does.
    let summit_h = full.map_or(top, |(e, ..)| e.max(top));

    let stages: Vec<(u32, usize)> = STAGES
        .into_iter()
        .filter(|(z, _)| *z > coarsest)
        .chain([(coarsest, CONTINENT)])
        .filter(|(z, _)| *z <= zs)
        .collect();
    let mut found = None;
    let mut read = Pixels::default();
    for (i, &(z, budget)) in stages.iter().enumerate() {
        let start = if z == zs {
            summit
        } else {
            let Some(at) = pyr.cell_of(z, sx, sy) else {
                continue;
            };
            let Some((c, _)) = snap(&mut Level { pyr, z, coarsest }, at, reach(z, SNAP_M)) else {
                continue;
            };
            c
        };
        // Held through refining too: a continental route's corridors are
        // the largest floods after it.
        let _one = (z == coarsest).then(|| {
            CONTINENTAL
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let (result, tree) = flood(&mut Level { pyr, z, coarsest }, start, summit_h, budget);
        match result {
            Flood::Col(col, reached) => {
                pixels_of(pyr, z, &tree, &mut read);
                let col = refine(
                    pyr,
                    &stages[..i],
                    z,
                    &tree,
                    reached,
                    col,
                    (sx, sy),
                    summit_h,
                    coarsest,
                    &mut read,
                );
                found = Some(col);
            }
            Flood::Island => {
                pixels_of(pyr, z, &tree, &mut read);
                found = Some(KeyCol {
                    h: 0.0,
                    z,
                    at: None,
                    route: Vec::new(),
                });
            }
            Flood::TooBig | Flood::Edge => continue,
        }
        break;
    }

    let prominence = found.as_ref().map(|c| (summit_h - c.h).max(0.0));
    let quality = if prominence.is_some_and(|p| p < MIN_PROMINENCE) {
        Quality::Weak
    } else {
        Quality::Found
    };
    let ele_source = full.map(|(.., si)| si);
    let mut prominence_sources = if found.is_some() {
        ds.used(&read)
    } else {
        Vec::new()
    };
    if let Some(si) = ele_source
        && found.is_some()
        && !prominence_sources.contains(&si)
    {
        prominence_sources.push(si);
        prominence_sources.sort_unstable();
    }
    let isolation_m = isolation(pyr, (sx, sy), summit_h, zs, coarsest);

    Measured {
        quality,
        summit: Some((zs, summit.0, summit.1)),
        pos: Some(pos),
        dist_m: Some(great_circle(n.lon, n.lat, pos.0, pos.1)),
        prominence,
        prom_res_m: found.as_ref().map(|c| ground_res(c.z, n.lat).round()),
        ele: full.map(|(e, ..)| e),
        isolation_m,
        col: found
            .as_ref()
            .and_then(|c| c.at.map(|(lon, lat)| (c.h, lon, lat))),
        ele_source,
        prominence_sources,
        isolation_sources: isolation_m.map_or_else(Vec::new, |r| ds.used(&disc((sx, sy), r))),
        route: found.map(|c| c.route).unwrap_or_default(),
        saddle: None,
    }
}

/// An OSM saddle or mountain pass node.
struct Saddle {
    osm_id: i64,
    name: Option<String>,
    lon: f64,
    lat: f64,
}

/// How far from a col an OSM saddle may lie and still be taken for it, at
/// least: the col is a cell's centre, the saddle a hand-placed node.
const SADDLE_M: f64 = 200.0;

/// The nearest saddle within reach of each col.
#[allow(clippy::cast_possible_truncation)]
fn match_saddles(measured: &mut [Measured], saddles: &[Saddle]) {
    const BUCKET: f64 = 2_000.0;
    let bucket = |lon: f64, lat: f64| {
        let (x, y) = lonlat_to_merc(lon, lat);
        ((x / BUCKET).floor() as i64, (y / BUCKET).floor() as i64)
    };
    let mut by_bucket: FxHashMap<(i64, i64), Vec<usize>> = FxHashMap::default();
    for (i, s) in saddles.iter().enumerate() {
        by_bucket.entry(bucket(s.lon, s.lat)).or_default().push(i);
    }
    for m in measured.iter_mut() {
        let (Some((_, lon, lat)), Some(res)) = (m.col, m.prom_res_m) else {
            continue;
        };
        let reach = SADDLE_M.max(2.0 * res);
        // Web Mercator metres per ground metre, to size the buckets searched.
        let span = (reach / lat.to_radians().cos() / BUCKET).ceil() as i64;
        let (bx, by) = bucket(lon, lat);
        m.saddle = (-span..=span)
            .flat_map(|dy| (-span..=span).map(move |dx| (bx + dx, by + dy)))
            .filter_map(|b| by_bucket.get(&b))
            .flatten()
            .map(|&i| (i, great_circle(lon, lat, saddles[i].lon, saddles[i].lat)))
            .filter(|&(_, d)| d <= reach)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i);
    }
}

/// The saddles in the input's `saddles` layer, if it has one.
fn read_saddles(path: &Path) -> Result<Vec<Saddle>> {
    let ds = Dataset::open(path).with_context(|| format!("{}", path.display()))?;
    let Ok(mut layer) = ds.layer_by_name("saddles") else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for f in layer.features() {
        let (Some(g), Ok(Some(osm_id))) = (
            f.geometry(),
            f.field_index("osm_id")
                .and_then(|i| f.field_as_integer64(i)),
        ) else {
            continue;
        };
        let (lon, lat, _) = g.get_point(0);
        out.push(Saddle {
            osm_id,
            name: f
                .field_index("name")
                .ok()
                .and_then(|i| f.field_as_string(i).ok().flatten()),
            lon,
            lat,
        });
    }
    Ok(out)
}

/// A key col: its height, the level it was found on and its lon/lat -- none
/// for an island's, which is the sea.
struct KeyCol {
    h: f64,
    z: u32,
    at: Option<(f64, f64)>,
    /// The flood's route, lon/lat: from the summit over the col to higher
    /// ground.
    route: Vec<(f64, f64)>,
}

/// The key col a flood on level `z` found, moved onto the finer `levels` in
/// turn: each floods only along the coarser route, where averaging cannot
/// have moved the pass by more than `CORRIDOR` cells, and stops where it
/// meets ground it has no data for.
#[allow(clippy::too_many_arguments)]
fn refine(
    pyr: &mut Pyramid,
    levels: &[(u32, usize)],
    mut z: u32,
    tree: &Tree,
    reached: (i64, i64),
    mut h: f64,
    (sx, sy): (f64, f64),
    summit_h: f64,
    coarsest: u32,
    read: &mut Pixels,
) -> KeyCol {
    let mut route = tree.route(reached);
    for &(zf, _) in levels.iter().rev() {
        let Some(start) = pyr.cell_of(zf, sx, sy) else {
            break;
        };
        // The coarse flood started from a cell snapped near the summit; the
        // finer one starts at the summit, so the corridor reaches it too.
        let mut walls = route.clone();
        if let (Some(&from), Some(at)) = (route.last(), pyr.cell_of(z, sx, sy)) {
            walls.extend(between(at, from));
        }
        let level = Level {
            pyr,
            z: zf,
            coarsest,
        };
        let (result, t) = flood(
            &mut Corridor::new(level, z, &walls),
            start,
            summit_h,
            CONTINENT,
        );
        let Flood::Col(col, reached) = result else {
            break;
        };
        pixels_of(pyr, zf, &t, read);
        route = t.route(reached);
        (h, z) = (col, zf);
    }
    let at = col_cell(&mut Level { pyr, z, coarsest }, &route)
        .and_then(|(cx, cy)| pyr.cell_centre(z, cx, cy))
        .map(|(mx, my)| crate::grid::merc_to_lonlat(mx, my));
    let line: Vec<(f64, f64)> = route
        .iter()
        .rev()
        .filter_map(|&(x, y)| pyr.cell_centre(z, x, y))
        .collect();
    KeyCol {
        h,
        z,
        at,
        route: simplify(&line, level_res(z))
            .into_iter()
            .map(|(x, y)| crate::grid::merc_to_lonlat(x, y))
            .collect(),
    }
}

/// The cells on a straight line from `a` to `b`.
fn between(a: (i64, i64), b: (i64, i64)) -> impl Iterator<Item = (i64, i64)> {
    let n = (b.0 - a.0).abs().max((b.1 - a.1).abs()).max(1);
    (0..=n).map(move |i| (a.0 + (b.0 - a.0) * i / n, a.1 + (b.1 - a.1) * i / n))
}

/// Douglas-Peucker: the points of `line` it cannot do without to stay within
/// `tolerance` of it.
fn simplify(line: &[(f64, f64)], tolerance: f64) -> Vec<(f64, f64)> {
    if line.len() < 3 {
        return line.to_vec();
    }
    let mut keep = vec![false; line.len()];
    keep[0] = true;
    keep[line.len() - 1] = true;
    let mut spans = vec![(0, line.len() - 1)];
    while let Some((a, b)) = spans.pop() {
        let ((ax, ay), (bx, by)) = (line[a], line[b]);
        let (dx, dy) = (bx - ax, by - ay);
        let len = dx.hypot(dy);
        let off = |&(px, py): &(f64, f64)| {
            if len == 0.0 {
                (px - ax).hypot(py - ay)
            } else {
                ((px - ax) * dy - (py - ay) * dx).abs() / len
            }
        };
        if let Some((i, d)) = (a + 1..b)
            .map(|i| (i, off(&line[i])))
            .max_by(|x, y| x.1.total_cmp(&y.1))
            && d > tolerance
        {
            keep[i] = true;
            spans.push((a, i));
            spans.push((i, b));
        }
    }
    line.iter()
        .zip(keep)
        .filter_map(|(p, k)| k.then_some(*p))
        .collect()
}

fn read_nodes(path: &Path, bbox: Option<[f64; 4]>) -> Result<Vec<Node>> {
    let ds = Dataset::open(path).with_context(|| format!("{}", path.display()))?;
    let mut layer = ds.layer_by_name("peaks")?;
    let field = |f: &Feature, name: &str| -> Option<String> {
        f.field_index(name)
            .ok()
            .and_then(|i| f.field_as_string(i).ok().flatten())
    };

    let mut out = Vec::new();
    // The output's osm_id index is unique; caught here, before hours of work.
    let mut ids = FxHashSet::default();
    for f in layer.features() {
        let osm_id = f
            .field_index("osm_id")
            .ok()
            .and_then(|i| f.field_as_integer64(i).ok().flatten())
            .context("a peak without osm_id")?;
        anyhow::ensure!(
            ids.insert(osm_id),
            "osm_id {osm_id} twice in {}",
            path.display()
        );
        let Some(g) = f.geometry() else { continue };
        let (lon, lat, _) = g.get_point(0);
        if let Some([w, s, e, n]) = bbox
            && !(lon >= w && lon <= e && lat >= s && lat <= n)
        {
            continue;
        }
        out.push(Node {
            osm_id,
            name: field(&f, "name"),
            kind: field(&f, "type"),
            ele_osm: field(&f, "ele_osm"),
            lon,
            lat,
        });
    }
    Ok(out)
}

/// `serve` reads the first twelve columns by position. The point is the
/// summit where one was found on national data, else the node.
fn write(
    path: &Path,
    ds_names: &Datasets,
    nodes: &[Node],
    saddles: &[Saddle],
    measured: &[Measured],
) -> Result<()> {
    let tmp = path.with_extension("tmp.gpkg");
    let _ = std::fs::remove_file(&tmp);

    let mut ds = DriverManager::get_driver_by_name("GPKG")?.create_vector_only(&tmp)?;
    let srs = gdal::spatial_ref::SpatialRef::from_epsg(4326)?;
    {
        let mut txn = ds.start_transaction()?;
        let layer = txn.create_layer(LayerOptions {
            name: "peaks",
            srs: Some(&srs),
            ty: gdal::vector::OGRwkbGeometryType::wkbPoint,
            options: None,
        })?;
        for (name, ty) in [
            ("osm_id", OGRFieldType::OFTInteger64),
            ("name", OGRFieldType::OFTString),
            ("type", OGRFieldType::OFTString),
            ("ele_osm", OGRFieldType::OFTString),
            ("prominence", OGRFieldType::OFTReal),
            ("prom_dist_m", OGRFieldType::OFTReal),
            ("ele", OGRFieldType::OFTReal),
            ("isolation_m", OGRFieldType::OFTReal),
            ("prom_res_m", OGRFieldType::OFTReal),
            ("summit", OGRFieldType::OFTString),
            ("osm_lon", OGRFieldType::OFTReal),
            ("osm_lat", OGRFieldType::OFTReal),
            ("col_ele", OGRFieldType::OFTReal),
            ("col_lon", OGRFieldType::OFTReal),
            ("col_lat", OGRFieldType::OFTReal),
            ("ele_source", OGRFieldType::OFTString),
            ("prominence_sources", OGRFieldType::OFTString),
            ("isolation_sources", OGRFieldType::OFTString),
            ("col_osm_id", OGRFieldType::OFTInteger64),
            ("col_name", OGRFieldType::OFTString),
        ] {
            FieldDefn::new(name, ty)?.add_to_layer(&layer)?;
        }

        let tenth = |v: f64| (v * 10.0).round() / 10.0;
        for (n, m) in nodes.iter().zip(measured) {
            let mut f = Feature::new(layer.defn())?;
            f.set_field_integer64(0, n.osm_id)?;
            for (i, v) in [(1, &n.name), (2, &n.kind), (3, &n.ele_osm)] {
                if let Some(v) = v {
                    f.set_field_string(i, v)?;
                }
            }
            for (i, v) in [
                (4, m.prominence.map(tenth)),
                (5, m.dist_m.map(tenth)),
                (6, m.ele.map(tenth)),
                (7, m.isolation_m.map(f64::round)),
                (8, m.prom_res_m),
            ] {
                if let Some(v) = v {
                    f.set_field_double(i, v)?;
                }
            }
            f.set_field_string(9, m.quality.name())?;
            f.set_field_double(10, n.lon)?;
            f.set_field_double(11, n.lat)?;
            if let Some((ele, lon, lat)) = m.col {
                f.set_field_double(12, tenth(ele))?;
                f.set_field_double(13, lon)?;
                f.set_field_double(14, lat)?;
            }
            for (i, used) in [
                (15, m.ele_source.map(|s| vec![s]).unwrap_or_default()),
                (16, m.prominence_sources.clone()),
                (17, m.isolation_sources.clone()),
            ] {
                if let Some(names) = ds_names.names(&used) {
                    f.set_field_string(i, &names)?;
                }
            }
            if let Some(s) = m.saddle.map(|s| &saddles[s]) {
                f.set_field_integer64(18, s.osm_id)?;
                if let Some(name) = &s.name {
                    f.set_field_string(19, name)?;
                }
            }

            // Moved only onto national data: GEDTM30 places a summit tens to
            // hundreds of metres off, further than OSM usually is.
            let (lon, lat) = match (m.quality, m.pos, m.summit) {
                (Quality::Found, Some(p), Some((zs, _, _))) if zs > GEDTM30_LEVEL => p,
                _ => (n.lon, n.lat),
            };
            let mut g = Geometry::empty(gdal::vector::OGRwkbGeometryType::wkbPoint)?;
            g.add_point_2d((lon, lat));
            f.set_geometry(g)?;
            f.create(&layer)?;
        }

        // Each prominence's route, from the summit over the key col to
        // higher ground.
        let routes = txn.create_layer(LayerOptions {
            name: "routes",
            srs: Some(&srs),
            ty: gdal::vector::OGRwkbGeometryType::wkbLineString,
            options: None,
        })?;
        FieldDefn::new("osm_id", OGRFieldType::OFTInteger64)?.add_to_layer(&routes)?;
        for (n, m) in nodes.iter().zip(measured) {
            if m.route.len() < 2 {
                continue;
            }
            let mut f = Feature::new(routes.defn())?;
            f.set_field_integer64(0, n.osm_id)?;
            let mut g = Geometry::empty(gdal::vector::OGRwkbGeometryType::wkbLineString)?;
            for &p in &m.route {
                g.add_point_2d(p);
            }
            f.set_geometry(g)?;
            f.create(&routes)?;
        }
        txn.commit()?;
    }
    // For `GET /peak/{osm_id}`.
    for sql in [
        "CREATE UNIQUE INDEX peaks_osm_id ON peaks(osm_id)",
        "CREATE UNIQUE INDEX routes_osm_id ON routes(osm_id)",
    ] {
        ds.execute_sql(sql, None, gdal::vector::sql::Dialect::DEFAULT)?;
    }
    drop(ds);

    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Measure every peak in `input` (optionally only inside `bbox`) and write
/// them, with prominence, to `output`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn run(
    root: &Path,
    doc: &Doc,
    credits: &Credits,
    input: &Path,
    output: &Path,
    bbox: Option<[f64; 4]>,
) -> Result<()> {
    // A regional run writes only its region, which would replace a file
    // holding every peak.
    anyhow::ensure!(
        bbox.is_none() || !output.exists(),
        "{} exists, and --bbox would replace it with one region's peaks",
        output.display()
    );
    let mut nodes = read_nodes(input, bbox)?;
    anyhow::ensure!(
        !nodes.is_empty(),
        "no peaks in {}{}",
        input.display(),
        if bbox.is_some() { " inside --bbox" } else { "" }
    );
    // Neighbours together, so a thread's blocks serve the peaks after it.
    nodes.sort_by_key(|n| {
        let (x, y) = lonlat_to_merc(n.lon, n.lat);
        ((y / 20_000.0) as i64, (x / 20_000.0) as i64)
    });

    let t0 = std::time::Instant::now();
    let done = AtomicUsize::new(0);
    let total = nodes.len();

    let (finest, coarsest) = (doc.grid.finest_level, doc.grid.coarsest_level);
    let datasets = Datasets::new(doc, credits);
    let mut measured: Vec<Measured> = nodes
        .par_chunks(64)
        .map_init(
            || Pyramid::open(root, doc),
            |pyr, chunk| -> Result<Vec<Measured>> {
                let pyr = pyr.as_mut().map_err(|e| anyhow::anyhow!("{e:#}"))?;
                let out = chunk
                    .iter()
                    .map(|n| measure(pyr, &datasets, n, finest, coarsest))
                    .collect();
                let d = done.fetch_add(chunk.len(), Atomic::Relaxed) + chunk.len();
                if d / 20_000 != (d - chunk.len()) / 20_000 {
                    eprintln!("{d} of {total} peaks, {:.0} s", t0.elapsed().as_secs_f64());
                }
                Ok(out)
            },
        )
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();

    // A DEM summit stands for one real summit: of the nodes snapped to it,
    // the nearest keeps the values, the others are left without rather than
    // lent a summit that is not theirs.
    // By position, so two nodes find one summit whichever level each
    // snapped on.
    let at = |m: &Measured| m.pos.map(|(lon, lat)| (lon.to_bits(), lat.to_bits()));
    let mut nearest: HashMap<(u64, u64), (usize, f64)> = HashMap::new();
    for (i, m) in measured.iter().enumerate() {
        if let (Some(s), Some(d)) = (at(m), m.dist_m) {
            let e = nearest.entry(s).or_insert((i, d));
            if d < e.1 {
                *e = (i, d);
            }
        }
    }
    for (i, m) in measured.iter_mut().enumerate() {
        if at(m).is_some_and(|s| nearest[&s].0 != i) {
            *m = Measured::only(Quality::Shared);
        }
    }

    let saddles = read_saddles(input)?;
    match_saddles(&mut measured, &saddles);

    write(output, &datasets, &nodes, &saddles, &measured)?;

    let count = |q: Quality| measured.iter().filter(|m| m.quality == q).count();
    println!(
        "{total} peaks: {} found, {} weak, {} on a slope, {} sharing a summit, {} without data; \
         {} with prominence, {} at an OSM saddle, {:.0} s -> {}",
        count(Quality::Found),
        count(Quality::Weak),
        count(Quality::Edge),
        count(Quality::Shared),
        count(Quality::None),
        measured.iter().filter(|m| m.prominence.is_some()).count(),
        measured.iter().filter(|m| m.saddle.is_some()).count(),
        t0.elapsed().as_secs_f64(),
        output.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Heights from rows of digits, `.` for the sea, `e` for the edge of
    /// this level's coverage; outside the rows is sea.
    struct Grid(Vec<Vec<Cell>>);

    impl Grid {
        fn new(rows: &[&str]) -> Self {
            Self(
                rows.iter()
                    .map(|r| {
                        r.chars()
                            .map(|c| match c {
                                'e' => Cell::Edge,
                                c => c
                                    .to_digit(10)
                                    .map_or(Cell::Sea, |d| Cell::Height(f64::from(d))),
                            })
                            .collect()
                    })
                    .collect(),
            )
        }
    }

    impl Cells for Grid {
        fn cell(&mut self, x: i64, y: i64) -> Cell {
            let (Ok(x), Ok(y)) = (usize::try_from(x), usize::try_from(y)) else {
                return Cell::Sea;
            };
            self.0
                .get(y)
                .and_then(|r| r.get(x))
                .copied()
                .unwrap_or(Cell::Sea)
        }
    }

    /// Two summits, 7 and 9, joined by a saddle at 4: the lower one's
    /// prominence is 3, its col the saddle.
    #[test]
    fn the_key_col_is_the_lowest_pass_to_higher_ground() {
        let mut g = Grid::new(&["1111111111", "1675448911", "1111111111"]);
        let (result, tree) = flood(&mut g, (2, 1), 7.0, 1000);
        assert_eq!(result, Flood::Col(4.0, (6, 1)));
        assert_eq!(col_cell(&mut g, &tree.route((6, 1))), Some((4, 1)));
    }

    /// The deeper of two routes to higher ground is not the key col.
    #[test]
    fn the_highest_pass_is_taken() {
        let mut g = Grid::new(&["1111111", "1622291", "1155591", "1111111"]);
        let (Flood::Col(col, reached), tree) = flood(&mut g, (1, 1), 6.0, 1000) else {
            panic!("no col");
        };
        assert_eq!(col, 5.0);
        let route = tree.route(reached);
        assert_eq!(route.last(), Some(&(1, 1)));
        assert_eq!(col_cell(&mut g, &route), Some((2, 2)));
    }

    /// Nothing higher, nodata all round: the island's highest point.
    #[test]
    fn an_island_summit_reaches_the_sea() {
        let mut g = Grid::new(&[".....", ".232.", ".383.", ".232.", "....."]);
        assert_eq!(flood(&mut g, (2, 2), 8.0, 1000).0, Flood::Island);
    }

    /// Higher ground beside an edge is still found: the edge stops the flood
    /// only when the ground past it is what decides.
    #[test]
    fn higher_ground_beside_an_edge_is_taken() {
        let mut g = Grid::new(&["eee", "e59", "eee"]);
        assert_eq!(flood(&mut g, (1, 1), 5.0, 1000).0, Flood::Col(5.0, (2, 1)));
    }

    /// The edge of a level's coverage is not the sea: the flood stops there
    /// for a coarser level to carry on, rather than calling the top an island.
    #[test]
    fn coverage_ending_is_not_the_sea() {
        let mut g = Grid::new(&["eeeee", "e232e", "e383e", "e232e", "eeeee"]);
        assert_eq!(flood(&mut g, (2, 2), 8.0, 1000).0, Flood::Edge);
    }

    /// A ring is the cells at exactly that Chebyshev distance, each once.
    #[test]
    fn a_ring_is_its_perimeter() {
        let r: std::collections::HashSet<_> = ring(0, 0, 2).collect();
        assert_eq!(r.len(), 16);
        assert!(r.iter().all(|&(x, y)| x.abs().max(y.abs()) == 2));
        assert_eq!(ring(5, 5, 1).count(), 8);
    }

    #[test]
    fn a_flood_past_its_budget_says_so() {
        let mut g = Grid::new(&["9111111119"]);
        assert_eq!(flood(&mut g, (4, 0), 1.0, 3).0, Flood::TooBig);
    }

    #[test]
    fn a_node_snaps_to_the_highest_cell_nearby() {
        let mut g = Grid::new(&["11111", "11191", "11111"]);
        assert_eq!(snap(&mut g, (1, 1), 2), Some(((3, 1), 9.0)));
        assert_eq!(snap(&mut g, (0, 0), 1), Some(((0, 0), 1.0)));
    }

    /// A cell's key survives negative coordinates.
    #[test]
    fn keys_round_trip() {
        for c in [(0, 0), (-1, 5), (123_456, -7), (-4_000_000, 4_000_000)] {
            assert_eq!(unkey(key(c)), c);
        }
    }

    #[test]
    fn a_pixel_holds_its_centre() {
        for p in [(0, 0), (70_000, 45_000), (-3, 9)] {
            let (x, y) = pixel_centre(p);
            assert_eq!(pixel(x, y), p);
        }
    }

    /// Every pixel within the radius is in the disc, poleward ones too.
    #[test]
    fn a_disc_covers_its_radius() {
        let c = lonlat_to_merc(20.0, 49.0);
        let d = disc(c, 50_000.0);
        for (lon, lat) in [(20.0, 49.44), (20.0, 48.56), (20.68, 49.0), (19.32, 49.0)] {
            let (x, y) = lonlat_to_merc(lon, lat);
            assert!(d.contains(&pixel(x, y)), "{lon},{lat}");
        }
    }

    /// A straight run loses its middle; a corner past the tolerance stays.
    #[test]
    fn simplify_keeps_corners() {
        let line = [(0.0, 0.0), (1.0, 0.1), (2.0, 0.0), (3.0, 5.0), (4.0, 10.0)];
        assert_eq!(
            simplify(&line, 0.5),
            vec![(0.0, 0.0), (2.0, 0.0), (4.0, 10.0)]
        );
    }
}
