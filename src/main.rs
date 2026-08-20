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
        Command::Cover { id } => {
            let s = find(&doc, &id)?;
            let tiles = grid::cover(&doc.grid, s.bbox);
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
            az,
            fov,
            alt_min,
            alt_max,
            step,
            range,
            edge_ratio,
            edge_hidden_ref,
            eye_level,
        } => {
            let p = panorama::Params {
                lon,
                lat,
                eye_height: eye,
                az_start: az,
                az_span: fov,
                alt_min,
                alt_max,
                step_deg: step,
                max_range: range,
                edge_ratio,
                edge_hidden_ref,
                eye_level,
            };
            let t0 = std::time::Instant::now();
            let buf = panorama::march(&cli.root, &doc, &p)?;
            let marched = t0.elapsed();
            panorama::render(&buf, &p, &out)?;

            let sky = buf.dist.iter().filter(|d| d.is_infinite()).count();
            println!(
                "viewpoint  {lon:.5} {lat:.5}  ground {:.1} m  eye {:.1} m",
                buf.eye_elevation - eye,
                buf.eye_elevation
            );
            println!(
                "buffer     {}x{} px  {:.0} deg from {:.0} ({})",
                buf.width,
                buf.height,
                fov,
                az,
                panorama::compass(az)
            );
            println!(
                "marched    {} samples, {} blocks cached, {:.2} s",
                buf.samples,
                buf.blocks,
                marched.as_secs_f64()
            );
            println!(
                "terrain    {:.1}% of the frame ({:.1}% sky)",
                100.0 * (buf.dist.len() - sky) as f64 / buf.dist.len() as f64,
                100.0 * sky as f64 / buf.dist.len() as f64
            );
            println!("wrote      {}", out.display());
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
