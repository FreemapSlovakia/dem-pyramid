//! Tooling for the Freemap elevation pyramid.
//!
//! Orchestration lives in bash calling the GDAL command line tools; this binary
//! holds the parts that are actual logic -- config validation, drift checking
//! against the real files, and footprint extraction.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod check;
mod config;
mod footprints;
mod gdal_cli;
mod grid;
mod panorama;
mod peaks;
mod queue;
mod server;

#[derive(Parser)]
#[command(about, version)]
struct Cli {
    /// Path to sources.yaml (defaults to the one next to the binary's repo).
    #[arg(long, global = true)]
    sources: Option<PathBuf>,

    /// Data root on the build host.
    #[arg(long, global = true, env = "DEM_ROOT", default_value = "/fm/storage2/dem")]
    root: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Tabular overview of every source.
    List,
    /// Machine-readable dump with defaults resolved.
    Json,
    /// Re-measure every source with GDAL and fail on drift.
    Check,
    /// Regenerate the ELEVATION_SOURCES value for freemap.conf.
    ElevationSources,
    /// Build a coverage footprint per source.
    Footprints {
        /// Comma-separated source ids; default is all.
        #[arg(long)]
        only: Option<String>,
    },
    /// Print the tile grid constants.
    GridInfo,
    /// Extent and pixel size of one tile.
    Tile {
        tx: u32,
        ty: u32,
        #[arg(long)]
        level: Option<u32>,
    },
    /// Tiles a source's bbox intersects.
    Cover {
        id: String,
        /// Override the source's own bbox: lon0,lat0,lon1,lat1.
        ///
        /// Chiefly for the global fallback, which is better materialised once
        /// over a generous region than in buffers around each country.
        #[arg(long)]
        bbox: Option<String>,
    },
    /// Shell-sourceable build variables for one (source, tile).
    WarpEnv {
        id: String,
        tx: u32,
        ty: u32,
    },
    /// Leaf files referenced by a source's VRT, one per line.
    VrtSources {
        id: String,
    },
    /// What contributes to one pyramid level, for the index builder.
    IndexPlan {
        level: u32,
    },
    /// Serve panoramas over HTTP.
    Serve {
        #[arg(long, default_value = "127.0.0.1:3100")]
        listen: String,
        /// GeoPackage of candidate peaks.
        #[arg(long)]
        peaks: Option<PathBuf>,
    },
    /// Render a panorama from a viewpoint.
    Panorama {
        #[arg(long)]
        lon: f64,
        #[arg(long)]
        lat: f64,
        #[arg(long, default_value = "panorama.png")]
        out: PathBuf,
        /// Eye height above ground, metres.
        #[arg(long, default_value_t = 1.7)]
        eye: f64,
        /// Take ground elevation as the local maximum within this radius,
        /// metres, instead of the value at the exact point.
        ///
        /// Compensates for the pyramid storing a 6.27 m average, which costs a
        /// summit more the sharper it is. 0 disables.
        #[arg(long, default_value_t = 10.0)]
        eye_search_radius: f64,
        /// Leftmost azimuth, degrees clockwise from north.
        #[arg(long, default_value_t = 0.0)]
        az: f64,
        /// Horizontal field of view, degrees.
        #[arg(long, default_value_t = 360.0)]
        fov: f64,
        #[arg(long, default_value_t = -8.0)]
        alt_min: f64,
        #[arg(long, default_value_t = 12.0)]
        alt_max: f64,
        /// Degrees per pixel.
        #[arg(long, default_value_t = 0.05)]
        step: f64,
        /// Maximum range, metres.
        #[arg(long, default_value_t = 300_000.0)]
        range: f64,
        /// Depth ratio above which ridge-against-ridge edges are stroked.
        /// A huge value strokes only terrain-against-sky skylines.
        #[arg(long, default_value_t = 1.35)]
        edge_ratio: f64,
        /// Hidden extent, metres, at which a silhouette reaches full strength.
        #[arg(long, default_value_t = 20_000.0)]
        edge_hidden_ref: f64,
        /// Draw the eye-level line at 0 degrees.
        #[arg(long, default_value_t = false)]
        eye_level: bool,
        /// Also write a 16-bit greyscale depth image, log-encoded, 0 for sky.
        ///
        /// Lets the client answer "how far is that ridge" for any pixel.
        /// Recovering it from the rendered colour would not work: colour mixes
        /// haze, the sky gradient and silhouette ink, so the mapping is not
        /// invertible.
        #[arg(long)]
        depth_out: Option<PathBuf>,
        /// Write depth as a gzipped raw little-endian u16 buffer: log-scaled,
        /// row delta-coded, 0 for sky.
        ///
        /// Less client code than a PNG, not more -- fetch, pipe through
        /// DecompressionStream, wrap in an Int16Array. No canvas, so no
        /// colour-profile transform to worry about. Dimensions come from the
        /// render response.
        #[arg(long)]
        depth_raw: Option<PathBuf>,
        /// Quantise u16 depth to this step before delta-coding, trading
        /// precision for size. One step is 0.0162% of the distance, so step 4
        /// is +-13 m at 20 km and step 16 is +-52 m.
        ///
        /// This is where the size actually comes from: at matched precision,
        /// quantise + delta + gzip is about half of LERC, which is why LERC is
        /// not used despite the client already decoding it.
        #[arg(long, default_value_t = 4)]
        depth_step: u16,
        /// GeoPackage of candidate peaks; enables the JSON sidecar.
        #[arg(long)]
        peaks: Option<PathBuf>,
        /// Where to write the peak JSON. Defaults to the image path with a
        /// .json extension.
        #[arg(long)]
        peaks_out: Option<PathBuf>,
        /// Drop peaks whose angular prominence is below this, degrees.
        #[arg(long, default_value_t = 0.05)]
        min_prominence: f64,
        /// Rays per output pixel horizontally, averaged down.
        ///
        /// This is where supersampling earns its cost. At long range several
        /// DEM cells fall inside one pixel's angular footprint, and a single
        /// ray picks one arbitrary value out of them.
        #[arg(long, default_value_t = 9)]
        supersample_x: u32,
        /// Rows per output pixel vertically, averaged down.
        ///
        /// Needed as much as the horizontal factor, for a reason that is not
        /// obvious: sub-pixel placement is analytic, but only for *one* edge
        /// per cell. The buffer holds a single surface per cell, so where
        /// several ridge bands fall inside one output pixel -- routine near
        /// the horizon -- all but the nearest are discarded and never
        /// stroked. Extra rows give each band its own cell to occupy.
        #[arg(long, default_value_t = 9)]
        supersample_y: u32,
    },
}

