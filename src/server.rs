//! HTTP endpoint.
//!
//! Policy-free on purpose: it renders whatever it is asked for, within limits
//! that exist to bound cost rather than to enforce entitlement. Whoever
//! proxies it -- freemap-v3-api, which already gates elevation sources on
//! premiumExpiration -- decides what each user may ask for.
//!
//! The response is one multipart/form-data body rather than URLs to fetch
//! separately. That skips a whole storage layer: no render id, no TTL, no
//! cleanup. The usual arguments for separate URLs -- parallel fetch, per
//! artifact caching -- buy little while nothing is cached, and a render that
//! took twenty seconds is not waiting on a second connection. When
//! precomputation arrives, cacheable GETs will earn their keep and can be
//! added alongside.

use anyhow::Result;
use axum::body::Body;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::Doc;
use crate::panorama::Cancel;
use crate::queue::{Queue, Rejected};
use crate::{avif, panorama, peaks};

/// Caps on what a single request may cost. Not entitlement -- that belongs to
/// the caller -- just a bound on how much work one request can demand.
const MAX_PIXELS: usize = 24_000_000;
const MAX_SUPERSAMPLE: u32 = 9;
const MIN_STEP: f64 = 0.02;

/// One multipart part: field name, optional filename, bytes.
type Part = (String, Option<String>, Vec<u8>);

#[derive(Deserialize)]
pub struct Request {
    lon: f64,
    lat: f64,
    /// Accepted only to warn about it. Priority moved to the `X-Priority`
    /// header because a body field cannot be overridden by the proxy; a caller
    /// still sending it here would otherwise have every premium user silently
    /// served at priority 0, with nothing to show why.
    #[serde(default)]
    priority: Option<i32>,
    /// Accepted only to warn about it. Renamed to `min_dominance` when the
    /// measure became signed; ignored here it would silently fall back to the
    /// 30 m default, which drops every negative-dominance peak -- the whole
    /// near field, and the reason the rename happened.
    #[serde(default)]
    min_prominence: Option<f64>,
    #[serde(default = "d_az")]
    az: f64,
    #[serde(default = "d_fov")]
    fov: f64,
    #[serde(default = "d_alt_min")]
    alt_min: f64,
    #[serde(default = "d_alt_max")]
    alt_max: f64,
    #[serde(default = "d_step")]
    step: f64,
    #[serde(default = "d_eye")]
    eye: f64,
    #[serde(default = "d_eye_radius")]
    eye_search_radius: f64,
    #[serde(default = "d_range")]
    range: f64,
    #[serde(default = "d_ss")]
    supersample_x: u32,
    #[serde(default = "d_ss")]
    supersample_y: u32,
    /// Include the depth buffer. Off by default: most viewers never hover, and
    /// it is by far the largest part of the response.
    #[serde(default)]
    depth: bool,
    #[serde(default = "d_depth_step")]
    depth_step: u16,
    #[serde(default = "d_peaks")]
    peaks: bool,
    #[serde(default = "d_min_dom")]
    min_dominance: f64,
    /// Keep at most this many peaks, the most dominant first. 0 is no cap.
    #[serde(default)]
    max_peaks: usize,
    /// Multiplier on the ridge silhouettes; 0 removes them.
    #[serde(default = "d_ridge_strength")]
    ridge_strength: f64,
    /// Stroke thickness in output pixels.
    #[serde(default = "d_ridge_width")]
    ridge_width: f64,
    /// `#rrggbb` for the silhouettes; black by default, which reads as shading.
    #[serde(default)]
    ridge_color: Option<String>,
    /// `#rrggbb` for near terrain, before haze washes it towards the sky.
    #[serde(default)]
    ground_color: Option<String>,
    /// `avif` or `png`. AVIF is fifteen to thirty times smaller for the same
    /// picture; PNG is here for callers that predate it.
    #[serde(default)]
    format: Format,
    /// AVIF quality, 1-100. Ignored for PNG, which is lossless.
    #[serde(default = "d_quality")]
    quality: u8,
}

