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
use crate::progress::{Job, Jobs, Phase, Registration};
use crate::queue::{Queue, Rejected};
use crate::{avif, panorama, peaks, viewshed};

/// Caps on what a single request may cost. Not entitlement -- that belongs to
/// the caller -- just a bound on how much work one request can demand.
const MAX_PIXELS: usize = 24_000_000;
const MAX_SUPERSAMPLE: u32 = 9;
const MIN_STEP: f64 = 0.02;
/// Beyond this a viewshed is mostly answering questions about the curvature of
/// the earth, and the cost grows with the square of it.
const MAX_VIEWSHED_RADIUS: f64 = 300_000.0;
/// Separate from `MAX_PIXELS`, which the panorama also uses. Four times it, so
/// a viewshed of any given radius may be drawn at twice the resolution.
///
/// A square, so the linear factor is what a caller feels: at the cap the image
/// is 9797 px a side.
///
/// Measured, because reasoning about it undercounted by eightfold. The
/// renderer's own buffers are the small part -- one byte per pixel of alpha
/// while the rays run, four once it is RGBA, so about 480 MB. Encoding is the
/// rest: `avif::write_fast_png` builds the whole PNG in memory while the RGBA
/// image is still live, writes it to a `/tmp` that `PrivateTmp=yes` puts on a
/// tmpfs -- so that file is RAM as well -- and `avifenc` then decodes it and
/// converts 96 megapixels to 10-bit 4:4:4. A full-size request measured
/// **3.3 GB resident plus 0.8 GB of tmpfs**, against 45 GB free on fm6.
///
/// Affordable, and renders are serialised so it is one request's worth rather
/// than everyone's -- but unguarded it would abort the process rather than
/// fail the request, taking the queue with it. `terrain.service` carries a
/// `MemoryMax` for that reason; raise this cap and raise that with it.
const MAX_VIEWSHED_PIXELS: usize = 96_000_000;
/// A warm tint that reads over both map and imagery; opacity carries the
/// detail, so the colour is deliberately flat.
const DEFAULT_VIEWSHED_COLOUR: (f64, f64, f64) = (255.0, 214.0, 102.0);
/// How often progress is sent. Fast enough to feel live, slow enough that a
/// queued client is not woken four times a second to be told nothing changed.
const TICK_MS: u64 = 250;
/// How long an unknown token is given to appear before the stream gives up --
/// the client may well subscribe before its own request has been accepted.
const UNKNOWN_TICKS: u32 = 40;

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
    /// Keep at most this many peaks, most label-worthy first -- dominance
    /// discounted by distance, see `peak_rank_power`. 0 is no cap.
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
    /// Multiplier on the 8-bit dither. 0 disables it, which is how to tell
    /// whether a gradient artefact is dithering or something else.
    #[serde(default = "d_dither")]
    dither_strength: f64,
    /// Degrees of extra elevation given to terrain at `range`, tapering to
    /// nothing at the viewpoint. Spreads out distance that the true projection
    /// compresses into the horizon. 0, the default, is the true projection.
    ///
    /// Terrain rises clear of what hides it, so ground the eye cannot see
    /// comes into view and the picture stops being a photograph. Peaks that
    /// only appear this way come back with `revealed: true`.
    #[serde(default)]
    depth_lift: f64,
    /// Whether summits only `depth_lift` brought into view may take label
    /// slots. Nothing is revealed without a lift, so this changes nothing at
    /// the default.
    #[serde(default = "d_true")]
    revealed_peaks: bool,
    /// Exponent on distance when `max_peaks` decides which summits to keep.
    /// 0 ranks on dominance alone, which is what this did before.
    #[serde(default = "d_rank_power")]
    peak_rank_power: f64,
    /// Degrees per column of the grid `dominance` is measured on, independent
    /// of `step` and `supersample_x`. Held to the ray spacing if finer; the
    /// value used comes back as `meta.peak_profile_step`.
    #[serde(default = "d_profile_step")]
    peak_profile_step: f64,
    /// Formula deciding which summits `max_peaks` keeps, as a MapLibre-shaped
    /// JSON prefix expression. Overrides `peak_rank_power`. See `rank`.
    #[serde(default)]
    peak_rank: Option<serde_json::Value>,
}

