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
    http::{HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::{Receiver, error::RecvError};
use tower_http::{
    compression::CompressionLayer,
    cors::{Any, CorsLayer},
};

use crate::scheduler::Scheduler;
use crate::wire::encode_polyline;

/// Port the API listens on.
const PORT: u16 = 8080;

/// How long a browser may reuse a route shape. A trip's path doesn't change within
/// a service day, and the static schedule behind it is only refreshed every 24h —
/// so this is a day of shape fetches that never leave the browser.
const SHAPE_CACHE: &str = "public, max-age=86400";

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
        .route("/api/status", get(status))
        .route("/api/status/live", get(status_live))
        .route("/api/shape/{slug}/{trip_id}", get(shape))
        .route("/api/debug/capture", post(debug_capture))
        .route("/api/subscribe", get(subscribe))
        .layer(CompressionLayer::new())
        .layer(cors)
        .with_state(scheduler);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", PORT)).await?;
    println!("Serving the API on http://{}", listener.local_addr()?);
    println!("  GET /api/status      — full source-health report (fetch once)");
    println!("  WS  /api/status/live — source-health deltas");
    println!("  WS  /api/subscribe   — live leaderboard, pushed every 15s");
    println!("  GET /api/shape/…     — a trip's route path (encoded polyline)");
    println!("  POST /api/debug/capture — archive one entry's data (AMD_DEBUG)");
    println!("The pages live in ../static (GitHub Pages) and read this API.");

    axum::serve(listener, app).await?;
    Ok(())
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
    let points = scheduler
        .trip_shape(&slug, &trip_id)
        .await
        .unwrap_or_default();

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
    match scheduler
        .capture_debug(&request.slug, &request.trip_id, &request.message)
        .await
    {
        Ok(path) => Json(CaptureResponse {
            ok: true,
            path: Some(path),
            error: None,
        }),
        Err(error) => Json(CaptureResponse {
            ok: false,
            path: None,
            error: Some(format!("{error:#}")),
        }),
    }
}

/// `GET /api/subscribe`: the leaderboard stream. Full snapshot on connect, then a
/// delta every 15s.
async fn subscribe(
    State(scheduler): State<Shared>,
    upgrade: WebSocketUpgrade,
) -> impl IntoResponse {
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
