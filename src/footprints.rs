//! Coverage footprint per source.
//!
//! Two modes, chosen per source in sources.yaml:
//!
//!   tiles  Union of the VRT's source rectangles, read straight out of the VRT
//!          XML. Exact for the tile canvas, and costs seconds -- no pixel is
//!          touched. This is what makes the dirty-tile set for an incremental
//!          rebuild precise.
//!
//!   bbox   The declared lon/lat box. Used for single-file sources, which all
//!          declare correct nodata, so their real edge is resolved at warp time
//!          and a conservative footprint only means rebuilding a few extra
//!          tiles.
//!
//! Deliberately NOT gdal_footprint: that derives the outline from pixel values,
//! which would mean reading all 6.7 TB of source data.
//!
//! Note the semantics: a footprint bounds where a source *could* contribute,
//! not where it has data. Tiles exist over sea and are nodata inside, so for
//! coarsely tiled sources the area comes out well above the country's land
//! area. That is conservative in the right direction.

use anyhow::{Context, Result};
use quick_xml::Reader;
use quick_xml::events::Event;
use serde::Serialize;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::config::{Doc, FootprintMode, Source, bbox_area_km2};
use crate::gdal_cli;

/// Rect edges longer than this are densified before reprojection, so a straight
/// edge in a national grid stays correct as a curve in lon/lat.
const SEGMENT_M: f64 = 5000.0;

/// Below this pixel size the geotransform is in degrees, not metres.
const DEGREE_PIXEL_THRESHOLD: f64 = 0.01;

#[derive(Debug, Serialize)]
pub struct Info {
    pub id: String,
    pub mode: FootprintMode,
    pub parts: usize,
    pub area_km2: f64,
    pub bbox_area_km2: f64,
    pub path: String,
}

pub struct Vrt {
    pub srs_wkt: String,
    pub gt: [f64; 6],
    pub rects: Vec<[f64; 4]>,
    pub sources: usize,
    /// Leaf files, resolved to absolute paths.
    pub files: Vec<String>,
}

pub fn parse_vrt(path: &str) -> Result<Vrt> {
    let mut reader = Reader::from_file(path)?;
    let mut buf = Vec::new();

    let mut srs_wkt = String::new();
    let mut gt = [0.0f64; 6];
    let mut have_gt = false;
    let mut rects = Vec::new();
    let mut sources = 0usize;
    let mut files: Vec<String> = Vec::new();
    let mut in_srs = false;
    let mut in_gt = false;
    let mut in_filename = false;
    let mut filename_relative = true;
    let base = Path::new(path).parent().map(Path::to_path_buf).unwrap_or_default();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) => {
                let name = e.name();
                let tag = name.as_ref();

                if tag == b"SRS" {
                    in_srs = srs_wkt.is_empty();
                } else if tag == b"GeoTransform" {
                    in_gt = !have_gt;
                } else if tag == b"SourceFilename" {
                    in_filename = true;
                    filename_relative = e
                        .attributes()
                        .flatten()
                        .find(|a| a.key.as_ref() == b"relativeToVRT")
                        .and_then(|a| a.unescape_value().ok().map(|v| v == "1"))
                        .unwrap_or(false);
                } else if tag == b"DstRect" {
                    let mut r = [0.0f64; 4];
                    for attr in e.attributes().flatten() {
                        let v = attr.unescape_value()?.parse::<f64>().unwrap_or(0.0);
                        match attr.key.as_ref() {
                            b"xOff" => r[0] = v,
                            b"yOff" => r[1] = v,
                            b"xSize" => r[2] = v,
                            b"ySize" => r[3] = v,
                            _ => {}
                        }
                    }
                    rects.push(r);
                } else if tag.ends_with(b"Source") {
                    sources += 1;
                }
            }
            Event::Text(t) => {
                if in_filename {
                    let name = t.unescape()?.trim().to_owned();
                    files.push(if filename_relative {
                        base.join(&name).display().to_string()
                    } else {
                        name
                    });
                } else if in_srs {
                    srs_wkt = t.unescape()?.trim().to_owned();
                } else if in_gt {
                    let parts: Vec<f64> = t
                        .unescape()?
                        .split(',')
                        .filter_map(|v| v.trim().parse::<f64>().ok())
                        .collect();
                    if parts.len() == 6 {
                        gt.copy_from_slice(&parts);
                        have_gt = true;
                    }
                }
            }
            Event::End(_) => {
                in_srs = false;
                in_gt = false;
                in_filename = false;
            }
            _ => {}
        }
        buf.clear();
    }

    anyhow::ensure!(have_gt, "{path}: no GeoTransform in VRT");
    Ok(Vrt {
        srs_wkt,
        gt,
        rects,
        sources,
        files,
    })
}