fn d_true() -> bool { true }
fn d_rank_power() -> f64 { peaks::DEFAULT_RANK_POWER }
fn d_profile_step() -> f64 { panorama::DEFAULT_PROFILE_STEP }
fn d_gamma() -> f64 { 1.0 }

#[derive(Deserialize, Clone, Copy, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Avif,
    Png,
}

#[derive(Deserialize)]
pub struct ViewshedRequest {
    lon: f64,
    lat: f64,
    /// How far to look, ground metres.
    #[serde(default = "d_vs_radius")]
    radius: f64,
    /// Ground metres per pixel. With `radius` this fixes the image size, and
    /// the two are checked together -- either alone looks harmless.
    #[serde(default = "d_vs_scale")]
    scale: f64,
    #[serde(default = "d_eye")]
    eye: f64,
    #[serde(default = "d_eye_radius")]
    eye_search_radius: f64,
    /// Height above ground of what is being looked at; 0 is the ground.
    #[serde(default)]
    target_height: f64,
    /// Curve on the overlay's opacity: `alpha ** (1/gamma)`, 0.1–10. Above 1
    /// lifts grazing ground, which is most of a large viewshed, without
    /// driving the near field solid the way a plain gain would.
    #[serde(default = "d_gamma")]
    gamma: f64,
    /// Least opacity visible ground may take, 0–1. A stencil rather than a
    /// shading. Hidden ground stays fully transparent regardless.
    #[serde(default)]
    alpha_floor: f64,
    /// `#rrggbb` for the visible area. Opacity carries the detail, so a flat
    /// colour is the point.
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    format: Format,
    #[serde(default = "d_quality")]
    quality: u8,
}

fn d_vs_radius() -> f64 { 30_000.0 }
fn d_vs_scale() -> f64 { 20.0 }

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
fn d_dither() -> f64 { panorama::DEFAULT_DITHER }

#[derive(Clone)]
pub struct Ctx {
    root: PathBuf,
    doc: Arc<Doc>,
    peaks_file: Option<PathBuf>,
    /// One render at a time: a single render already saturates nine cores, so
    /// overlapping them trades latency for nothing. Priority-ordered, so a
    /// premium request does not queue behind anonymous ones.
    queue: Queue,
    jobs: Jobs,
}

/// Register the caller's `X-Job` token, if it sent one.
///
/// The token is the client's to invent; the server only needs it to be short
/// and unique. Absent, everything works exactly as before -- progress is
/// something a caller opts into.
fn job_for(jobs: &Jobs, headers: &header::HeaderMap) -> (Option<Arc<Job>>, Option<Registration>) {
    let Some(token) = headers.get("x-job").and_then(|v| v.to_str().ok()) else {
        return (None, None);
    };
    match jobs.register(token.trim()) {
        Some((job, reg)) => (Some(job), Some(reg)),
        None => (None, None),
    }
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
        jobs: Jobs::new(),
    };

    let app = Router::new()
        .route("/panorama", post(panorama_route))
        .route("/viewshed", post(viewshed_route))
        .route("/progress/{token}", axum::routing::get(progress_route))
        .route("/health", axum::routing::get(|| async { "ok" }))
        .with_state(ctx);

    if !avif::available() {
        eprintln!(
            "warning: avifenc not found, so every request will fail unless it \
             asks for format=png -- install libavif-bin"
        );
    }

    let listener = tokio::net::TcpListener::bind(listen).await?;
    println!("listening on {listen}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// Media type for a part, from the name it is sent under.
fn content_type(filename: &str) -> &'static str {
    match filename.rsplit('.').next() {
        Some("avif") => "image/avif",
        Some("png") => "image/png",
        Some("gz") => "application/gzip",
        _ => "application/octet-stream",
    }
}

