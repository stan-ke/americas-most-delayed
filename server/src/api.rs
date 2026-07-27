//! The HTTP + WebSocket API in front of the polling scheduler.
//!
//! This is the **dynamic half** of the deployment. The pages themselves are the
//! static half and live on GitHub Pages (see `../../static/`), so nothing here
//! serves HTML: every route below is data, and the browser fetches it
//! cross-origin. That's the whole reason for the CORS layer.
//!
//! - `GET /api/status` — the full source-health report, with the `seq` a client
//!   needs before it can merge deltas. Fetched **once** per page load, not polled.
//! - `WS  /api/status/live` — source-health deltas, one every 2s.
//! - `WS  /api/subscribe` — the leaderboard: a full snapshot on connect, then a
//!   delta every 15s.
//! - `GET /api/shape/{slug}/{trip_id}` — one trip's route path, as an encoded
//!   polyline.
//! - `POST /api/debug/capture` — debug mode only; archives one entry's data.
//!
//! **Bytes are the product here.** Egress is what a VPS bills for, so the live
//! streams push deltas rather than snapshots (see [`crate::wire`]) and the two
//! things still served whole — the initial status report and a route shape — go out
//! over HTTP specifically so [`CompressionLayer`] can br/gzip them, which a
//! websocket frame doesn't get.

use std::sync::Arc;

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{
        Path, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::{Receiver, error::RecvError};
use tower_http::{
    compression::CompressionLayer,
    cors::{Any, CorsLayer},
};

use crate::metrics;
use crate::scheduler::Scheduler;
use crate::wire::encode_polyline;

/// Default port the API listens on, when `PORT` isn't set in the environment.
const DEFAULT_PORT: u16 = 8080;

/// Port the API listens on: the `PORT` env var if it parses as a `u16`, else
/// [`DEFAULT_PORT`]. A malformed value falls back to the default (logged) rather
/// than aborting startup.
fn port() -> u16 {
    match std::env::var("PORT") {
        Ok(raw) => raw.parse().unwrap_or_else(|_| {
            eprintln!("Ignoring malformed PORT={raw:?}, using {DEFAULT_PORT}");
            DEFAULT_PORT
        }),
        Err(_) => DEFAULT_PORT,
    }
}

/// How long a browser may reuse a route shape. A trip's path doesn't change within
/// a service day, and the static schedule behind it is only refreshed every 24h —
/// so this is a day of shape fetches that never leave the browser.
const SHAPE_CACHE: &str = "public, max-age=86400";

/// Optional bearer token that protects `GET /metrics`. Empty/missing means metrics
/// remain unauthenticated.
fn metrics_bearer_auth() -> Option<String> {
    std::env::var("METRICS_BEARER_AUTH")
        .ok()
        .filter(|value| !value.is_empty())
}

/// Constant-time string equality for auth-token checks.
fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.bytes().zip(right.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

type Shared = Arc<Scheduler>;

/// Serve the API forever. Blocks until the listener fails.
pub async fn serve(scheduler: Shared) -> Result<()> {
    // The pages are served from another origin entirely (GitHub Pages), so every
    // request here is cross-origin. Nothing we serve is private or authenticated,
    // so any origin may read it.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([header::CONTENT_TYPE]);

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/api/status", get(status))
        .route("/api/status/live", get(status_live))
        .route("/api/shape/{slug}/{trip_id}", get(shape))
        .route("/api/debug/capture", post(debug_capture))
        .route("/api/subscribe", get(subscribe))
        .layer(CompressionLayer::new())
        .layer(cors)
        .with_state(scheduler);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port())).await?;
    println!("Serving the API on http://{}", listener.local_addr()?);

    axum::serve(listener, app).await?;
    Ok(())
}

/// `GET /api/healthz`: is the poll loop turning?
///
/// **200** with the health JSON when a feed has been polled successfully recently,
/// **503** with the same JSON (carrying `reason`) when not — so a load balancer or
/// uptime check can gate on the status code while a human still gets the detail.
/// See [`Scheduler::health`] for what "healthy" means and why it's cheap enough to
/// poll often.
async fn healthz(State(scheduler): State<Shared>) -> Response {
    scheduler.metrics().record_http("healthz");
    let health = scheduler.health();
    let code = if health.ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(health)).into_response()
}

/// `GET /metrics`: the Prometheus/OpenMetrics scrape (see [`crate::metrics`]).
///
/// Cumulative counters live in the registry and are read as-is; the gauge-style
/// metrics are snapshotted from the scheduler's live state right here, at scrape
/// time, so they're always fresh without any per-tick bookkeeping.
async fn metrics(State(scheduler): State<Shared>, headers: HeaderMap) -> Response {
    if let Some(expected) = metrics_bearer_auth() {
        let authorized = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|token| constant_time_eq(token, &expected));
        if !authorized {
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"))],
                "Unauthorized",
            )
                .into_response();
        }
    }

    scheduler.metrics().record_http("metrics");
    let body = scheduler.metrics().render(&scheduler.gauge_values());
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static(metrics::CONTENT_TYPE),
        )],
        body,
    )
        .into_response()
}

/// `GET /api/status`: the whole source-health report, `seq` included.
///
/// The status page fetches this once, then follows `/api/status/live` for changes.
/// It's ~176 KB of JSON, which is exactly why it's here on HTTP (where the
/// compression layer gets it down to ~17 KB) and not on the socket.
///
/// Already serialized by the delta stream — handing back the string keeps this a
/// zero-work read of shared state.
async fn status(State(scheduler): State<Shared>) -> Response {
    scheduler.metrics().record_http("status");
    json_response(scheduler.status_full())
}