fn find<'a>(doc: &'a config::Doc, id: &str) -> anyhow::Result<&'a config::Source> {
    doc.sources
        .iter()
        .find(|s| s.id == id)
        .ok_or_else(|| anyhow::anyhow!("unknown source id: {id}"))
}

fn default_sources() -> PathBuf {
    // Alongside the executable's repo root when run from the build dir, else
    // the current directory.
    let cwd = std::env::current_dir().unwrap_or_default().join("sources.yaml");
    if cwd.exists() {
        return cwd;
    }
    PathBuf::from("sources.yaml")
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let path = cli.sources.unwrap_or_else(default_sources);
    let doc = config::load(&path)?;

    match cli.command {
        Command::List => {
            println!(
                "{:14} {:>5} {:>6} {:>6} {:>12}  {:5} path",
                "id", "prio", "res", "finest", "nodata", "fp"
            );
            println!("{}", "-".repeat(96));
            for s in &doc.sources {
                println!(
                    "{:14} {:>5} {:>6} {:>6} {:>12}  {:5} {}",
                    s.id,
                    s.priority,
                    s.native_res,
                    s.finest_level,
                    s.nodata.to_string(),
                    match s.footprint {
                        config::FootprintMode::Tiles => "tiles",
                        config::FootprintMode::Bbox => "bbox",
                    },
                    s.path
                );
            }
        }
        Command::Json => {
            serde_json::to_writer_pretty(std::io::stdout(), &doc)?;
            println!();
        }
        Command::Check => check::run(&doc)?,
        Command::ElevationSources => {
            // freemap-v3-api is FIRST wins, so walk priority descending -- the
            // opposite of gdalbuildvrt's LAST wins.
            let lines: Vec<String> = doc
                .sources
                .iter()
                .map(|s| {
                    let [a, b, c, d] = s.bbox;
                    format!("{}:{}:{a},{b},{c},{d}", s.api_name, s.path)
                })
                .collect();
            println!("ELEVATION_SOURCES=\"{}\"", lines.join(";\n"));
        }
        Command::Footprints { only } => {
            footprints::run(&doc, &cli.root, only.as_deref())?;
        }
        Command::GridInfo => {
            let g = &doc.grid;
            println!("crs             {}", g.crs);
            println!("levels          z{}..z{}", g.coarsest_level, g.finest_level);
            println!("tile            {} px at z{}", g.tile_px, g.finest_level);
            println!("block           {} px", g.block_px);
            println!("tile span       {:.4} m (projected)", grid::tile_span(g));
            println!("world           {0}x{0} tiles", grid::tiles_across(g));
            for z in (g.coarsest_level..=g.finest_level).rev() {
                println!(
                    "  z{z:<3} {:>6} px/tile  {:>10.4} m projected  {:>8.2} m ground @49N",
                    grid::tile_px_at(g, z)?,
                    config::level_res(z),
                    config::ground_res(z, 49.0)
                );
            }
        }
        Command::Tile { tx, ty, level } => {
            let g = &doc.grid;
            let level = level.unwrap_or(g.finest_level);
            let [x0, y0, x1, y1] = grid::tile_extent(g, tx, ty);
            let (lon0, lat0) = grid::merc_to_lonlat(x0, y0);
            let (lon1, lat1) = grid::merc_to_lonlat(x1, y1);
            println!("tile     {tx}_{ty} at z{level}");
            println!("size     {0}x{0} px", grid::tile_px_at(g, level)?);
            println!("extent   {x0:.4} {y0:.4} {x1:.4} {y1:.4}");
            println!("lonlat   {lon0:.5} {lat0:.5} {lon1:.5} {lat1:.5}");
        }
        Command::Cover { id, bbox } => {
            let s = find(&doc, &id)?;
            let area = match bbox {
                Some(spec) => {
                    let v: Vec<f64> = spec
                        .split(',')
                        .map(|t| t.trim().parse::<f64>())
                        .collect::<Result<_, _>>()
                        .context("--bbox wants lon0,lat0,lon1,lat1")?;
                    anyhow::ensure!(v.len() == 4, "--bbox wants four numbers");
                    [v[0], v[1], v[2], v[3]]
                }
                None => s.bbox,
            };
            let tiles = grid::cover(&doc.grid, area);
            println!("{} tiles for {id}", tiles.len());
            for (tx, ty) in tiles {
                println!("{tx} {ty}");
            }
        }
        Command::WarpEnv { id, tx, ty } => {
            let s = find(&doc, &id)?;
            let root = cli.root.to_str().context("non-utf8 root")?;
            print!("{}", grid::warp_env(&doc.grid, s, tx, ty, root)?);
        }
        Command::VrtSources { id } => {
            let s = find(&doc, &id)?;
            let vrt = footprints::parse_vrt(&s.path)?;
            for f in vrt.files {
                println!("{f}");
            }
        }
        Command::Panorama {
            lon,
            lat,
            out,
            eye,
            eye_search_radius,
            az,
            fov,
            alt_min,
            alt_max,
            step,
            range,
            edge_ratio,
            edge_hidden_ref,
            eye_level,
            depth_out,
            depth_raw,
            depth_step,
            peaks: peaks_path,
            peaks_out,
            min_prominence,
            supersample_x,
            supersample_y,
        } => {
            let p = panorama::Params {
                lon,
                lat,
                eye_height: eye,
                eye_search_radius,
                az_start: az,
                az_span: fov,
                alt_min,
                alt_max,
                step_deg: step,
                max_range: range,
                edge_ratio,
                edge_hidden_ref,
                eye_level,
                supersample_x,
                supersample_y,
            };
            let t0 = std::time::Instant::now();
            // Nothing cancels a CLI render; the flag exists for the server.
            let cancel = panorama::Cancel::default();
            let mut cands = match &peaks_path {
                Some(src) => peaks::load(src, lon, lat, range)?,
                None => Vec::new(),
            };
            let found = cands.len();
            let (img, stats) = panorama::render(&cli.root, &doc, &p, &cancel, &mut cands)?;
            let elapsed = t0.elapsed();
            img.save(&out)
                .with_context(|| format!("writing {}", out.display()))?;

            println!(
                "viewpoint  {lon:.5} {lat:.5}  ground {:.1} m  eye {:.1} m",
                stats.eye_elevation - eye,
                stats.eye_elevation
            );
            println!(
                "image      {}x{} px  {:.0} deg from {:.0} ({})  supersample {}x{}",
                stats.width,
                stats.height,
                fov,
                az,
                panorama::compass(az),
                supersample_x,
                supersample_y
            );
            println!(
                "marched    {} samples, {} blocks cached, {:.2} s",
                stats.samples,
                stats.blocks,
                elapsed.as_secs_f64()
            );
            println!(
                "terrain    {:.1}% of the frame ({:.1}% sky)",
                100.0 * (1.0 - stats.sky_fraction),
                100.0 * stats.sky_fraction
            );
            println!("wrote      {}", out.display());

            if let Some(dpath) = depth_out {
                stats
                    .depth
                    .save(&dpath)
                    .with_context(|| format!("writing {}", dpath.display()))?;
                println!(
                    "depth      16-bit log scale, {:.0} m .. {:.0} km, 0 = sky",
                    panorama::DEPTH_NEAR,
                    panorama::DEPTH_FAR / 1000.0
                );
                println!("wrote      {}", dpath.display());
            }

            if let Some(rpath) = depth_raw {
                use std::io::Write;
                let step = depth_step.max(1);
                let mut bytes = Vec::with_capacity(stats.width * stats.height * 2);
                for row in 0..stats.height {
                    // Delta-code rows before deflating: this is where PNG gets
                    // its compression on smooth gradients, and without it a
                    // raw buffer loses to the PNG.
                    let mut prev = 0i32;
                    for col in 0..stats.width {
                        let v = stats.depth.get_pixel(col as u32, row as u32)[0];
                        // Keep 0 meaning sky rather than rounding it away.
                        let q = if v == 0 { 0 } else { (v / step) * step };
                        let d = (i32::from(q) - prev) as i16;
                        bytes.extend_from_slice(&d.to_le_bytes());
                        prev = i32::from(q);
                    }
                }
                let mut enc = flate2::write::GzEncoder::new(
                    std::fs::File::create(&rpath)?,
                    flate2::Compression::default(),
                );
                enc.write_all(&bytes)?;
                enc.finish()?;
                let rel = f64::from(step) * 100.0
                    * (panorama::DEPTH_FAR.ln() - panorama::DEPTH_NEAR.ln())
                    / 65534.0;
                println!(
                    "depth-raw  {}x{} u16 LE log-scaled, step {} (+-{:.0} m at 20 km), \
                     delta-coded, 0 = sky, gzip",
                    stats.width,
                    stats.height,
                    step,
                    20_000.0 * rel / 200.0
                );
                println!("wrote      {}", rpath.display());
            }

            if peaks_path.is_some() {
                // In frame, not hidden, and standing far enough above what is
                // behind it to be worth a label. Visibility was decided during
                // the render, from the columns it marched anyway.
                cands.retain(|k| {
                    k.visible
                        && k.column >= 0
                        && k.prominence >= min_prominence
                        && k.y >= 0.0
                        && k.y <= f64::from(stats.height as u32)
                });
                cands.sort_by(|a, b| b.prominence.partial_cmp(&a.prominence).unwrap());

                let dst = peaks_out.unwrap_or_else(|| out.with_extension("json"));
                serde_json::to_writer_pretty(
                    std::io::BufWriter::new(std::fs::File::create(&dst)?),
                    &cands,
                )?;
                println!(
                    "peaks      {found} in range, {} labelled, no extra rays",
                    cands.len()
                );
                println!("wrote      {}", dst.display());
            }
        }
        Command::Serve {
            listen,
            peaks: peaks_file,
        } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(server::serve(cli.root.clone(), doc, peaks_file, &listen))?;
        }
        Command::IndexPlan { level } => {
            let root = cli.root.to_str().context("non-utf8 root")?;
            println!("RES\t{}", config::level_res(level));
            println!("PX\t{}", grid::tile_px_at(&doc.grid, level)?);
            for s in &doc.sources {
                // A source contributes to every level at or coarser than the
                // one it was materialised at, via its overview chain. Finer
                // levels are simply absent -- the marcher falls back.
                if s.finest_level < level {
                    continue;
                }
                // -1 means the base image rather than an overview.
                let ovr = s.finest_level as i64 - level as i64 - 1;
                println!(
                    "SRC\t{}\t{}\t{ovr}\t{root}/norm/{}/{}",
                    s.id, s.priority, s.id, s.finest_level
                );
            }
        }
    }

    Ok(())
}