/// Rect in VRT pixel space -> polygon WKT in the source CRS, plus its area.
fn rect_wkt(gt: &[f64; 6], r: &[f64; 4], segment: bool) -> (String, f64) {
    let corner = |px: f64, py: f64| {
        (
            gt[0] + px * gt[1] + py * gt[2],
            gt[3] + px * gt[4] + py * gt[5],
        )
    };
    let (x0, y0) = corner(r[0], r[1]);
    let (x1, y1) = corner(r[0] + r[2], r[1] + r[3]);

    let area = ((x1 - x0) * (y1 - y0)).abs();

    // Densify long edges so reprojection to lon/lat keeps the true curve.
    let steps = if segment {
        let longest = (x1 - x0).abs().max((y1 - y0).abs());
        ((longest / SEGMENT_M).ceil() as usize).max(1)
    } else {
        1
    };

    let mut wkt = String::from("POLYGON ((");
    let push = |x: f64, y: f64, wkt: &mut String| {
        let _ = write!(wkt, "{x:.4} {y:.4},");
    };
    for i in 0..steps {
        push(x0 + (x1 - x0) * i as f64 / steps as f64, y0, &mut wkt);
    }
    for i in 0..steps {
        push(x1, y0 + (y1 - y0) * i as f64 / steps as f64, &mut wkt);
    }
    for i in 0..steps {
        push(x1 - (x1 - x0) * i as f64 / steps as f64, y1, &mut wkt);
    }
    for i in 0..steps {
        push(x0, y1 - (y1 - y0) * i as f64 / steps as f64, &mut wkt);
    }
    let _ = write!(wkt, "{x0:.4} {y0:.4}))");
    (wkt, area)
}

fn bbox_wkt(bbox: [f64; 4]) -> String {
    let [a, b, c, d] = bbox;
    format!("POLYGON (({a} {b},{c} {b},{c} {d},{a} {d},{a} {b}))")
}

fn write_csv(path: &Path, rows: &[String]) -> Result<()> {
    let file = std::fs::File::create(path)?;
    let mut w = std::io::BufWriter::new(file);
    writeln!(w, "WKT,fid")?;
    for (i, wkt) in rows.iter().enumerate() {
        writeln!(w, "\"{wkt}\",{i}")?;
    }
    w.flush()?;
    Ok(())
}

fn build(src: &Source, out_dir: &Path, tmp_dir: &Path) -> Result<Info> {
    let out = out_dir.join(format!("{}.gpkg", src.id));
    let csv = tmp_dir.join(format!("{}.csv", src.id));
    if out.exists() {
        std::fs::remove_file(&out)?;
    }

    let (rows, area_km2, s_srs) = match src.footprint {
        FootprintMode::Bbox => (
            vec![bbox_wkt(src.bbox)],
            bbox_area_km2(src.bbox),
            "EPSG:4326".to_owned(),
        ),
        FootprintMode::Tiles => {
            let vrt = parse_vrt(&src.path)?;
            if vrt.rects.len() != vrt.sources {
                eprintln!(
                    "  warning: {} has {} sources but {} DstRects",
                    src.id,
                    vrt.sources,
                    vrt.rects.len()
                );
            }
            let segment = vrt.gt[1].abs() > DEGREE_PIXEL_THRESHOLD;
            let mut area = 0.0;
            let rows: Vec<String> = vrt
                .rects
                .iter()
                .map(|r| {
                    let (wkt, a) = rect_wkt(&vrt.gt, r, segment);
                    area += a;
                    wkt
                })
                .collect();

            // Prefer the dataset's own WKT; fall back to the declared code.
            // Keeping the authority matters for datum shifts -- OSGB36 read
            // through a bare PROJ.4 string lands ~100 m off.
            let s_srs = if vrt.srs_wkt.is_empty() {
                gdal_cli::srs_wkt(&src.crs).unwrap_or_else(|| src.crs.clone())
            } else {
                vrt.srs_wkt.clone()
            };
            (rows, area / 1e6, s_srs)
        }
    };

    let parts = rows.len();
    write_csv(&csv, &rows)?;

    gdal_cli::run(
        "ogr2ogr",
        &[
            "-f",
            "GPKG",
            "-nln",
            "footprint",
            "-overwrite",
            "-s_srs",
            &s_srs,
            "-t_srs",
            "EPSG:4326",
            out.to_str().context("non-utf8 output path")?,
            csv.to_str().context("non-utf8 csv path")?,
        ],
    )?;

    let _ = std::fs::remove_file(&csv);

    Ok(Info {
        id: src.id.clone(),
        mode: src.footprint,
        parts,
        area_km2: (area_km2 * 10.0).round() / 10.0,
        bbox_area_km2: (bbox_area_km2(src.bbox) * 10.0).round() / 10.0,
        path: out.display().to_string(),
    })
}

pub fn run(doc: &Doc, root: &Path, only: Option<&str>) -> Result<()> {
    let wanted: Option<Vec<&str>> = only.map(|s| s.split(',').collect());

    let out_dir: PathBuf = root.join("footprints");
    let tmp_dir: PathBuf = root.join("tmp");
    std::fs::create_dir_all(&out_dir)?;
    std::fs::create_dir_all(&tmp_dir)?;

    let mut summary = Vec::new();
    for src in &doc.sources {
        if let Some(w) = &wanted
            && !w.contains(&src.id.as_str())
        {
            continue;
        }
        let info = build(src, &out_dir, &tmp_dir)?;
        let fill = 100.0 * info.area_km2 / info.bbox_area_km2;
        println!(
            "{:14} {:5} parts={:>6} area={:>10.1} km2  ({fill:5.1}% of bbox)",
            info.id,
            match info.mode {
                FootprintMode::Tiles => "tiles",
                FootprintMode::Bbox => "bbox",
            },
            info.parts,
            info.area_km2
        );
        summary.push(info);
    }

    let f = std::fs::File::create(out_dir.join("summary.json"))?;
    serde_json::to_writer_pretty(std::io::BufWriter::new(f), &summary)?;
    Ok(())
}