#[derive(Deserialize, Clone, Copy, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Avif,
    Png,
}

fn d_az() -> f64 { 0.0 }
fn d_fov() -> f64 { 360.0 }
fn d_alt_min() -> f64 { -18.0 }
fn d_alt_max() -> f64 { 12.0 }
fn d_step() -> f64 { 0.05 }
fn d_eye() -> f64 { 1.7 }
fn d_eye_radius() -> f64 { 10.0 }
fn d_range() -> f64 { 300_000.0 }
fn d_ss() -> u32 { 9 }
fn d_depth_step() -> u16 { 4 }
fn d_peaks() -> bool { true }
fn d_min_dom() -> f64 { 30.0 }
fn d_ridge_strength() -> f64 { 1.0 }
fn d_ridge_width() -> f64 { 1.0 }
fn d_quality() -> u8 { avif::QUALITY }

#[derive(Clone)]
pub struct Ctx {
    root: PathBuf,
    doc: Arc<Doc>,
    peaks_file: Option<PathBuf>,
    /// One render at a time: a single render already saturates nine cores, so
    /// overlapping them trades latency for nothing. Priority-ordered, so a
    /// premium request does not queue behind anonymous ones.
    queue: Queue,
}

pub async fn serve(
    root: PathBuf,
    doc: Doc,
    peaks_file: Option<PathBuf>,
    listen: &str,
) -> Result<()> {
    let ctx = Ctx {
        root,
        doc: Arc::new(doc),
        peaks_file,
        queue: Queue::new(),
    };

    let app = Router::new()
        .route("/panorama", post(panorama_route))
        .route("/health", axum::routing::get(|| async { "ok" }))
        .with_state(ctx);

    let listener = tokio::net::TcpListener::bind(listen).await?;
    println!("listening on {listen}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

fn bad(msg: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, msg.to_string()).into_response()
}

/// Queue priority, from a header rather than the body.
///
/// A body field would be client-controlled: anyone could POST
/// `priority: 2147483647` and outrank every real premium user for ever. The
/// public vhost sets `X-Priority: 0` unconditionally, which overwrites
/// whatever a caller sent, so only something reaching the service on loopback
/// -- freemap-v3-api, having authenticated the user -- can raise it.
fn priority_of(headers: &header::HeaderMap) -> i32 {
    headers
        .get("x-priority")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<i32>().ok())
        .unwrap_or(0)
}

