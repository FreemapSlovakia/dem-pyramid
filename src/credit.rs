//! Which terrain models a render could have been answered from.
//!
//! The pyramid is a mosaic: by the time the marcher reads it, a sample carries
//! no record of the source it came from. So this asks the question the other
//! way round -- of the *view* rather than of the pixels -- by sweeping the
//! ground the render can see and naming every source whose box covers it.
//!
//! *Every* box, not the highest priority one, because priority does not decide
//! alone: nodata falls through at read time, so ground inside a national box is
//! routinely served by whatever lies beneath. Naming only the top box would
//! leave GEDTM30 credited nowhere -- it sits under all of them and answers most
//! of the area their boxes overstate.
//!
//! Sources are compared by their measured lon/lat box, which bounds where one
//! *could* contribute rather than where it has data, so the answer errs towards
//! naming a model that contributed nothing.
//!
//! Each dataset is reported as a model named after its directory without the
//! number (`es-29`), carrying its own credit lines, so a client displays what
//! it is given rather than keeping a copy of the licence text that rots.
//!
//! The credits are not ours: they are read from the elevation API's own source
//! tree -- and a dataset gained there is credited here without a release.

use crate::config::{Doc, Source};
use crate::measure::{id_of, model_of};
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

/// What each source is reported as, by source id.
pub type Credits = HashMap<String, Seen>;

/// Where the elevation API keeps its source list. Both `serve` and `check`
/// default to it, so they cannot end up reading different trees.
pub const DEFAULT_DIR: &str = "/fm/storage1/backend.freemap.sk-data/elevation-sources";

/// The fields this reads; anything else in `source.json` is the elevation
/// API's business.
#[derive(Deserialize)]
struct SourceJson {
    #[serde(default)]
    attributions: Vec<Attribution>,
}

/// One dataset in the elevation API's list.
pub struct Entry {
    /// The directory, `NNN-slug`. The numeric prefix is the precedence: the
    /// first entry covering a point answers for it, so sorting by this sorts
    /// most authoritative first.
    pub dir: String,
    /// The directory itself, which links the dataset's rasters.
    pub path: std::path::PathBuf,
    pub attributions: Vec<Attribution>,
    /// Whether the directory links a `mercator.tif`, which is what puts a
    /// dataset in the pyramid. A dangling link counts, so `index.sh` reports it
    /// rather than quietly leaving the dataset out.
    pub mercator: bool,
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
            path: subdir.clone(),
            attributions: parsed.attributions,
            mercator: subdir.join("mercator.tif").symlink_metadata().is_ok(),
        });
    }

    entries.sort_by(|a, b| a.dir.cmp(&b.dir));

    Ok(entries)
}

/// The list's entries for a dataset the index serves that still link a
/// `mercator.tif`, matched by id rather than directory, so a renumbered dataset
/// is still credited until the index picks up its new priority. More than one
/// is two directories spelling one id, and neither can be trusted to be the
/// raster that was indexed.
fn entries_of<'a>(id: &str, entries: &'a [Entry]) -> Vec<&'a Entry> {
    entries
        .iter()
        .filter(|e| e.mercator && id_of(&e.dir) == id)
        .collect()
}

/// What each source is reported as: the name and credit of its entry, both
/// from the list as it is now.
pub fn credits_of(sources: &[Source], entries: &[Entry]) -> Credits {
    sources
        .iter()
        .filter_map(|s| match entries_of(&s.id, entries).as_slice() {
            [e] => Some((
                s.id.clone(),
                Seen {
                    source: model_of(&e.dir).to_owned(),
                    attributions: e.attributions.clone(),
                },
            )),
            _ => None,
        })
        .collect()
}

