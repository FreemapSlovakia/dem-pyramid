//! Tooling for the Freemap elevation pyramid.
//!
//! The pyramid indexes the rasters terrain-tiles serves. This binary measures
//! them, checks the measurement against the real files, plans the index that
//! `bin/index.sh` writes, and renders from it.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod avif;
mod check;
mod config;
mod coverage;
mod credit;
mod gdal_cli;
mod gradient;
mod grid;
mod measure;
mod panorama;
mod peaks;
mod progress;
mod queue;
mod rank;
mod server;
mod viewshed;

#[derive(Parser)]
#[command(about, version)]
struct Cli {
    /// Finest and coarsest pyramid levels.
    #[arg(long, global = true, default_value_t = 14)]
    finest_level: u32,
    #[arg(long, global = true, default_value_t = 8)]
    coarsest_level: u32,

    /// Data root on the build host.
    #[arg(
        long,
        global = true,
        env = "DEM_ROOT",
        default_value = "/fm/storage2/dem"
    )]
    root: PathBuf,

    #[command(subcommand)]
    command: Command,
}

// Parsed once per process, so the panorama flags' size costs nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Command {
    /// The datasets the index serves.
    List,
    /// Machine-readable dump with defaults resolved.
    Json,
    /// Fail on anything that would stop `bin/index.sh` or `serve`, and say
    /// when the index lags the source list.
    Check {
        /// The elevation API's source list: a checkout of
        /// FreemapSlovakia/elevation-sources.
        #[arg(long, env = "ELEVATION_SOURCES_DIR", default_value = credit::DEFAULT_DIR)]
        elevation_sources: PathBuf,
    },
    /// Measure the source list's rasters and print every level's index rows,
    /// for `bin/index.sh`.
    IndexPlan {
        #[arg(long, env = "ELEVATION_SOURCES_DIR", default_value = credit::DEFAULT_DIR)]
        elevation_sources: PathBuf,
    },
    /// Serve panoramas over HTTP.
    Serve {
        #[arg(long, default_value = "127.0.0.1:3100")]
        listen: String,
        /// GeoPackage of candidate peaks.
        #[arg(long)]
        peaks: Option<PathBuf>,
        /// The elevation API's source tree, which is where every model's
        /// credit line lives. Serving refuses to start unless every source
        /// it serves is named there, with a credit.
        #[arg(long, env = "ELEVATION_SOURCES_DIR", default_value = credit::DEFAULT_DIR)]
        elevation_sources: PathBuf,
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
        /// Multiplier on the ridge silhouettes; 0 removes them.
        #[arg(long, default_value_t = 1.0)]
        ridge_strength: f64,
        /// Silhouette thickness, output pixels.
        #[arg(long, default_value_t = 1.0)]
        ridge_width: f64,
        /// Multiplier on the 8-bit dither; 0 turns it off.
        #[arg(long, default_value_t = panorama::DEFAULT_DITHER)]
        dither_strength: f64,
        /// Degrees of extra elevation for terrain at --range, tapering to
        /// nothing at the viewpoint. Unfolds distance the true projection
        /// compresses into the horizon; 0 is the true projection.
        ///
        /// Terrain rises clear of what hides it, so concealed ground comes
        /// into view. Peaks shown only that way are marked `revealed`.
        #[arg(long, default_value_t = 0.0)]
        depth_lift: f64,
        /// Silhouette colour, #rrggbb. Black reads as shading.
        #[arg(long)]
        ridge_color: Option<String>,
        /// Near-terrain colour, #rrggbb, before haze washes it out.
        #[arg(long)]
        ground_color: Option<String>,
        /// Distance-to-colour ramp for the ground, as JSON, replacing the
        /// built-in --ground-color haze. See docs/API.md.
        ///
        ///   {"far_distance": "auto",
        ///    "stops": [[0, "#3a4a34"], [0.45, "#6f89a0"], [1, "sky"]]}
        ///
        /// Positions run 0 at the eye to 1 at far_distance, which is either a
        /// number of metres or "auto" to measure the terrain in frame.
        ///
        /// "clip" drops terrain past far_distance instead of holding the last
        /// colour. Off by default, and ignored under "auto", which measures a
        /// percentile and so would always cut something.
        #[arg(long)]
        ground_gradient: Option<String>,
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
        /// Formula deciding which summits --max-peaks keeps, as a JSON prefix
        /// expression. See docs/API.md; omit for the built-in ranking.
        #[arg(long)]
        peak_rank: Option<String>,
        /// Which peaks are returned at all, same expression language.
        #[arg(long)]
        peak_filter: Option<String>,
        /// Degrees per column of the grid dominance is measured on, so the
        /// score does not change with --step or --supersample-x. Held to the
        /// ray spacing if finer.
        #[arg(long, default_value_t = panorama::DEFAULT_PROFILE_STEP)]
        peak_profile_step: f64,
        /// Keep at most this many peaks, most label-worthy first -- dominance
        /// discounted by distance, or by --peak-rank. 0 is no cap.
        #[arg(long, default_value_t = 0)]
        max_peaks: usize,
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

fn main() -> Result<()> {
    let cli = Cli::parse();

    let grid = config::Grid {
        finest_level: cli.finest_level,
        coarsest_level: cli.coarsest_level,
    };

    let (coarsest, finest) = (grid.coarsest_level, grid.finest_level);
    let index_dir = config::index_dir(&cli.root);
    let index = |grid| config::load(&index_dir, grid);

    match cli.command {
        Command::List => {
            let doc = index(grid)?;
            println!("{:14} {:>5}", "id", "prio");
            println!("{}", "-".repeat(20));
            for s in &doc.sources {
                println!("{:14} {:>5}", s.id, s.priority);
            }
        }
        Command::Json => {
            serde_json::to_writer_pretty(std::io::stdout(), &index(grid)?)?;
            println!();
        }
        Command::Check { elevation_sources } => {
            check::run(&index_dir, &elevation_sources, grid)?;
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
            ridge_strength,
            ridge_width,
            dither_strength,
            depth_lift,
            ridge_color,
            ground_color,
            ground_gradient,
            depth_out,
            depth_raw,
            depth_step,
            peaks: peaks_path,
            peaks_out,
            peak_rank,
            peak_filter,
            peak_profile_step,
            max_peaks,
            supersample_x,
            supersample_y,
        } => {
            panorama::validate_style(ridge_strength, ridge_width, depth_lift, peak_profile_step)?;
            // The server's parser refuses a non-finite before any check sees
            // it; clap's does not, so this is the side where `--range NaN`
            // reaches the geometry and renders an empty frame with exit 0.
            // `range` also has to be positive rather than merely finite: the
            // lift divides by it, so `--range 0` computes 0/0 and hands the
            // marcher a NaN. Both self-cancel today -- a march loop that never
            // runs cannot draw a NaN -- which is exactly the kind of accident
            // worth not depending on.
            for (name, v) in [
                ("lon", lon),
                ("lat", lat),
                ("az", az),
                ("fov", fov),
                ("step", step),
                ("alt-min", alt_min),
                ("alt-max", alt_max),
                ("eye", eye),
                ("eye-search-radius", eye_search_radius),
                ("range", range),
                ("dither-strength", dither_strength),
                // `edge-hidden-ref` divides the hidden extent and the result
                // goes through `clamp`, which passes a NaN straight out, so a
                // non-finite here inks no edge in the frame and the render
                // still exits 0. `edge-ratio` survives its own `max(1.001)`,
                // and is listed anyway rather than left as a puzzle.
                ("edge-ratio", edge_ratio),
                ("edge-hidden-ref", edge_hidden_ref),
            ] {
                anyhow::ensure!(v.is_finite(), "--{name} must be a finite number");
            }
            anyhow::ensure!(range > 0.0, "--range must be positive");
            // The frame the server refuses too: `render` divides by the height.
            panorama::frame(fov, alt_min, alt_max, step)?;
            // Compiled before the render, like the server does, so a typo
            // costs a message rather than twenty seconds of marching.
            let compile = |flag: &str, src: &Option<String>| -> Result<Option<rank::Program>> {
                let Some(src) = src else { return Ok(None) };
                let json = serde_json::from_str(src)
                    .with_context(|| format!("--{flag} is not valid JSON"))?;
                rank::Program::compile(&json)
                    .map(Some)
                    .map_err(|e| anyhow::anyhow!("--{flag}: {e}"))
            };
            let peak_rank = compile("peak-rank", &peak_rank)?;
            let peak_filter = compile("peak-filter", &peak_filter)?;
            // Same reason: a bad stop list should cost a message, not a march.
            let gradient = match &ground_gradient {
                Some(src) => {
                    let json: serde_json::Value =
                        serde_json::from_str(src).context("--ground-gradient is not valid JSON")?;
                    let g = gradient::Gradient::parse(&json)?;
                    gradient::validate(&g, range)?;
                    Some(g)
                }
                None => None,
            };
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
                ridge_strength,
                ridge_width,
                dither_strength,
                depth_lift,
                peak_profile_step,
                ridge_colour: match &ridge_color {
                    Some(s) => panorama::parse_colour(s)?,
                    None => panorama::DEFAULT_RIDGE,
                },
                ground_colour: match &ground_color {
                    Some(s) => panorama::parse_colour(s)?,
                    None => panorama::DEFAULT_GROUND,
                },
                gradient,
            };
            let doc = index(grid)?;
            let t0 = std::time::Instant::now();
            // Nothing cancels a CLI render; the flag exists for the server.
            let cancel = panorama::Cancel::default();
            let mut cands = match &peaks_path {
                Some(src) => peaks::load(src, lon, lat, range)?,
                None => Vec::new(),
            };
            let found = cands.len();
            let (img, stats) = panorama::render(&cli.root, &doc, &p, &cancel, &mut cands, None)?;
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
            // Where the ramp ended up, which is the one thing about a gradient
            // the caller cannot work out from what they sent when they asked
            // for "auto".
            if let Some(far) = stats.far_distance {
                println!("gradient   far {:.0} m ({:.1} km)", far, far / 1000.0);
            }
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
                let step = depth_step.max(1);
                std::fs::write(&rpath, panorama::depth_bytes(&stats.depth, step)?)?;
                let rel = f64::from(step)
                    * 100.0
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
                peaks::select(
                    &mut cands,
                    &peaks::Selection {
                        max_peaks,
                        height: stats.height,
                        rank: peak_rank,
                        filter: peak_filter,
                    },
                );

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
            elevation_sources,
        } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(server::serve(
                cli.root.clone(),
                index(grid)?,
                &elevation_sources,
                peaks_file,
                &listen,
            ))?;
        }
        Command::IndexPlan { elevation_sources } => {
            let entries = credit::load_entries(&elevation_sources)?;
            let (rasters, failed) = measure::rasters(&entries, coarsest);

            // All or nothing: an index silently missing a raster that was
            // merely unreadable for a moment has a hole in it.
            for (dir, why) in &failed {
                eprintln!("FAIL {dir}: {why}");
            }
            anyhow::ensure!(
                failed.is_empty(),
                "{} raster(s) could not be measured",
                failed.len()
            );
            measure::validate(&rasters)?;
            // Refused here rather than at the next restart, when `serve` would
            // stop over it with the new index already in place.
            credit::check_attributions(rasters.iter().map(|r| r.id.as_str()), &entries)?;

            for z in (coarsest..=finest).rev() {
                println!("RES\t{z}\t{}", config::level_res(z));
                for row in measure::rows(&rasters, z) {
                    println!("SRC\t{z}\t{}\t{}\t{}", row.id, row.priority, row.location);
                }
            }
        }
    }

    Ok(())
}
