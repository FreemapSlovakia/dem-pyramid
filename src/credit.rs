//! Which terrain models a render could have been answered from.
//!
//! The pyramid is a mosaic: by the time the marcher reads it, a sample carries
//! no record of the source it came from. So this asks the question the other
//! way round -- of the *view* rather than of the pixels -- by sweeping the
//! ground the render can see and naming every source whose box covers it.
//!
//! *Every* box, not the highest priority one, because priority does not decide
//! alone: tiles are filtered against each source's footprint when the index is
//! built, and nodata falls through at read time, so ground inside a national
//! box is routinely served by whatever lies beneath. Naming only the top box
//! would leave GEDTM30 credited nowhere -- it sits under all of them and
//! answers most of the area their boxes overstate.
//!
//! Sources are compared by their declared lon/lat box, which bounds where one
//! *could* contribute rather than where it has data, so the answer errs towards
//! naming a model that contributed nothing. `footprints/` holds the exact
//! outlines if that ever proves too loose.
//!
//! Each model is reported under its `api_name` carrying its own credit lines,
//! so a client displays what it is given rather than keeping a copy of the
//! licence text that rots.
//!
//! The credits are not ours: they are read from the elevation API's own source
//! tree, whose `name` is the `api_name` here. One model, one credit, written
//! once -- and a dataset gained there is credited here without a release.

use crate::config::{Doc, Source};
use crate::panorama::destination;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Floor on the sweep's cell. A model whose declared box is finer than this is
/// a tile rather than a terrain model, and chasing it would cost more than the
/// render being credited.
const MIN_CELL_M: f64 = 1_000.0;

/// Backstops on the sweep, so a stray box cannot turn crediting into the
/// expensive part of a render. Neither can bind at the ranges the routes
/// allow -- at the `MIN_CELL_M` floor a 400 km sweep wants 566 rings and 2513
/// bearings -- which is the point: binding would silently cost the coverage
/// the sizing exists to guarantee rather than merely cap the cost.
const MAX_RINGS: usize = 1024;
const MAX_BEARINGS: usize = 4096;

/// Metres per degree of latitude. Only ever used to size the sweep, where a
/// sphere is close enough.
const DEG_M: f64 = 111_320.0;

/// How a model wants to be credited.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Attribution {
    /// The credit line verbatim as the licence asks for it.
    pub name: String,
    /// Where the dataset lives, when there is a page to link to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Credit lines by `api_name`.
pub type Credits = HashMap<String, Vec<Attribution>>;

/// Where the elevation API keeps its source list. Both `serve` and `check`
/// default to it, so they cannot end up reading different trees.
pub const DEFAULT_DIR: &str = "/fm/storage1/backend.freemap.sk-data/elevation-sources";

/// The fields this reads; anything else in `source.json` is the elevation
/// API's business.
#[derive(Deserialize)]
struct SourceJson {
    name: String,
    /// Absent only in a malformed entry; `check` reports it rather than
    /// refusing to serve over it.
    #[serde(default)]
    file: String,
    #[serde(default)]
    attributions: Vec<Attribution>,
    /// Whether the pyramid builds this dataset. The one thing about a dataset
    /// that cannot be measured from it.
    #[serde(default)]
    pyramid: bool,
}

/// One dataset in the elevation API's list.
pub struct Entry {
    /// The directory, `NNN-slug`. The numeric prefix is the precedence: the
    /// first entry covering a point answers for it, so sorting by this sorts
    /// most authoritative first.
    pub dir: String,
    /// The model this dataset belongs to; several entries share one.
    pub name: String,
    /// The raster, absolute, as GDAL opens it.
    pub file: String,
    pub attributions: Vec<Attribution>,
    /// Whether the pyramid builds this dataset.
    pub pyramid: bool,
}