/// Fails unless every source the pyramid serves is in the elevation API's list
/// exactly once, with a credit to show.
pub fn check_attributions<'a>(
    ids: impl IntoIterator<Item = &'a str>,
    entries: &[Entry],
) -> Result<()> {
    for id in ids {
        let entry = match entries_of(id, entries).as_slice() {
            [e] => *e,
            [] => bail!(
                "{}: no longer has a mercator.tif in the elevation source list, \
                 so nothing says how to credit it",
                id
            ),
            many => bail!(
                "{}: several directories in the elevation source list are this \
                 dataset: {}",
                id,
                many.iter()
                    .map(|e| e.dir.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };

        if entry.attributions.is_empty() {
            bail!("{}: {} carries no attribution", id, entry.dir);
        }
    }

    Ok(())
}

/// One model behind a render, as `meta.sources` reports it.
#[derive(Debug, Clone, Serialize)]
pub struct Seen {
    /// The dataset's directory without its number: `sk`, `es-29`.
    pub source: String,
    /// Plural because one dataset can merge several surveys, each with its own
    /// licence.
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

    doc.sources
        .iter()
        .zip(hit)
        .filter(|(_, hit)| *hit)
        .map(|(source, _)| {
            // `check_attributions` refuses to serve a source without one.
            credits.get(&source.id).cloned().unwrap_or_else(|| Seen {
                source: source.id.clone(),
                attributions: Vec::new(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Grid;

    fn source(model: &str, priority: i64, bbox: [f64; 4]) -> Source {
        Source {
            id: model.replace('-', "_"),
            priority,
            bbox,
        }
    }

    /// Priority order, the way `config::load` leaves it.
    fn doc(mut sources: Vec<Source>) -> Doc {
        sources.sort_by_key(|s| -s.priority);

        Doc {
            grid: Grid {
                finest_level: 14,
                coarsest_level: 5,
            },
            sources,
        }
    }

    fn ids(sources: &[Source]) -> impl Iterator<Item = &str> {
        sources.iter().map(|s| s.id.as_str())
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
            source("fr", 175, [-5.539817, 41.269923, 10.708437, 51.094491]),
            source("gedtm30", 0, [-180.00125, -65.00125, 180.00125, 85.00125]),
        ]);

        // Deep inside France, so every sample lands in `fr`'s box too.
        let seen = sources_seen(&doc, &Credits::new(), 2.5, 46.5, 100_000.0, 0.0, 360.0);

        assert_eq!(names(&seen), ["fr", "gedtm30"]);
    }

    /// Most authoritative first, one entry per dataset, named after its
    /// directory.
    #[test]
    fn each_dataset_is_its_own_model_in_priority_order() {
        let doc = doc(vec![
            source("es-29", 190, [-9.380774, 36.058592, -5.458629, 43.863392]),
            source("es-30", 189, [-6.926858, 35.180057, 0.24331, 43.736066]),
            source("gedtm30", 0, [-180.00125, -65.00125, 180.00125, 85.00125]),
        ]);

        let list = [
            entry("090-es-29", true),
            entry("100-es-30", true),
            entry("999-gedtm30", true),
        ];
        let credits = credits_of(&doc.sources, &list);
        let seen = sources_seen(&doc, &credits, -6.0, 40.0, 50_000.0, 0.0, 360.0);

        assert_eq!(names(&seen), ["es-29", "es-30", "gedtm30"]);
    }

    /// The sweep is sized from the smallest box in the catalogue, so a source
    /// far smaller than the range cannot fall between two samples.
    #[test]
    fn a_box_smaller_than_the_range_is_still_found() {
        // 8 km deep, lying between 190 and 198 km due north -- inside the gap
        // a fixed 24-ring sweep of a 300 km range leaves between 187.5 and 200.
        let doc = doc(vec![
            source("tiny", 100, [2.0, 50.7067, 2.15, 50.7786]),
            source("gedtm30", 0, [-180.00125, -65.00125, 180.00125, 85.00125]),
        ]);

        let seen = sources_seen(&doc, &Credits::new(), 2.075, 49.0, 300_000.0, 0.0, 360.0);

        assert!(names(&seen).contains(&"tiny"), "{:?}", names(&seen));
    }

    /// A sweep that crosses the antimeridian comes back past +-180, which no
    /// box holds. The model on the far side is reached only once the sample is
    /// wrapped back into range.
    #[test]
    fn a_sweep_across_the_antimeridian_reaches_the_far_side() {
        let doc = doc(vec![source("across", 100, [-179.5, -1.0, -177.0, 1.0])]);

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
            source("north", 100, [-1.0, 50.0, 1.0, 52.0]),
            source("south", 90, [-1.0, 44.0, 1.0, 46.0]),
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
        let catalogue = doc(vec![source("sk", 230, [16.8, 47.7, 22.6, 49.7])]);
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

    fn entry(dir: &str, credited: bool) -> Entry {
        Entry {
            dir: dir.into(),
            path: dir.into(),
            mercator: true,
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
        let doc = doc(vec![source("sk", 230, [16.8, 47.7, 22.6, 49.7])]);

        assert!(check_attributions(ids(&doc.sources), &[]).is_err());
        assert!(check_attributions(ids(&doc.sources), &[entry("100-sk", false)]).is_err());
        assert!(check_attributions(ids(&doc.sources), &[entry("100-sk", true)]).is_ok());

        let mut unlinked = entry("100-sk", true);
        unlinked.mercator = false;
        assert!(check_attributions(ids(&doc.sources), &[unlinked]).is_err());
    }

    /// An indexed dataset missing from the list is refused, whatever else the
    /// list credits.
    #[test]
    fn a_dataset_missing_from_the_list_is_refused() {
        let de_by = source("de_by", 100, [9.0, 47.0, 14.0, 51.0]);
        let de_nw = source("de_nw", 99, [5.8, 50.3, 9.5, 52.6]);

        let list = [entry("100-de_by", true)];

        assert!(check_attributions(ids(std::slice::from_ref(&de_by)), &list).is_ok());
        assert!(check_attributions(ids(&[de_by, de_nw]), &list).is_err());
    }

    /// A directory respelt between `-` and `_` upstream is still the same
    /// source, and is credited from its new entry rather than left with none.
    #[test]
    fn a_respelt_directory_keeps_its_credit() {
        let doc = doc(vec![source("es_29", 190, [-9.4, 36.0, -5.4, 43.9])]);
        let list = [entry("090-es-29", true)];

        assert!(check_attributions(ids(&doc.sources), &list).is_ok());

        let credits = credits_of(&doc.sources, &list);
        let seen = sources_seen(&doc, &credits, -7.0, 40.0, 20_000.0, 0.0, 360.0);
        assert!(!seen[0].attributions.is_empty(), "{seen:?}");
        // Named as the list spells it now, not as the index last saw it.
        assert_eq!(seen[0].source, "es-29");
    }

    /// Two directories that are one dataset leave nothing to say which raster
    /// was measured, so serving refuses rather than picking one.
    #[test]
    fn a_dataset_in_two_directories_is_refused() {
        let doc = doc(vec![source("es_29", 190, [-9.4, 36.0, -5.4, 43.9])]);
        let list = [entry("090-es-29", true), entry("100-es_29", true)];

        assert!(check_attributions(ids(&doc.sources), &list).is_err());
    }

    /// The list's other datasets lend nothing to a render.
    #[test]
    fn only_indexed_datasets_are_credited() {
        let doc = doc(vec![source("es-30", 100, [-8.0, 36.0, -2.0, 44.0])]);

        let credits = credits_of(
            &doc.sources,
            &[entry("100-es-30", true), entry("360-sonny-es-canary", true)],
        );

        assert_eq!(credits.keys().collect::<Vec<_>>(), ["es_30"]);
    }
}