/// `GET /api/shape/{slug}/{trip_id}`: the trip's route path, as an encoded
/// polyline (see [`encode_polyline`] for why it isn't a JSON array of pairs).
///
/// Read on demand from the source's already-loaded static schedule and not retained
/// (see [`Scheduler::trip_shape`]); an empty string means no shape is available yet
/// (static not loaded, or the feed ships no `shapes.txt`).
async fn shape(
    State(scheduler): State<Shared>,
    Path((slug, trip_id)): Path<(String, String)>,
) -> Response {
    scheduler.metrics().record_http("shape");
    let points = scheduler
        .trip_shape(&slug, &trip_id)
        .await
        .unwrap_or_default();
    scheduler.metrics().record_shape(!points.is_empty());

    // Never cache an empty answer: "static isn't loaded yet" is a passing state, and
    // caching it for a day would leave the map blank long after the shape exists.
    let caching = if points.is_empty() {
        "no-store"
    } else {
        SHAPE_CACHE
    };

    (
        [(header::CACHE_CONTROL, HeaderValue::from_static(caching))],
        Json(ShapeResponse {
            polyline: encode_polyline(&points),
        }),
    )
        .into_response()
}

/// One trip's route path. `polyline` is precision-5 encoded; empty when unknown.
#[derive(Serialize)]
struct ShapeResponse {
    polyline: String,
}

/// A debug-capture request from the leaderboard page's per-row 🐛 button (only
/// shown when `AMD_DEBUG` is set): which entry, plus the operator's note.
#[derive(Deserialize)]
struct CaptureRequest {
    slug: String,
    trip_id: String,
    #[serde(default)]
    message: String,
}

/// The result of a capture: the archive path on success, or the error message.
#[derive(Serialize)]
struct CaptureResponse {
    ok: bool,
    path: Option<String>,
    error: Option<String>,
}

/// `POST /api/debug/capture`: zip up everything behind one leaderboard entry (see
/// [`Scheduler::capture_debug`]). Errors (debug disabled, unknown slug, write
/// failure) come back in the JSON body rather than as an HTTP error, so the
/// frontend can show them inline.
async fn debug_capture(
    State(scheduler): State<Shared>,
    Json(request): Json<CaptureRequest>,
) -> Json<CaptureResponse> {
    scheduler.metrics().record_http("debug_capture");
    match scheduler
        .capture_debug(&request.slug, &request.trip_id, &request.message)
        .await
    {
        Ok(path) => {
            scheduler.metrics().record_debug_capture(true);
            Json(CaptureResponse {
                ok: true,
                path: Some(path),
                error: None,
            })
        }
        Err(error) => {
            scheduler.metrics().record_debug_capture(false);
            Json(CaptureResponse {
                ok: false,
                path: None,
                error: Some(format!("{error:#}")),
            })
        }
    }
}

/// `GET /api/subscribe`: the leaderboard stream. Full snapshot on connect, then a
/// delta every 15s.
async fn subscribe(
    State(scheduler): State<Shared>,
    upgrade: WebSocketUpgrade,
) -> impl IntoResponse {
    scheduler.metrics().record_http("subscribe");
    scheduler.metrics().record_ws_open("leaderboard");
    upgrade.on_upgrade(move |socket| {
        // Subscribe *before* building the full, so no delta that lands in between is
        // lost. A delta the client already has is harmless — its `seq` is not ahead
        // of the client's, so the page ignores it (see [`crate::wire`]).
        let updates = scheduler.subscribe();
        let full = scheduler.board_full();
        stream(socket, updates, full)
    })
}

/// `GET /api/status/live`: source-health deltas. Nothing is sent on connect — the
/// page has already fetched its full from `GET /api/status`, compressed.
async fn status_live(
    State(scheduler): State<Shared>,
    upgrade: WebSocketUpgrade,
) -> impl IntoResponse {
    scheduler.metrics().record_http("status_live");
    scheduler.metrics().record_ws_open("status");
    upgrade.on_upgrade(move |socket| {
        let updates = scheduler.subscribe_status();
        stream(socket, updates, String::new())
    })
}

/// Pump one delta stream down one socket until the client goes away.
///
/// `full` (when non-empty) is sent first: the state every later delta is merged
/// into. Deltas are already-serialized JSON shared across every client, so a push
/// is a refcount bump, not a re-encode.
async fn stream(mut socket: WebSocket, mut updates: Receiver<Arc<str>>, full: String) {
    if !full.is_empty() && socket.send(Message::text(full)).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            update = updates.recv() => match update {
                Ok(delta) => {
                    if socket.send(Message::text(&*delta)).await.is_err() {
                        break;
                    }
                }
                // Lagged past the buffer under a burst. We can't skip a delta — the
                // client's merge would silently keep stale fields — so we drop the
                // connection and let it reconnect into a fresh full. The page
                // reconnects on close anyway, and would have to on the `base` gap
                // regardless.
                Err(RecvError::Lagged(_)) => break,
                Err(RecvError::Closed) => break,
            },
            // Drain client frames so we notice a close (and let axum answer pings).
            // We don't act on client messages otherwise.
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
        }
    }
}

/// Hand back an already-serialized JSON body without re-encoding it.
fn json_response(body: String) -> Response {
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        body,
    )
        .into_response()
}