async fn panorama_route(
    State(ctx): State<Ctx>,
    headers: header::HeaderMap,
    Json(req): Json<Request>,
) -> Response {
    if req.priority.is_some() {
        eprintln!(
            "warning: request carried a body `priority` field, which is ignored; \
             set the X-Priority header instead"
        );
    }
    // Rejected rather than warned about. A warning goes to our stderr, where
    // the caller cannot see it, and they would get a 200 carrying peaks
    // filtered at the 30 m default -- the whole negative range gone, with
    // nothing in the response to say why. That silent failure is the thing the
    // rename existed to prevent, so it has to be visible from the client side.
    if let Some(v) = req.min_prominence {
        return bad(format!(
            "`min_prominence` was removed; use `min_dominance` (metres, signed, \
             may be negative). You sent {v}"
        ));
    }

    // Validate before arithmetic. `as usize` saturates rather than failing and
    // the product then wraps in release, so an absurd alt range sails past the
    // pixel limit and reaches the allocator -- one request aborting the
    // process on a service that is otherwise carefully bounded.
    for (name, v) in [
        ("lon", req.lon),
        ("lat", req.lat),
        ("az", req.az),
        ("fov", req.fov),
        ("step", req.step),
        ("alt_min", req.alt_min),
        ("alt_max", req.alt_max),
    ] {
        if !v.is_finite() {
            return bad(format!("{name} must be a finite number"));
        }
    }
    if let Err(e) = panorama::validate_style(req.ridge_strength, req.ridge_width) {
        return bad(e.to_string());
    }
    if !(-90.0..=90.0).contains(&req.alt_min) || !(-90.0..=90.0).contains(&req.alt_max) {
        return bad("alt_min and alt_max must lie within -90..90");
    }
    if req.alt_max <= req.alt_min {
        return bad("alt_max must exceed alt_min");
    }

    let step = req.step.max(MIN_STEP);
    let fov = req.fov.clamp(0.1, 360.0);
    // Rounded, not truncated, to match what `render` will actually allocate --
    // otherwise the dimensions validated here are up to a row and a column
    // short of the buffers the limit is meant to bound, and the message quotes
    // a size the caller never asked for.
    let width = (fov / step).round() as usize;
    let height = ((req.alt_max - req.alt_min) / step).round() as usize;
    if width.checked_mul(height).is_none_or(|n| n > MAX_PIXELS) {
        return bad(format!(
            "{width}x{height} exceeds the {MAX_PIXELS} pixel limit; raise step or narrow fov"
        ));
    }

    let colour = |field: &str, given: &Option<String>, fallback| match given {
        Some(s) => panorama::parse_colour(s).map_err(|e| format!("{field}: {e}")),
        None => Ok(fallback),
    };
    let (ridge_colour, ground_colour) = match (
        colour("ridge_color", &req.ridge_color, panorama::DEFAULT_RIDGE),
        colour("ground_color", &req.ground_color, panorama::DEFAULT_GROUND),
    ) {
        (Ok(r), Ok(g)) => (r, g),
        (Err(e), _) | (_, Err(e)) => return bad(e),
    };

    let p = panorama::Params {
        lon: req.lon,
        lat: req.lat,
        eye_height: req.eye,
        eye_search_radius: req.eye_search_radius.clamp(0.0, 200.0),
        az_start: req.az,
        az_span: fov,
        alt_min: req.alt_min,
        alt_max: req.alt_max,
        step_deg: step,
        max_range: req.range.clamp(1_000.0, 400_000.0),
        edge_ratio: 1.35,
        edge_hidden_ref: 20_000.0,
        eye_level: false,
        supersample_x: req.supersample_x.clamp(1, MAX_SUPERSAMPLE),
        supersample_y: req.supersample_y.clamp(1, MAX_SUPERSAMPLE),
        // Unbounded above: the composite clamps alpha to 1 anyway, so a large
        // value only makes the linework solid and costs nothing. A ceiling
        // here would silently rewrite the caller's number instead. Negative is
        // rejected above as meaningless -- alpha clamps at zero too, so it
        // would draw exactly what 0 draws.
        ridge_strength: req.ridge_strength,
        ridge_width: req.ridge_width,
        ridge_colour,
        ground_colour,
    };

    // Cancellation has to be cooperative: a blocking task cannot be killed,
    // and dropping its JoinHandle only detaches it. This guard is owned by the
    // handler future, so if the client hangs up -- while queued or mid-render
    // -- axum drops the future, the guard drops, and the render sees the flag
    // and abandons the work.
    struct Guard(Cancel);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }
    let cancel = Cancel::default();
    let guard = Guard(cancel.clone());

    let (permit, queued) = match ctx.queue.acquire(priority_of(&headers)).await {
        Ok(v) => v,
        Err(Rejected::Full) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "render queue is full; try again shortly",
            )
                .into_response();
        }
        Err(Rejected::ShuttingDown) => {
            return (StatusCode::SERVICE_UNAVAILABLE, "shutting down").into_response();
        }
    };

    // Rendering is CPU-bound and blocking; keep it off the async runtime.
    let want_peaks = req.peaks && ctx.peaks_file.is_some();
    let peaks_file = ctx.peaks_file.clone();
    let root = ctx.root.clone();
    let doc = ctx.doc.clone();
    let depth_step = req.depth_step.max(1);
    let want_depth = req.depth;
    let format = req.format;
    let quality = req.quality.clamp(1, 100);
    let min_dom = req.min_dominance;
    let max_peaks = req.max_peaks;

    let render_cancel = cancel.clone();
    let built = tokio::task::spawn_blocking(move || -> Result<Vec<Part>> {
        // The permit lives here, not in the handler. Handler locals drop in
        // reverse declaration order, so a permit held there would be released
        // the instant the client hung up -- admitting the next render while
        // this one is still marching at full width and has not yet noticed the
        // cancel flag. Holding it until the blocking task actually returns is
        // what keeps "one render at a time" true.
        let _permit = permit;
        let cancel = render_cancel;
        // Candidates go in before marching, so the render answers them from
        // the columns it produces anyway.
        let mut found = match (want_peaks, &peaks_file) {
            (true, Some(pf)) => peaks::load(pf, p.lon, p.lat, p.max_range)?,
            _ => Vec::new(),
        };
        let (img, stats) = panorama::render(&root, &doc, &p, &cancel, &mut found)?;

        peaks::select(&mut found, min_dom, max_peaks, stats.height);

        let meta = serde_json::json!({
            "width": stats.width,
            "height": stats.height,
            "eye_elevation": stats.eye_elevation,
            "az_start": p.az_start,
            "fov": p.az_span,
            "alt_min": p.alt_min,
            "alt_max": p.alt_max,
            "step_deg": p.step_deg,
            "samples": stats.samples,
            "depth": want_depth.then(|| serde_json::json!({
                "encoding": "u16-le log, row delta-coded, gzip",
                "near_m": panorama::DEPTH_NEAR,
                "far_m": panorama::DEPTH_FAR,
                "step": depth_step,
                "sky": 0,
            })),
            "peaks": found,
        });

        let mut parts: Vec<Part> = Vec::new();
        parts.push(("meta".into(), None, serde_json::to_vec(&meta)?));

        let (name, bytes) = match format {
            Format::Avif => ("panorama.avif", avif::encode(&img, quality, avif::SPEED)?),
            Format::Png => {
                let mut png = std::io::Cursor::new(Vec::new());
                img.write_to(&mut png, image::ImageFormat::Png)?;
                ("panorama.png", png.into_inner())
            }
        };
        parts.push(("image".into(), Some(name.into()), bytes));

        if want_depth {
            parts.push((
                "depth".into(),
                Some("depth.bin.gz".into()),
                panorama::depth_bytes(&stats.depth, depth_step)?,
            ));
        }
        Ok(parts)
    })
    .await
    // Log inside the task, around the whole of it: peak resolution can be
    // cancelled too, and once the client has hung up nothing after the await
    // runs, so this is the only place any of it can be observed.
    .inspect(|outcome| {
        if let Err(e) = outcome {
            if cancel.is_cancelled() {
                eprintln!("render abandoned: client hung up ({e})");
            }
        }
    });

    // Only reachable while the client is still connected: if it hangs up, axum
    // drops this future and none of the code below runs. Cancellation is
    // therefore reported by the blocking task's own logging, not from here.
    let parts = match built {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            eprintln!("render failed: {e:#}");
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
        Err(e) => {
            eprintln!("render task failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    // Held until here so the cancellation window covers queueing and rendering
    // alike.
    drop(guard);

    let boundary = "dempyramid7f3a9c2e";
    let mut body = Vec::new();
    for (name, filename, data) in parts {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        match &filename {
            // A part with a filename arrives as a Blob; without one, as a
            // string. That is what lets `meta` come back parseable directly.
            Some(f) => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\n\
                     Content-Type: application/octet-stream\r\n\r\n"
                )
                .as_bytes(),
            ),
            None => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"\r\n\
                     Content-Type: application/json\r\n\r\n"
                )
                .as_bytes(),
            ),
        }
        body.extend_from_slice(&data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    (
        [
            (
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            ),
            (
                header::HeaderName::from_static("x-queue-depth"),
                queued.to_string(),
            ),
        ],
        Body::from(body),
    )
        .into_response()
}