/// Reads the elevation API's source tree: one subdirectory per dataset, each
/// with a `source.json`. Sorted by directory, which is precedence order.
///
/// Anything without a `source.json` is skipped, the same way the elevation API
/// skips it -- which is what keeps `.git` and `README.md` out of the way now
/// that the tree is a checkout. A malformed one is an error, because a
/// silently uncredited model is a licence breach.
pub fn load_entries(dir: &Path) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();

    for entry in
        std::fs::read_dir(dir).with_context(|| format!("elevation sources: {}", dir.display()))?
    {
        let subdir = entry?.path();
        let path = subdir.join("source.json");

        if !path.exists() {
            continue;
        }

        let text = std::fs::read_to_string(&path).with_context(|| format!("{}", path.display()))?;

        let parsed: SourceJson = serde_json::from_str(&text)
            .with_context(|| format!("{}: not a source.json", path.display()))?;

        entries.push(Entry {
            dir: subdir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            name: parsed.name,
            file: parsed.file,
            attributions: parsed.attributions,
            pyramid: parsed.pyramid,
        });
    }

    entries.sort_by(|a, b| a.dir.cmp(&b.dir));

    Ok(entries)
}

/// Credit lines by `api_name`, for the datasets the pyramid builds. Several
/// share a name -- Spain is four, France seven -- so their credits merge under
/// it, deduped.
///
/// Only the ones it builds: a dataset the list carries but the pyramid does
/// not would otherwise lend its licence line to every render naming that
/// model, which is the miscrediting `check_attributions` keys on the file to
/// avoid.
pub fn credits_of(entries: &[Entry]) -> Credits {
    let mut credits: Credits = HashMap::new();

    for entry in entries.iter().filter(|e| e.pyramid) {
        let into = credits.entry(entry.name.clone()).or_default();

        for attr in &entry.attributions {
            if !into.contains(attr) {
                into.push(attr.clone());
            }
        }
    }

    credits
}

/// Fails unless every source the pyramid serves is named in the elevation
/// API's list, under the model it is reported as, with a credit to show.
///
/// Keyed on the file rather than the `api_name`, because a name is a model and
/// several datasets share one: a new DTM added under a name that is already
/// credited would otherwise inherit another dataset's licence line and be
/// served confidently miscredited, which is worse than being uncredited.
pub fn check_attributions(sources: &[Source], entries: &[Entry]) -> Result<()> {
    for source in sources {
        let Some(entry) = entries.iter().find(|e| e.file == source.path) else {
            bail!(
                "{}: {} is in no source.json, so nothing says how to credit it",
                source.id,
                source.path
            );
        };

        if entry.name != source.api_name {
            bail!(
                "{}: reported as {} here but named {} in {}",
                source.id,
                source.api_name,
                entry.name,
                entry.dir
            );
        }

        if entry.attributions.is_empty() {
            bail!("{}: {} carries no attribution", source.id, entry.dir);
        }
    }

    Ok(())
}

/// One model behind a render, as `meta.sources` reports it.
#[derive(Debug, Serialize)]
pub struct Seen {
    /// The `api_name`: a country code for a national model, the model's own id
    /// for one that is not country-scoped.
    pub source: String,
    /// Plural because a model can be several datasets under one name -- `be` is
    /// Wallonia and Flanders, each with its own licence.
    pub attributions: Vec<Attribution>,
}

fn holds(source: &Source, lon: f64, lat: f64) -> bool {
    let [w, s, e, n] = source.bbox;

    lon >= w && lon <= e && lat >= s && lat <= n
}

/// `destination` adds a signed delta to the longitude without wrapping it, so
/// a sweep that crosses the antimeridian comes back past +-180 -- where no box
/// holds it, not even the global one, and the render is credited to nothing.
fn wrap_lon(lon: f64) -> f64 {
    (lon + 180.0).rem_euclid(360.0) - 180.0
}

/// The shorter side of a source's declared box, metres.
///
/// Longitude is measured at the pole-most edge, where a degree is shortest, so
/// the answer is the box at its narrowest rather than at its widest.
fn box_extent_m(source: &Source) -> f64 {
    let [w, s, e, n] = source.bbox;
    let lat_m = (n - s) * DEG_M;
    let lon_m = (e - w) * DEG_M * s.abs().max(n.abs()).to_radians().cos();

    lat_m.min(lon_m)
}