/// Live progress for a token, as server-sent events.
///
/// Opened alongside the request rather than instead of it: the render still
/// answers on its own connection, and this only reports. It may be opened
/// before the request lands, so an unknown token waits rather than 404s --
/// otherwise the client would have to race its own two connections.
async fn progress_route(
    State(ctx): State<Ctx>,
    axum::extract::Path(token): axum::extract::Path<String>,
) -> Response {
    use axum::response::sse::{Event, KeepAlive, Sse};

    struct Watch {
        waited: u32,
        seen: bool,
        finished: bool,
    }

    let jobs = ctx.jobs.clone();
    let stream = futures_util::stream::unfold(
        Watch {
            waited: 0,
            seen: false,
            finished: false,
        },
        move |mut st| {
            let jobs = jobs.clone();
            let token = token.clone();
            async move {
                if st.finished {
                    return None;
                }
                tokio::time::sleep(std::time::Duration::from_millis(TICK_MS)).await;
                let body = match jobs.get(&token) {
                    Some(job) => {
                        st.seen = true;
                        let snap = job.snapshot();
                        st.finished = snap.phase == Phase::Done;
                        snap.to_json(st.finished)
                    }
                    // Gone after having been seen: the request finished and
                    // took its registration with it. That is the ordinary
                    // ending, and it usually arrives before the last tick can
                    // catch the job reporting `done` itself.
                    None if st.seen => {
                        st.finished = true;
                        serde_json::json!({
                            "phase": "done", "ahead": 0, "percent": 100, "final": true
                        })
                    }
                    // Never seen: it may not have arrived yet, since a client
                    // has to subscribe before or alongside its own request.
                    // Wait a while, then give up -- and say so, because the
                    // request may have been rejected before it ever
                    // registered, and a client waiting for `done` would
                    // reconnect for ever.
                    None => {
                        st.waited += 1;
                        st.finished = st.waited > UNKNOWN_TICKS;
                        serde_json::json!({
                            "phase": "unknown", "ahead": 0, "percent": 0, "final": st.finished
                        })
                    }
                };
                let event = Event::default().data(body.to_string());
                Some((Ok::<_, std::convert::Infallible>(event), st))
            }
        },
    );

    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

/// Cancellation has to be cooperative: a blocking task cannot be killed, and
/// dropping its JoinHandle only detaches it. This guard is owned by the
/// handler future, so if the client hangs up -- while queued or mid-render --
/// axum drops the future, the guard drops, and the work sees the flag and
/// abandons itself.
struct CancelOnDrop(Cancel);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
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
    // Every numeric field, not only the ones feeding the pixel count.
    //
    // A guard rather than a live fix: serde_json refuses the `NaN` and
    // `Infinity` literals and rejects out-of-range exponents, so today nothing
    // gets a non-finite past the parser. It is cheap insurance on a property
    // the geometry quietly depends on and the clamps do not provide -- a
    // `f64::clamp` returns NaN for a NaN input, so an unchecked field would
    // carry it straight into the marcher and answer 200 with an empty
    // picture. The CLI, which parses with `f64::from_str` and accepts "NaN",
    // does exactly that: `--range NaN` renders 100% sky and exits 0.
    //
    // Cheaper here than a debate later about whether some future entry point
    // -- a different body format, a query string, an internal caller -- has
    // the same parser guarantees.
    for (name, v) in [
        ("lon", req.lon),
        ("lat", req.lat),
        ("az", req.az),
        ("fov", req.fov),
        ("step", req.step),
        ("alt_min", req.alt_min),
        ("alt_max", req.alt_max),
        ("eye", req.eye),
        ("eye_search_radius", req.eye_search_radius),
        ("range", req.range),
        ("min_dominance", req.min_dominance),
        ("dither_strength", req.dither_strength),
        ("peak_rank_power", req.peak_rank_power),
    ] {
        if !v.is_finite() {
            return bad(format!("{name} must be a finite number"));
        }
    }
    if let Err(e) = panorama::validate_style(
        req.ridge_strength,
        req.ridge_width,
        req.depth_lift,
        req.peak_profile_step,
    ) {
        return bad(e.to_string());
    }
    if !(0.0..=peaks::MAX_RANK_POWER).contains(&req.peak_rank_power) {
        // Negative would invert the weighting -- distance would *raise* a
        // score -- and quietly return the most remote summits in the data.
        return bad(format!(
            "peak_rank_power must lie within 0..{}",
            peaks::MAX_RANK_POWER
        ));
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
        dither_strength: req.dither_strength.clamp(0.0, 8.0),
        depth_lift: req.depth_lift,
        peak_profile_step: req.peak_profile_step,
    };

    let cancel = Cancel::default();
    let guard = CancelOnDrop(cancel.clone());
    let (job, _registration) = job_for(&ctx.jobs, &headers);

    let (permit, queued) = match ctx.queue.acquire(priority_of(&headers), job.clone()).await {
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
    let keep_revealed = req.revealed_peaks;
    let rank_power = req.peak_rank_power;
    // Compiled here, before a render slot is taken, so a bad formula costs the
    // caller a 400 rather than costing everyone twenty seconds of queue.
    let rank = match &req.peak_rank {
        Some(j) => match crate::rank::Program::compile(j) {
            Ok(p) => Some(p),
            Err(e) => return bad(format!("peak_rank: {e}")),
        },
        None => None,
    };

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
        if let Some(job) = &job {
            job.set_phase(Phase::Rendering);
        }
        // Candidates go in before marching, so the render answers them from
        // the columns it produces anyway.
        let mut found = match (want_peaks, &peaks_file) {
            (true, Some(pf)) => peaks::load(pf, p.lon, p.lat, p.max_range)?,
            _ => Vec::new(),
        };
        let (img, stats) = panorama::render(&root, &doc, &p, &cancel, &mut found, job.as_deref())?;
        if let Some(job) = &job {
            job.set_phase(Phase::Encoding);
        }

        peaks::select(
            &mut found,
            &peaks::Selection {
                min_dominance: min_dom,
                max_peaks,
                height: stats.height,
                keep_revealed,
                rank_power,
                rank,
            },
        );

        let meta = serde_json::json!({
            "width": stats.width,
            "height": stats.height,
            "eye_elevation": stats.eye_elevation,
            "az_start": p.az_start,
            "fov": p.az_span,
            "alt_min": p.alt_min,
            "alt_max": p.alt_max,
            "step_deg": p.step_deg,
            // What `dominance` was actually measured on, after being held to
            // the ray spacing. Two requests return the same peaks only if
            // they agree here, so a client rendering a view twice can check
            // rather than assume.
            "peak_profile_step": stats.peak_profile_step,
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

        // Encoding is seconds of work holding the render permit, so a client
        // that hung up during the march must not buy the next one a wait.
        anyhow::ensure!(!cancel.is_cancelled(), "cancelled");
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
        if let Some(job) = &job {
            job.set_phase(Phase::Done);
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

    multipart(parts, queued)
}

async fn viewshed_route(
    State(ctx): State<Ctx>,
    headers: header::HeaderMap,
    Json(req): Json<ViewshedRequest>,
) -> Response {
    for (name, v) in [
        ("lon", req.lon),
        ("lat", req.lat),
        ("radius", req.radius),
        ("scale", req.scale),
        ("eye", req.eye),
        ("eye_search_radius", req.eye_search_radius),
        ("target_height", req.target_height),
        ("gamma", req.gamma),
        ("alpha_floor", req.alpha_floor),
    ] {
        if !v.is_finite() {
            return bad(format!("{name} must be a finite number"));
        }
    }
    // Rejected rather than clamped: a gamma of 0 is a division by zero and a
    // negative one inverts the picture, showing least where the eye sees most.
    if !(0.1..=10.0).contains(&req.gamma) {
        return bad("gamma must lie within 0.1..10");
    }
    if !(0.0..=1.0).contains(&req.alpha_floor) {
        return bad("alpha_floor must lie within 0..1");
    }
    if req.radius <= 0.0 || req.radius > MAX_VIEWSHED_RADIUS {
        return bad(format!("radius must lie within 0..{MAX_VIEWSHED_RADIUS} m"));
    }
    if req.scale <= 0.0 {
        return bad("scale must be positive");
    }

    // Checked together, because neither is alarming alone: a 100 km radius is
    // reasonable, 5 m per pixel is reasonable, and asking for both is a
    // 40000 x 40000 raster.
    let px = viewshed::extent(req.radius, req.scale);
    if px.checked_mul(px).is_none_or(|n| n > MAX_VIEWSHED_PIXELS) {
        return bad(format!(
            "{px}x{px} is {} pixels, over the {MAX_VIEWSHED_PIXELS} limit; \
             raise scale or reduce radius",
            px.saturating_mul(px)
        ));
    }

    let colour = match &req.color {
        Some(s) => match panorama::parse_colour(s) {
            Ok(c) => c,
            Err(e) => return bad(format!("color: {e}")),
        },
        None => DEFAULT_VIEWSHED_COLOUR,
    };

    let p = viewshed::Params {
        lon: req.lon,
        lat: req.lat,
        eye_height: req.eye,
        eye_search_radius: req.eye_search_radius.clamp(0.0, 200.0),
        radius: req.radius,
        scale: req.scale,
        target_height: req.target_height.clamp(0.0, 1000.0),
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        colour: (colour.0 as u8, colour.1 as u8, colour.2 as u8),
        gamma: req.gamma,
        alpha_floor: req.alpha_floor,
    };

    let cancel = Cancel::default();
    let _guard = CancelOnDrop(cancel.clone());
    let (job, _registration) = job_for(&ctx.jobs, &headers);
    let (permit, queued) = match ctx.queue.acquire(priority_of(&headers), job.clone()).await {
        Ok(v) => v,
        Err(Rejected::Full) => {
            return (StatusCode::SERVICE_UNAVAILABLE, "queue full").into_response();
        }
        Err(Rejected::ShuttingDown) => {
            return (StatusCode::SERVICE_UNAVAILABLE, "shutting down").into_response();
        }
    };

    let (root, doc, format, quality) = (ctx.root.clone(), ctx.doc.clone(), req.format, req.quality);
    let work = cancel.clone();
    let built = tokio::task::spawn_blocking(move || -> Result<Vec<Part>> {
        let _permit = permit;
        if let Some(job) = &job {
            job.set_phase(Phase::Rendering);
        }
        let out = viewshed::render(&root, &doc, &p, &work, job.as_deref())?;
        anyhow::ensure!(!work.is_cancelled(), "cancelled");
        if let Some(job) = &job {
            job.set_phase(Phase::Encoding);
        }

        let meta = serde_json::json!({
            "width": out.image.width(),
            "height": out.image.height(),
            // West, south, east, north -- the order Leaflet's LatLngBounds
            // wants once you pair them up.
            "bounds": out.bounds,
            "eye_elevation": out.eye_elevation,
            "radius": p.radius,
            "scale": p.scale,
            "target_height": p.target_height,
            "rays": out.rays,
            "samples": out.samples,
        });

        let mut png = std::io::Cursor::new(Vec::new());
        let (name, bytes) = match format {
            Format::Avif => (
                "viewshed.avif",
                avif::encode_rgba(&out.image, quality.clamp(1, 100), avif::SPEED)?,
            ),
            Format::Png => {
                out.image.write_to(&mut png, image::ImageFormat::Png)?;
                ("viewshed.png", png.into_inner())
            }
        };
        if let Some(job) = &job {
            job.set_phase(Phase::Done);
        }
        Ok(vec![
            ("meta".into(), None, serde_json::to_vec(&meta)?),
            ("image".into(), Some(name.into()), bytes),
        ])
    })
    .await
    ;

    let parts = match built {
        Ok(Ok(p)) => p,
        Ok(Err(e)) if e.to_string() == "cancelled" => {
            return (StatusCode::REQUEST_TIMEOUT, "cancelled").into_response();
        }
        Ok(Err(e)) => {
            // Logged here rather than in an `inspect` on `built`: that sees
            // only the JoinError, so a panic was reported and an ordinary
            // failure -- no elevation at the viewpoint, avifenc missing --
            // went out as a 500 with nothing in the log.
            eprintln!("viewshed failed: {e:#}");
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response();
        }
        Err(e) => {
            eprintln!("viewshed panicked: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response();
        }
    };
    multipart(parts, queued)
}

/// Pack the parts into one multipart/form-data body.
fn multipart(parts: Vec<Part>, queued: usize) -> Response {
    let boundary = "dempyramid7f3a9c2e";
    let mut body = Vec::new();
    for (name, filename, data) in parts {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        match &filename {
            // A part with a filename arrives as a Blob; without one, as a
            // string. That is what lets `meta` come back parseable directly.
            // Typed, so the Blob the client receives carries its own type and
            // can be used directly. Left as octet-stream this happened to work
            // only because <img> sniffs its content, and any client wrapping
            // the bytes in a Blob of the declared type got an unusable one.
            Some(f) => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\n\
                     Content-Type: {}\r\n\r\n",
                    content_type(f)
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
