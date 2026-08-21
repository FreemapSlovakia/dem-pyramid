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
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::Doc;
use crate::panorama::Cancel;
use crate::queue::Queue;
use crate::{panorama, peaks};

/// Caps on what a single request may cost. Not entitlement -- that belongs to
/// the caller -- just a bound on how much work one request can demand.
const MAX_PIXELS: usize = 24_000_000;
const MAX_SUPERSAMPLE: u32 = 9;
const MIN_STEP: f64 = 0.02;

#[derive(Deserialize)]
pub struct Request {
    lon: f64,
    lat: f64,
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
    #[serde(default = "d_min_prom")]
    min_prominence: f64,
    /// Queue priority; higher goes first. Set by whoever authenticates the
    /// caller -- this service does not know what premium means.
    #[serde(default)]
    priority: i32,
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
fn d_min_prom() -> f64 { 0.05 }

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

async fn panorama_route(State(ctx): State<Ctx>, Json(req): Json<Request>) -> Response {
    let step = req.step.max(MIN_STEP);
    let fov = req.fov.clamp(0.1, 360.0);
    if req.alt_max <= req.alt_min {
        return bad("alt_max must exceed alt_min");
    }
    let width = (fov / step) as usize;
    let height = ((req.alt_max - req.alt_min) / step) as usize;
    if width * height > MAX_PIXELS {
        return bad(format!(
            "{width}x{height} exceeds the {MAX_PIXELS} pixel limit; raise step or narrow fov"
        ));
    }

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

    let queued = ctx.queue.depth();
    let _permit = ctx.queue.acquire(req.priority).await;

    // Whoever was waiting may have given up by the time the slot came free.
    if cancel.is_cancelled() {
        return StatusCode::REQUEST_TIMEOUT.into_response();
    }

    // Rendering is CPU-bound and blocking; keep it off the async runtime.
    let want_peaks = req.peaks && ctx.peaks_file.is_some();
    let peaks_file = ctx.peaks_file.clone();
    let root = ctx.root.clone();
    let doc = ctx.doc.clone();
    let depth_step = req.depth_step.max(1);
    let want_depth = req.depth;
    let min_prom = req.min_prominence;

    let render_cancel = cancel.clone();
    let built = tokio::task::spawn_blocking(move || -> Result<Vec<(String, Option<String>, Vec<u8>)>> {
        let cancel = render_cancel;
        let (img, stats) = panorama::render(&root, &doc, &p, &cancel)?;

        let mut found = Vec::new();
        if want_peaks {
            if let Some(pf) = &peaks_file {
                let mut cands = peaks::load(pf, p.lon, p.lat, p.max_range)?;
                panorama::resolve_peaks(&root, &doc, &p, &mut cands, &cancel)?;
                cands.retain(|k| {
                    k.visible
                        && k.column >= 0
                        && k.prominence >= min_prom
                        && k.y >= 0.0
                        && k.y <= stats.height as f64
                });
                cands.sort_by(|a, b| b.prominence.partial_cmp(&a.prominence).unwrap());
                found = cands;
            }
        }

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

        let mut parts: Vec<(String, Option<String>, Vec<u8>)> = Vec::new();
        parts.push(("meta".into(), None, serde_json::to_vec(&meta)?));

        let mut png = std::io::Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png)?;
        parts.push((
            "image".into(),
            Some("panorama.png".into()),
            png.into_inner(),
        ));

        if want_depth {
            let mut raw = Vec::with_capacity(stats.width * stats.height * 2);
            for row in 0..stats.height {
                let mut prev = 0i32;
                for col in 0..stats.width {
                    let v = stats.depth.get_pixel(col as u32, row as u32)[0];
                    let q = if v == 0 { 0 } else { (v / depth_step) * depth_step };
                    raw.extend_from_slice(&((i32::from(q) - prev) as i16).to_le_bytes());
                    prev = i32::from(q);
                }
            }
            let mut enc =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(&raw)?;
            parts.push((
                "depth".into(),
                Some("depth.bin.gz".into()),
                enc.finish()?,
            ));
        }
        Ok(parts)
    })
    .await;

    let parts = match built {
        Ok(Ok(p)) => p,
        Ok(Err(e)) if cancel.is_cancelled() => {
            // Nobody is listening; the status is for the log, not the client.
            let _ = e;
            return StatusCode::REQUEST_TIMEOUT.into_response();
        }
        Ok(Err(e)) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    // The render finished, so the work is no longer abandonable; keeping the
    // guard alive until here is what makes the cancellation window cover both
    // queueing and rendering.
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