/// What a render is answered from: the models behind a sector of ground, most
/// authoritative first.
///
/// `fov_deg` of 360 is a full turn; the caller's own `az_start` is where the
/// sweep begins, which matters only for a slice.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn sources_seen(
    doc: &Doc,
    credits: &Credits,
    lon: f64,
    lat: f64,
    range_m: f64,
    az_start: f64,
    fov_deg: f64,
) -> Vec<Seen> {
    if doc.sources.is_empty() {
        return Vec::new();
    }

    // Sized from the smallest box in the catalogue rather than fixed, so
    // adding a city-scale model tightens the sweep instead of silently going
    // uncredited.
    //
    // Divided by sqrt(2) because the sweep is a lattice and the boxes are
    // axis-aligned lon/lat: samples a box's own width apart leave a diagonal
    // gap of that width times sqrt(2), and a box the size of the one that set
    // the spacing drops into it. The half-diagonal is what has to fit.
    let cell = doc
        .sources
        .iter()
        .map(box_extent_m)
        .fold(f64::INFINITY, f64::min)
        .max(MIN_CELL_M)
        / std::f64::consts::SQRT_2;

    let mut hit = vec![false; doc.sources.len()];
    let mut left = doc.sources.len();

    // Not `position`: nodata falls through, so every box over a sample is a
    // source that could have answered it, not just the first.
    let mark = |plon: f64, plat: f64, hit: &mut Vec<bool>, left: &mut usize| {
        for (i, source) in doc.sources.iter().enumerate() {
            if !hit[i] && holds(source, plon, plat) {
                hit[i] = true;
                *left -= 1;
            }
        }
    };

    mark(wrap_lon(lon), lat, &mut hit, &mut left);

    let rings = ((range_m / cell).ceil() as usize).clamp(1, MAX_RINGS);

    // Held so the arc between neighbouring bearings stays within a cell at the
    // far edge, where they are furthest apart.
    let step_deg = if range_m > 0.0 {
        (cell / range_m).to_degrees()
    } else {
        fov_deg.max(1.0)
    };

    let steps = ((fov_deg / step_deg).ceil() as usize).clamp(1, MAX_BEARINGS);

    'sweep: for b in 0..=steps {
        let az = az_start + fov_deg * (b as f64) / (steps as f64);

        for r in 1..=rings {
            let (plon, plat) = destination(lon, lat, az, range_m * (r as f64) / (rings as f64));

            mark(wrap_lon(plon), plat, &mut hit, &mut left);

            if left == 0 {
                break 'sweep;
            }
        }
    }

    // By `api_name` rather than by id: several ids can be one model to a reader
    // -- Spain is four -- and the credit is the same for all of them.
    let mut seen: Vec<Seen> = Vec::new();

    for (i, source) in doc.sources.iter().enumerate() {
        if hit[i] && !seen.iter().any(|s| s.source == source.api_name) {
            seen.push(Seen {
                source: source.api_name.clone(),
                // `check_credits` refuses to serve a source with none.
                attributions: credits.get(&source.api_name).cloned().unwrap_or_default(),
            });
        }
    }

    seen
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FootprintMode, Grid, Nodata};

    fn source(id: &str, api_name: &str, priority: i64, bbox: [f64; 4]) -> Source {
        Source {
            id: id.into(),
            dir: format!("100-{id}"),
            api_name: api_name.into(),
            path: String::new(),
            priority,
            native_res: 1.0,
            bbox,
            nodata: Nodata::Declared("declared".into()),
            finest_level: 14,
            resampling: "bilinear".into(),
            footprint: FootprintMode::Bbox,
            fill_nodata_md: None,
        }
    }

    /// Priority order, the way `config::load` leaves it.
    fn doc(mut sources: Vec<Source>) -> Doc {
        sources.sort_by_key(|s| -s.priority);

        Doc {
            grid: Grid {
                crs: "EPSG:3857".into(),
                tile_px: 512,
                block_px: 512,
                finest_level: 14,
                coarsest_level: 5,
            },
            sources,
        }
    }

    fn names(seen: &[Seen]) -> Vec<&str> {
        seen.iter().map(|s| s.source.as_str()).collect()
    }

    /// The whole point of the feature: the fallback under a national box is
    /// what actually answers the ground that box overstates, so it has to be
    /// credited alongside it rather than hidden by it.
    #[test]
    fn a_source_is_credited_through_the_box_above_it() {
        let doc = doc(vec![
            source(
                "fr",
                "fr",
                175,
                [-5.539817, 41.269923, 10.708437, 51.094491],
            ),
            source(
                "gedtm30",
                "gedtm30",
                0,
                [-180.00125, -65.00125, 180.00125, 85.00125],
            ),
        ]);

        // Deep inside France, so every sample lands in `fr`'s box too.
        let seen = sources_seen(&doc, &Credits::new(), 2.5, 46.5, 100_000.0, 0.0, 360.0);

        assert_eq!(names(&seen), ["fr", "gedtm30"]);
    }

    /// Most authoritative first, and one entry per model however many datasets
    /// carry its name.
    #[test]
    fn datasets_sharing_a_name_are_one_credit_in_priority_order() {
        let doc = doc(vec![
            source(
                "es_29",
                "es",
                190,
                [-9.380774, 36.058592, -5.458629, 43.863392],
            ),
            source(
                "es_30",
                "es",
                189,
                [-6.926858, 35.180057, 0.24331, 43.736066],
            ),
            source(
                "gedtm30",
                "gedtm30",
                0,
                [-180.00125, -65.00125, 180.00125, 85.00125],
            ),
        ]);

        let seen = sources_seen(&doc, &Credits::new(), -6.0, 40.0, 50_000.0, 0.0, 360.0);

        assert_eq!(names(&seen), ["es", "gedtm30"]);
    }

    /// The sweep is sized from the smallest box in the catalogue, so a source
    /// far smaller than the range cannot fall between two samples.
    #[test]
    fn a_box_smaller_than_the_range_is_still_found() {
        // 8 km deep, lying between 190 and 198 km due north -- inside the gap
        // a fixed 24-ring sweep of a 300 km range leaves between 187.5 and 200.
        let doc = doc(vec![
            source("tiny", "tiny", 100, [2.0, 50.7067, 2.15, 50.7786]),
            source(
                "gedtm30",
                "gedtm30",
                0,
                [-180.00125, -65.00125, 180.00125, 85.00125],
            ),
        ]);

        let seen = sources_seen(&doc, &Credits::new(), 2.075, 49.0, 300_000.0, 0.0, 360.0);

        assert!(names(&seen).contains(&"tiny"), "{:?}", names(&seen));
    }

    /// A sweep that crosses the antimeridian comes back past +-180, which no
    /// box holds. The model on the far side is reached only once the sample is
    /// wrapped back into range.
    #[test]
    fn a_sweep_across_the_antimeridian_reaches_the_far_side() {
        let doc = doc(vec![source(
            "across",
            "across",
            100,
            [-179.5, -1.0, -177.0, 1.0],
        )]);

        // Due east from just west of the line; 300 km is ~2.7 deg at the
        // equator, so the far samples land beyond +180 before wrapping.
        let seen = sources_seen(&doc, &Credits::new(), 179.9, 0.0, 300_000.0, 90.0, 1.0);

        assert_eq!(names(&seen), ["across"]);
    }

    /// A slice names what is in front of it, not what is behind. Anchored by
    /// the full turn from the same viewpoint, so the slice cannot pass by
    /// reaching neither.
    #[test]
    fn a_narrow_slice_leaves_out_what_it_cannot_see() {
        let doc = doc(vec![
            source("north", "north", 100, [-1.0, 50.0, 1.0, 52.0]),
            source("south", "south", 90, [-1.0, 44.0, 1.0, 46.0]),
        ]);
        let credits = Credits::new();

        let all = sources_seen(&doc, &credits, 0.0, 48.0, 300_000.0, 0.0, 360.0);
        assert_eq!(names(&all), ["north", "south"]);

        // Looking north from between them.
        let slice = sources_seen(&doc, &credits, 0.0, 48.0, 300_000.0, 350.0, 20.0);
        assert_eq!(names(&slice), ["north"]);
    }

    /// The box that sets the spacing is the one most exposed to it: samples a
    /// box's own width apart leave a diagonal gap wider than the box, and a
    /// square one drops through. Scanned rather than spot-checked, because
    /// whether it is missed depends on how the sweep happens to sit against
    /// the box's axes.
    #[test]
    fn a_box_the_size_of_the_spacing_is_found_wherever_it_sits() {
        const SIDE_M: f64 = 30_000.0;
        const RANGE_M: f64 = 300_000.0;
        let (vlon, vlat) = (0.0, 45.0);

        let mut missed = Vec::new();

        for bi in 0..52 {
            let az = f64::from(bi) * 360.0 / 52.0;

            for di in 1..=12 {
                // Held clear of the rim, where a box may be clipped by the
                // range rather than missed by the sweep.
                let dist = RANGE_M * 0.85 * f64::from(di) / 12.0;
                let (clon, clat) = destination(vlon, vlat, az, dist);

                let half_lat = SIDE_M / DEG_M / 2.0;
                let (s, n) = (clat - half_lat, clat + half_lat);
                let half_lon = SIDE_M / (DEG_M * s.abs().max(n.abs()).to_radians().cos()) / 2.0;

                let doc = doc(vec![source(
                    "square",
                    "square",
                    100,
                    [clon - half_lon, s, clon + half_lon, n],
                )]);

                if sources_seen(&doc, &Credits::new(), vlon, vlat, RANGE_M, 0.0, 360.0).is_empty() {
                    missed.push(format!("{az:.0} deg at {:.0} km", dist / 1000.0));
                }
            }
        }

        assert!(missed.is_empty(), "missed {}: {missed:?}", missed.len());
    }

    /// The routes reject these upstream, so this only pins that the casts and
    /// the division stay harmless if one ever gets through.
    #[test]
    fn degenerate_sweeps_do_not_panic() {
        let catalogue = doc(vec![source("sk", "sk", 230, [16.8, 47.7, 22.6, 49.7])]);
        let empty = doc(vec![]);
        let credits = Credits::new();

        // Every one of these collapses the sweep onto the viewpoint, which is
        // inside `sk`, rather than dividing by zero or overflowing a cast.
        for range in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let seen = sources_seen(&catalogue, &credits, 18.0, 48.0, range, 0.0, 360.0);
            assert_eq!(names(&seen), ["sk"], "range {range}");
        }

        assert_eq!(
            names(&sources_seen(
                &catalogue, &credits, 18.0, 48.0, 300_000.0, 0.0, 0.0
            )),
            ["sk"]
        );

        assert!(sources_seen(&empty, &credits, 18.0, 48.0, 300_000.0, 0.0, 360.0).is_empty());
    }

    fn entry(dir: &str, name: &str, file: &str, credited: bool) -> Entry {
        Entry {
            dir: dir.into(),
            name: name.into(),
            file: file.into(),
            pyramid: true,
            attributions: if credited {
                vec![Attribution {
                    name: "DMR 5.0: ÚGKK SR".into(),
                    url: None,
                }]
            } else {
                vec![]
            },
        }
    }

    /// A model with nothing to credit it with must not be served.
    #[test]
    fn a_source_with_no_credit_refuses_to_serve() {
        let doc = doc(vec![source("sk", "sk", 230, [16.8, 47.7, 22.6, 49.7])]);

        assert!(check_attributions(&doc.sources, &[]).is_err());
        assert!(check_attributions(&doc.sources, &[entry("010-sk", "sk", "", false)]).is_err());
        assert!(check_attributions(&doc.sources, &[entry("010-sk", "sk", "", true)]).is_ok());
    }

    /// A new dataset under a name that is already credited must not inherit
    /// the other dataset's licence line. Keyed on the file, so it does not.
    #[test]
    fn a_dataset_missing_from_the_list_is_refused_even_under_a_credited_name() {
        let mut de_by = source("de_by", "de", 100, [9.0, 47.0, 14.0, 51.0]);
        de_by.path = "/dtm/de_by/all.vrt".into();

        let mut de_nw = source("de_nw", "de", 99, [5.8, 50.3, 9.5, 52.6]);
        de_nw.path = "/dtm/de_nw/all.vrt".into();

        let list = [entry("245-de_by", "de", "/dtm/de_by/all.vrt", true)];

        assert!(check_attributions(&doc(vec![de_by.clone()]).sources, &list).is_ok());

        // de_nw is not in the list, though "de" is credited because of de_by.
        assert!(check_attributions(&doc(vec![de_by, de_nw]).sources, &list).is_err());
    }
}
