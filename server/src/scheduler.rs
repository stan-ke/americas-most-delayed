//! Dynamic polling of many GTFS-realtime feeds.
//!
//! We track hundreds of agencies at once, but only a handful ever have a bus on
//! the leaderboard at any given moment. Polling every feed on a fixed short
//! interval would waste an enormous amount of networking on feeds that never
//! surface. Instead each feed carries its own poll interval:
//!
//! - A feed is polled every [`BASE_INTERVAL`] while one of its vehicles sits in
//!   the global top [`LEADERBOARD_SIZE`].
//! - As soon as it drops out, its interval backs off geometrically (doubling
//!   each miss) up to [`MAX_INTERVAL`], so quiet feeds are checked rarely.
//! - The moment it lands back in the top, it snaps to [`BASE_INTERVAL`].
//!
//! Each feed runs as its own async task, looping "poll, then sleep its current
//! interval". Networking is the bottleneck, so a shared [`Semaphore`] of
//! [`MAX_CONCURRENT_POLLS`] permits bounds how many feeds are ever in flight at
//! once; the CPU-bound decode/delay/parse work is handed to the blocking pool so
//! it never stalls the runtime.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use gtfs_rt::{FeedHeader, FeedMessage};
use prost::Message;
use reqwest::Client;
use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::sync::{Semaphore, broadcast};

use crate::agency::AgencyConfig;
use crate::auth::FeedAuth;
use crate::delay::{self, DelayedTrip, TripObservation, VehiclePositions};
use crate::gtfs::{self, Gtfs};
use crate::history::TripHistory;
use crate::metrics::{GaugeValues, Metrics};
use crate::realtime;
use crate::score::{self, ScoreBreakdown, ScoreInputs};
use crate::wire::DeltaStream;

/// Ring-buffer capacity for the live-update broadcast. A websocket client that
/// falls this far behind is resynced from a fresh snapshot rather than the
/// buffered deltas, so this only needs to absorb brief bursts.
const UPDATE_BUFFER: usize = 64;

/// HTTP statuses that mark a source as broken: unauthorized (`401`) or gone
/// (`404`). We take a source out of the normal rotation the first time it returns
/// one — but not forever, see [`FAILED_RETRY_INTERVAL`].
const FATAL_STATUSES: [u16; 2] = [401, 404];
/// How often a [`SourceState::Failed`] source is retried. A 401/404 is usually
/// permanent, but not always — a key gets provisioned, a feed moves back, an
/// agency's gateway briefly answers 404 for everything — and a source retired for
/// good is one we'd never notice coming back. Slow enough (20 min) that a few
/// hundred dead feeds cost a negligible trickle of requests, and a source that
/// answers is folded straight back into the rotation by
/// [`record_success`](Scheduler::record_success).
const FAILED_RETRY_INTERVAL: Duration = Duration::from_secs(20 * 60);

/// Poll interval for a feed with a vehicle currently on the leaderboard.
const BASE_INTERVAL: Duration = Duration::from_secs(20);
/// The slowest we ever poll a feed that keeps missing the leaderboard.
const MAX_INTERVAL: Duration = Duration::from_secs(300);
/// How many vehicles make the wall of shame — and the cutoff for staying "hot".
const LEADERBOARD_SIZE: usize = 25;
/// How many `NoRealtime` (static-only) agencies to surface in `/status`: the
/// largest N by scheduled-trip count, so only substantial agencies we're missing
/// realtime for show up, not every tiny static-only feed.
const NO_REALTIME_DISPLAY: usize = 100;
/// Ceiling on concurrent in-flight feed fetches. Higher = more parallel network.
const MAX_CONCURRENT_POLLS: usize = 48;
/// Per-request network timeout, so one hung feed can't pin a permit forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Sent on every feed/static request. Many agency servers reject requests with no
/// `User-Agent` (403), so identifying ourselves is what makes their feeds fetchable.
const USER_AGENT: &str = "AmericasMostDelayed/1.0 (transit delay monitor)";
/// How often the leaderboard is (re)printed and pushed to websocket clients.
const PRINT_INTERVAL: Duration = Duration::from_secs(15);
/// How often the source-status stream ticks — fast enough that a poll landing on a
/// feed lights its LED up while you're looking at it. Affordable only because the
/// tick sends a delta rather than the whole report (see [`crate::wire`]).
const STATUS_INTERVAL: Duration = Duration::from_secs(2);
/// How stale the freshest successful poll may get before `/api/healthz` reports
/// unhealthy. With hundreds of feeds staggered across the poll rotation, at least
/// one succeeds every couple of seconds even when every feed has backed off to
/// [`MAX_INTERVAL`] — so this only trips if the whole loop wedges, not on normal
/// backoff.
const HEALTH_STALE_AFTER: u64 = 120;
/// Directory for cached static GTFS zips.
const CACHE_DIR: &str = "./feeds";
/// Directory debug captures are written to (git-ignored, like `./feeds`). Only
/// ever created/written when [`Scheduler::debug`] is on and a capture is triggered.
const DEBUG_DIR: &str = "./debug";
/// Concurrent static-feed fetches for the background census/refresh — kept small
/// and separate from the poll limiter so it never starves live polling.
const STATIC_FETCH_CONCURRENCY: usize = 12;
/// How often the maintenance task scans for stale static caches (well under
/// [`gtfs::STATIC_TTL`], so a feed is refreshed within a pass of crossing it).
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// How far a delayed trip's live vehicle may sit from its route shape before we
/// treat the trip as bad data (a mismatched trip/vehicle) and drop it from the
/// leaderboard. Generous, to catch gross mismatches — a vehicle in the wrong city —
/// not the normal GPS / shape-simplification wobble of a few hundred metres.
const OFF_ROUTE_KM: f64 = 2.0;
/// How long a ranked trip may go unmentioned by its feed before we call it
/// **finished** and start its decay clock (see [`crate::score`]). Comfortably longer
/// than [`MAX_INTERVAL`], so an ordinary gap between two polls of a backed-off feed
/// never retires a live trip — and in practice a trip near the board keeps its feed
/// hot at [`BASE_INTERVAL`], so the end of a run is noticed within seconds of it
/// actually happening. The decay clock is set to the trip's *last sighting*, not to
/// when we noticed, so a slow feed doesn't win its trips extra time on the wall.
const TRIP_END_GRACE: u64 = 10 * 60;
/// Ceiling on how many finished trips we keep around to decay. The 24h horizon alone
/// would bound this, but loosely: this caps the archive at the only ones that could
/// still plausibly rank, so memory doesn't scale with a day of every agency's late
/// trips. Kept well above [`LEADERBOARD_SIZE`] so the board never runs dry as
/// entries decay out.
const ARCHIVE_CAP: usize = 400;
/// How close a delayed trip's live vehicle must sit to either end of its route shape
/// (its start or final terminal) before we drop it from the leaderboard — it's a
/// vehicle parked at a terminal, not one late en route, and its reported delay is
/// spurious (a run that hasn't departed, or a finished run going stale). Kept small
/// so a bus genuinely crawling near either endpoint isn't suppressed; the trip stays
/// *watched* (its history keeps accruing), it's only held off the ranked board.
const TERMINAL_KM: f64 = 0.4;

/// One entry on the public leaderboard — a single late trip, ranked globally.
#[derive(Debug, Clone, Serialize)]
pub struct LeaderboardEntry {
    pub rank: usize,
    /// Agency display name.
    pub agency: String,
    /// Agency slug, stable across restarts (joins to `/status`).
    pub slug: String,
    /// Realtime trip id — the key the map uses to fetch this trip's route shape
    /// (`GET /api/shape/{slug}/{trip_id}`).
    pub trip_id: String,
    pub route: String,
    /// What kind of vehicle runs this route — `bus`, `tram`, `subway`, `rail`,
    /// `ferry`, `cable-tram`, `aerial-lift`, `funicular`, `trolleybus`, `monorail`
    /// — from the static schedule's `route_type` (see [`gtfs::RouteKind`]). The map
    /// picks its vehicle icon from this. `null` when the agency's schedule isn't
    /// loaded or doesn't classify the route; the page then shows a generic vehicle.
    /// Fixed for the life of a trip, so it costs one field once per row on the wire.
    pub vehicle_type: Option<&'static str>,
    pub headsign: Option<String>,
    pub next_stop: Option<String>,
    pub vehicle: Option<String>,
    pub delay_seconds: i64,
    /// The worst this trip ever got while we could vouch for it — what it's ranked
    /// on, and what a finished trip is remembered for rather than whatever its last
    /// frame happened to say. Equal to `delay_seconds` for almost every live trip.
    pub peak_delay_seconds: i64,
    /// When the trip stopped being reported, as a unix timestamp — `null` while it's
    /// still running. A finished trip keeps its place and fades out over the next 24
    /// hours (see [`crate::score`]); the page turns this into "finished 12m ago".
    ///
    /// A timestamp rather than an age, for the same reason as
    /// [`SourceStatus::last_poll`]: an age would change every tick for every ended
    /// row and drag them all onto the wire (see [`crate::wire`]). This changes once,
    /// ever, per trip.
    pub ended_at: Option<u64>,
    /// How the delay was derived: `trip-level`, `stop-level`, or `vs-schedule`.
    pub source: &'static str,
    /// How long we've been watching this trip, and how late it was when we first
    /// saw it — the evidence that this delay accumulated under observation rather
    /// than arriving fully-formed (see [`crate::history`]). Every ranked trip has
    /// passed that check, so these are the receipts, not a caveat.
    pub tracked_seconds: u64,
    pub birth_delay_seconds: i64,
    /// Live vehicle location, when the feed's vehicle-positions feed places this
    /// trip. Only fetched for hot (top-[`LEADERBOARD_SIZE`]) feeds; the map on the
    /// leaderboard page uses it for the most-delayed vehicle.
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    /// The arithmetic behind this row's rank, **debug mode only** — the payload
    /// behind the leaderboard's 🧮 button. Omitted entirely (not null) when
    /// `AMD_DEBUG` is off, so it costs nothing in production: it carries the decay
    /// factor, which changes every tick and would otherwise drag every finished row
    /// onto the wire (see [`crate::wire`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score_breakdown: Option<ScoreBreakdown>,
}

/// The whole leaderboard at one instant — the websocket payload, pushed on
/// connect and again on every update.
#[derive(Debug, Clone, Serialize)]
pub struct LeaderboardSnapshot {
    /// Unix seconds when this snapshot was built.
    pub generated_at: u64,
    pub entries: Vec<LeaderboardEntry>,
    /// Whether debug capture is enabled (env `AMD_DEBUG`). The frontend shows a
    /// per-row "capture" button only when this is true.
    pub debug_enabled: bool,
}

/// Whether a source is being polled, blocked behind auth, or has been disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceState {
    /// In the poll rotation.
    Active,
    /// Skipped: the realtime feed needs authentication we don't have.
    RequiresAuth,
    /// Skipped: the catalog has a static schedule for this agency but no paired
    /// GTFS-realtime trip-updates feed, so there's nothing live to poll. Still
    /// censused for its `total_trips` scale, so a large agency the catalog is
    /// missing realtime for stands out in `/status`.
    NoRealtime,
    /// Skipped: the feed has trip updates but no vehicle-positions feed, so we can't
    /// verify a delayed trip's vehicle is actually on its route. Surfaced in
    /// `/status` but never polled.
    NoVehiclePositions,
    /// Out of the normal rotation after the feed returned a [`FATAL_STATUSES`]
    /// code. Retried every [`FAILED_RETRY_INTERVAL`]; a poll that succeeds puts
    /// the source back to `Active`.
    Failed(u16),
}

/// Mutable per-source health, updated on every poll. Serialized into a
/// [`SourceStatus`] for `/status` (which also folds in derived figures like
/// total scheduled trips).
struct SourceRuntime {
    state: SourceState,
    /// Current poll interval — the "fetch frequency".
    interval: Duration,
    /// When this source was last polled, as a unix timestamp — see
    /// [`SourceStatus::last_poll`] for why it isn't an age.
    last_poll: Option<u64>,
    /// Whether the most recent poll succeeded.
    last_success: Option<bool>,
    /// Human-readable reason the most recent poll failed, if it did.
    last_error: Option<String>,
    /// Vehicles (trip updates) the feed published on its last successful poll.
    vehicles_now: usize,
    /// How many of those trips came out *late* (the feed's board size) on the last
    /// poll. A big agency stuck at 0 here signals its delays aren't being read.
    late_trips: usize,
    /// How many late trips the last poll produced that the delay history refused
    /// to vouch for (see [`crate::history`]). Expected to be nonzero on a feed with
    /// sloppy trip assignment — and to be *large* right after startup, when nothing
    /// has been watched long enough yet.
    vetted_out: usize,
    /// High-water mark of `vehicles_now`, a rough always-available scale signal.
    peak_vehicles: usize,
    /// Whether a vehicle of this source currently sits on the leaderboard.
    hot: bool,
    /// Whether this source is right now downloading and importing its static
    /// GTFS (a census count or a full load). Transient and orthogonal to
    /// `state` — an `Active` or `NoRealtime` source flips this on only while the
    /// zip fetch + SQLite build is in flight.
    loading: bool,
    /// Distinct trips in the agency's static schedule — its scale. Populated for
    /// every agency by the background census (and updated on a full static load),
    /// so it doesn't depend on a feed ever going hot. `None` until first counted.
    total_trips: Option<usize>,
}

impl SourceRuntime {
    fn new(state: SourceState) -> Self {
        SourceRuntime {
            state,
            interval: BASE_INTERVAL,
            last_poll: None,
            last_success: None,
            last_error: None,
            vehicles_now: 0,
            late_trips: 0,
            vetted_out: 0,
            peak_vehicles: 0,
            hot: false,
            loading: false,
            total_trips: None,
        }
    }
}

/// One source's line in the `/status` report.
#[derive(Debug, Clone, Serialize)]
pub struct SourceStatus {
    pub slug: String,
    pub display_name: String,
    pub country: Option<String>,
    /// `active`, `requires_auth`, `no_realtime`, `no_vehicle_positions`, or `failed`.
    pub state: &'static str,
    /// The HTTP status that disabled the source, when `state == "failed"`.
    pub failed_status: Option<u16>,
    /// Whether a vehicle is currently on the leaderboard.
    pub hot: bool,
    /// Whether the source is right now downloading/importing its static GTFS.
    pub loading: bool,
    /// Current poll interval in seconds; `None` for sources we don't poll.
    pub poll_interval_seconds: Option<u64>,
    /// When this source was last polled, as a unix timestamp — *not* an age.
    ///
    /// An age would change on every tick for every source, so no row would ever be
    /// unchanged and the delta stream (see [`crate::wire`]) would degenerate into
    /// re-sending the whole report. A timestamp only changes when the source is
    /// actually polled, which is what makes a tick cost a few hundred bytes instead
    /// of 176 KB. The page subtracts it from the message's `generated_at`.
    pub last_poll: Option<u64>,
    pub last_success: Option<bool>,
    pub last_error: Option<String>,
    /// Vehicles the feed is publishing right now.
    pub vehicles_now: usize,
    /// How many of those trips are currently late (this feed's leaderboard size).
    pub late_trips: usize,
    /// Late trips the last poll produced but the delay history wouldn't vouch for.
    pub vetted_out: usize,
    pub peak_vehicles: usize,
    /// Total trips in the agency's static schedule — its scale. The background
    /// census populates this for every agency; `None` only before the first
    /// census pass reaches it, or if its static feed can't be fetched.
    pub total_trips: Option<usize>,
}

/// Aggregate counts across all sources, for a quick health glance.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StatusSummary {
    pub total_sources: usize,
    pub active: usize,
    pub requires_auth: usize,
    /// Sources with a static schedule but no realtime feed to poll.
    pub no_realtime: usize,
    /// Sources with trip updates but no vehicle-positions feed to verify against.
    pub no_vehicle_positions: usize,
    pub failed: usize,
    pub hot: usize,
    /// Sources currently downloading/importing their static GTFS.
    pub loading: usize,
    /// Sum of `vehicles_now` across every source.
    pub vehicles_now: usize,
    /// Feeds whose static schedule is currently loaded (an open SQLite connection).
    pub static_loaded: usize,
    /// Heap SQLite is holding across all those connections, and its high-water mark.
    /// Compare against `process_rss_bytes`: that ratio is what says whether SQLite is
    /// worth tuning at all, or whether the memory is somewhere else entirely.
    pub sqlite_bytes: i64,
    pub sqlite_peak_bytes: i64,
    /// Resident set size of the whole process (Linux only).
    pub process_rss_bytes: Option<u64>,
}

/// The `/api/healthz` response — see [`Scheduler::health`].
#[derive(Debug, Clone, Serialize)]
pub struct Health {
    /// Whether the poll loop is turning. Drives the HTTP status: 200 vs 503.
    pub ok: bool,
    /// Feeds currently in the poll rotation.
    pub active_sources: usize,
    /// Seconds since the freshest successful poll across all feeds; `None` if none
    /// has succeeded yet.
    pub last_success_age_seconds: Option<u64>,
    /// Why we're unhealthy, when we are.
    pub reason: Option<&'static str>,
}

/// The full `/status` response.
#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub generated_at: u64,
    pub summary: StatusSummary,
    pub sources: Vec<SourceStatus>,
}

/// One trip's standing on the wall of shame, from the first poll that ranked it
/// until it decays off — **including after it has finished running**.
///
/// This is the restructure the scored leaderboard needed. The old board could only
/// ever show what a feed was reporting *right now*, so the worst trip of the day
/// disappeared the moment it finally pulled in. A record outlives its trip: once the
/// feed stops mentioning it, [`Scheduler::sweep_archive`] stamps `ended_at` and the
/// score decays from there (see [`crate::score`]).
///
/// So every field a finished entry still needs is **frozen into the record** rather
/// than looked up at render time — the position, the provenance receipts, the static
/// span. By the time a record is being displayed hours after it ended, the live
/// positions map has moved on and [`TripHistory`] has long forgotten the trip.
struct TripRecord {
    /// The trip as of its last sighting: route, headsign, next stop, vehicle, delay.
    trip: DelayedTrip,
    /// The worst vetted delay this trip ever reached — what it's scored on. The
    /// growth bound in [`crate::history`] means this tracks the current delay closely
    /// while live; it matters for a finished trip, whose final frame may be a
    /// revised-down estimate that isn't what the run should be remembered for.
    peak_delay_seconds: i64,
    /// First and most recent poll that ranked this trip.
    first_seen: u64,
    last_seen: u64,
    /// When the trip stopped running — `None` while it's still being reported. Set
    /// to the *last sighting*, so the decay clock reflects when the trip actually
    /// ended rather than when we got around to noticing.
    ended_at: Option<u64>,
    /// What the timetable plans for this trip, looked up once and cached: it's static
    /// data, and re-querying it every poll for every ranked trip would be pure waste.
    span: Option<gtfs::TripSpan>,
    /// Whether we've *tried* the span lookup with a loaded schedule. Distinguishes
    /// "this agency's static isn't loaded yet, try again next poll" from "the
    /// schedule doesn't know this trip", so we neither retry forever nor give up
    /// before the static arrives.
    span_checked: bool,
    /// The provenance receipts as of the last sighting, frozen so a finished trip
    /// keeps them after [`TripHistory`] has forgotten it.
    tracked_seconds: u64,
    birth_delay_seconds: i64,
    /// Where the vehicle was when we last placed it — for a finished trip, where it
    /// ended up.
    latitude: Option<f64>,
    longitude: Option<f64>,
}

impl TripRecord {
    /// Everything [`crate::score`] needs, gathered from the record. `now` is only
    /// used to age a finished trip; a live one scores the same whenever it's asked.
    fn score_inputs(&self, now: u64) -> ScoreInputs {
        ScoreInputs {
            delay_seconds: self.peak_delay_seconds,
            scheduled_duration_seconds: self.span.and_then(|s| s.duration_seconds),
            stops_remaining: self.span.and_then(|s| s.stops_remaining),
            tracked_seconds: self.tracked_seconds,
            birth_delay_seconds: self.birth_delay_seconds,
            has_live_location: self.latitude.is_some() && self.longitude.is_some(),
            seconds_since_end: self.ended_at.map(|ended| now.saturating_sub(ended) as i64),
        }
    }

    fn score(&self, now: u64) -> f64 {
        self.score_inputs(now).score()
    }
}

/// A successful poll: the delayed trips plus how many vehicles the feed carried,
/// an observation of every trip for the delay history, and whether it's a
/// times-only feed that needs its static schedule loaded to surface any delay at
/// all.
struct PollSuccess {
    trips: Vec<DelayedTrip>,
    observations: Vec<TripObservation>,
    vehicle_count: usize,
    needs_schedule: bool,
}

/// Shared state driving the whole polling system.
pub struct Scheduler {
    configs: Vec<AgencyConfig>,
    /// Shared async HTTP client (cheap to clone; connection-pooled internally).
    client: Client,
    /// Per-host feed credentials, injected into requests to gated feeds.
    auth: Arc<FeedAuth>,
    /// Caps how many feed fetches are in flight at once.
    limiter: Semaphore,
    /// Latest delayed trips per agency index — the **live** working set, replaced
    /// wholesale on every poll and pruned by the off-route check. It's what a poll
    /// produces; the leaderboard is no longer derived from it directly.
    boards: Mutex<HashMap<usize, Vec<DelayedTrip>>>,
    /// Every trip that has ranked recently, live or finished, per agency index —
    /// **this is what the leaderboard is built from**. Each poll folds its board in
    /// here ([`record_board`](Self::record_board)); trips the feed stops mentioning
    /// are stamped finished and decay out over 24h
    /// ([`sweep_archive`](Self::sweep_archive)). See [`TripRecord`].
    archive: Mutex<HashMap<usize, HashMap<String, TripRecord>>>,
    /// Latest live vehicle coordinates per agency index, keyed by `trip_id`.
    /// Populated only for hot feeds (whose vehicle-positions feed we fetch), and
    /// joined onto leaderboard entries so the map can show the delayed vehicle.
    positions: Mutex<HashMap<usize, VehiclePositions>>,
    /// Lazily loaded static GTFS per agency. A present key means we've tried:
    /// `Some` is loaded, `None` means the load was attempted and failed.
    static_gtfs: Mutex<HashMap<usize, Option<Arc<Gtfs>>>>,
    /// Per-source health, indexed by agency index (parallel to `configs`).
    status: Mutex<Vec<SourceRuntime>>,
    /// What every live trip's delay has done over time. A trip only reaches the
    /// leaderboard if this vouches for it — see [`crate::history`] for why a single
    /// snapshot can't tell a real delay from a stale `trip_id`, and time can.
    history: TripHistory,
    /// The leaderboard as connected clients currently hold it, and the fan-out of
    /// the per-tick deltas that keep it there (see [`crate::wire`]). A client is
    /// served [`board`](Self::board)'s `full()` on connect and merges every delta
    /// after it.
    board: Mutex<DeltaStream>,
    updates: broadcast::Sender<Arc<str>>,
    /// The same, for the source-status page — the far more expensive of the two, so
    /// the one the delta protocol exists for.
    source_status: Mutex<DeltaStream>,
    status_updates: broadcast::Sender<Arc<str>>,
    /// Prometheus metrics for the whole pipeline (served at `/metrics`). Counters
    /// are bumped from the hot paths below; gauges are snapshotted at scrape time
    /// from [`gauge_values`](Self::gauge_values). See [`crate::metrics`].
    metrics: Arc<Metrics>,
    /// Whether debug capture (`AMD_DEBUG`) is on. Gates [`capture_debug`](Self::capture_debug)
    /// and is surfaced to the frontend via each snapshot's `debug_enabled`.
    debug: bool,
}

impl Scheduler {
    fn new(configs: Vec<AgencyConfig>, client: Client, auth: Arc<FeedAuth>, debug: bool) -> Self {
        // Seed each source's state from its config: auth-gated feeds start (and
        // stay) `RequiresAuth`; everything else joins the poll rotation.
        let status = configs
            .iter()
            .map(|config| {
                let state = if !config.has_trip_updates() {
                    SourceState::NoRealtime
                } else if config.requires_auth() {
                    SourceState::RequiresAuth
                } else if !config.has_vehicle_positions() {
                    SourceState::NoVehiclePositions
                } else {
                    SourceState::Active
                };
                SourceRuntime::new(state)
            })
            .collect();

        let (updates, _) = broadcast::channel(UPDATE_BUFFER);
        let (status_updates, _) = broadcast::channel(UPDATE_BUFFER);

        Scheduler {
            configs,
            client,
            auth,
            limiter: Semaphore::new(MAX_CONCURRENT_POLLS),
            boards: Mutex::new(HashMap::new()),
            archive: Mutex::new(HashMap::new()),
            positions: Mutex::new(HashMap::new()),
            static_gtfs: Mutex::new(HashMap::new()),
            status: Mutex::new(status),
            history: TripHistory::new(),
            // The leaderboard's array *is* its ranking, so every entry rides in
            // every delta (an unchanged one shrinks to its identity). The status
            // list is just a set, so unchanged sources don't ride at all.
            board: Mutex::new(DeltaStream::new("entries", &["slug", "trip_id"], true)),
            updates,
            source_status: Mutex::new(DeltaStream::new("sources", &["slug"], false)),
            status_updates,
            metrics: Arc::new(Metrics::new(debug, unix_now())),
            debug,
        }
    }

    /// The indices of every feed we actually poll (auth-gated feeds excluded).
    fn pollable(&self) -> Vec<usize> {
        let status = self.status.lock().unwrap();
        (0..self.configs.len())
            .filter(|&idx| status[idx].state == SourceState::Active)
            .collect()
    }

    /// One feed's polling loop: poll, then sleep its current interval, forever.
    /// A fatal status doesn't end the task — it drops the feed to the slow
    /// [`FAILED_RETRY_INTERVAL`] until it answers again. The initial `stagger`
    /// spreads the first polls across [`BASE_INTERVAL`] so we don't fire hundreds
    /// of requests at once.
    async fn run_feed(self: Arc<Self>, idx: usize, stagger: Duration) {
        tokio::time::sleep(stagger).await;
        let mut interval = BASE_INTERVAL;
        loop {
            interval = self.poll_once(idx, interval).await;
            tokio::time::sleep(interval).await;
        }
    }

    /// Poll one feed once: update its board and health, and return the interval
    /// until its next poll.
    async fn poll_once(&self, idx: usize, current_interval: Duration) -> Duration {
        let config = &self.configs[idx];

        // Enrich with the static schedule only if we've already loaded it (which
        // only happens once a feed reaches the leaderboard — see below).
        let gtfs = self.loaded_static(idx);

        match self.fetch_delayed_trips(config, gtfs).await {
            Ok(poll) => {
                let PollSuccess {
                    mut trips,
                    observations,
                    vehicle_count,
                    needs_schedule,
                } = poll;
                // Fold this poll into the delay history, and keep only the trips
                // whose lateness we watched accumulate. Without this a feed that
                // hands us a stale `trip_id` — a bus finished one run and sent out
                // on a later one, still wearing the old label — reads as hours late
                // and tops the board (see [`crate::history`]).
                let refused = self
                    .history
                    .vet(idx, &observations, &mut trips, unix_now() as i64);
                self.record_success(idx, vehicle_count, trips.len(), refused.len());
                self.boards.lock().unwrap().insert(idx, trips);
                // A trip the history has just falsified may already be archived from
                // an earlier poll, back when we still believed it. Evict it: left
                // alone it would simply stop being refreshed, get stamped "finished",
                // and spend a day decaying across the wall of shame on a delay we now
                // know was a stale label.
                let evicted = self.evict_archived(idx, &refused);
                self.metrics.record_evicted("falsified", evicted as u64);
                // A times-only feed (no delay fields, just predicted times) surfaced
                // nothing: load its static schedule so the next poll can derive
                // delays by comparison. Without this it could never appear — it
                // can't get hot without delays, and has no delays without a schedule.
                if needs_schedule {
                    self.ensure_static_loaded(idx).await;
                }
            }
            Err(err) => {
                eprintln!("[{}] poll failed: {err:#}", config.display_name);
                // A 401/404 means this feed is gone or gated: drop it out of the
                // rotation. Its board is cleared and everything it contributed is
                // forgotten, but its task stays alive to retry it slowly — the
                // status may yet turn out to have been temporary.
                if let Some(status) = http_status(&err).filter(|s| FATAL_STATUSES.contains(s)) {
                    if self.record_failure(idx, &err, Some(status)) {
                        eprintln!(
                            "[{}] disabling source after HTTP {status}; retrying every {}m",
                            config.display_name,
                            FAILED_RETRY_INTERVAL.as_secs() / 60
                        );
                    }
                    self.boards.lock().unwrap().remove(&idx);
                    self.positions.lock().unwrap().remove(&idx);
                    self.history.forget_source(idx);
                    self.status.lock().unwrap()[idx].interval = FAILED_RETRY_INTERVAL;
                    return FAILED_RETRY_INTERVAL;
                }
                self.record_failure(idx, &err, None);
            }
        }

        // Fold this poll's board into the archive *before* asking whether the feed is
        // hot: the archive is what the leaderboard ranks, so a trip that isn't in it
        // yet can't be seen to be winning. Hotness must be judged on score, not raw
        // delay — the two no longer agree, and a short trip that scores its way onto
        // the board would otherwise never earn the positions fetch that verifies it.
        self.record_board(idx).await;

        // Is this feed hot (a trip in the global top N)? That keeps it on the fast
        // interval, earns it a static-feed load for richer labels, and fetches its
        // live vehicle positions so the map can show the delayed vehicle. Fetching
        // positions also prunes off-route trips, so we record and recheck afterward:
        // a feed whose only delayed trips were bogus drops out and backs off.
        if self.on_leaderboard(idx) {
            self.ensure_static_loaded(idx).await;
            self.update_vehicle_positions(idx).await;
            // Again, now that the pruning has happened and the static schedule may
            // have arrived — this is what attaches positions and trip spans.
            self.record_board(idx).await;
        }
        let hot = self.on_leaderboard(idx);
        let next_interval = if hot {
            BASE_INTERVAL
        } else {
            (current_interval * 2).min(MAX_INTERVAL)
        };

        let mut status = self.status.lock().unwrap();
        status[idx].hot = hot;
        status[idx].interval = next_interval;
        next_interval
    }

    /// Fold a feed's current live board into the [`archive`](Self::archive) — the
    /// set the leaderboard actually ranks.
    ///
    /// Upsert, never replace: a trip already on record keeps its identity (and its
    /// peak, its birth, its first sighting) and simply gets refreshed. A trip that is
    /// still being reported is by definition not finished, so this also *clears* any
    /// `ended_at` — that's how a feed which skipped a beat, or a vehicle that pulled
    /// away from a terminal layover and is genuinely late again, gets its place back
    /// instead of decaying while still running.
    ///
    /// Idempotent, because [`poll_once`](Self::poll_once) calls it twice: once before
    /// deciding the feed is hot, and again after the vehicle-position fetch has
    /// pruned the board and possibly loaded the static schedule.
    async fn record_board(&self, idx: usize) {
        let live: Vec<DelayedTrip> = self
            .boards
            .lock()
            .unwrap()
            .get(&idx)
            .cloned()
            .unwrap_or_default();
        if live.is_empty() {
            return;
        }

        // The static span is looked up **once per trip, ever** — it's timetable data
        // that can't change under us, and re-querying it every poll for every ranked
        // trip would be pure waste. `span_checked` is what stops us both from
        // retrying forever and from giving up before the agency's static arrives.
        let pending: Vec<(String, Option<u32>)> = {
            let archive = self.archive.lock().unwrap();
            let known = archive.get(&idx);
            live.iter()
                .filter(|trip| {
                    known
                        .and_then(|records| records.get(&trip.trip_id))
                        .is_none_or(|record| !record.span_checked)
                })
                .map(|trip| (trip.trip_id.clone(), trip.stop_sequence))
                .collect()
        };
        let spans: HashMap<String, Option<gtfs::TripSpan>> = match self.loaded_static(idx) {
            Some(gtfs) if !pending.is_empty() => tokio::task::spawn_blocking(move || {
                pending
                    .into_iter()
                    .map(|(trip_id, sequence)| {
                        let span = gtfs.trip_span(&trip_id, sequence);
                        (trip_id, span)
                    })
                    .collect()
            })
            .await
            .unwrap_or_default(),
            // No schedule loaded yet: leave every span unchecked and try again on a
            // later poll, once this feed has gone hot enough to earn a static load.
            _ => HashMap::new(),
        };

        // Both of these are frozen into the record rather than joined at render time,
        // because a finished trip outlives both sources: the positions map moves on,
        // and the history forgets the trip after 20 minutes.
        let coords: Vec<Option<(f64, f64)>> = {
            let positions = self.positions.lock().unwrap();
            let feed = positions.get(&idx);
            live.iter()
                .map(|trip| feed.and_then(|f| f.get(&trip.trip_id)).copied())
                .collect()
        };
        let receipts: Vec<_> = live
            .iter()
            .map(|trip| self.history.provenance(idx, &trip.trip_id))
            .collect();

        let now = unix_now();
        let mut archive = self.archive.lock().unwrap();
        let records = archive.entry(idx).or_default();
        for ((trip, position), receipt) in live.into_iter().zip(coords).zip(receipts) {
            let span = spans.get(&trip.trip_id).copied();
            match records.get_mut(&trip.trip_id) {
                Some(record) => {
                    record.peak_delay_seconds = record.peak_delay_seconds.max(trip.delay_seconds);
                    record.last_seen = now;
                    record.ended_at = None;
                    record.trip = trip;
                    if let Some(span) = span {
                        record.span = span;
                        record.span_checked = true;
                    }
                    if let Some((lat, lon)) = position {
                        record.latitude = Some(lat);
                        record.longitude = Some(lon);
                    }
                    if let Some(receipt) = receipt {
                        record.tracked_seconds = receipt.tracked_seconds;
                        record.birth_delay_seconds = receipt.birth_delay_seconds;
                    }
                }
                None => {
                    records.insert(
                        trip.trip_id.clone(),
                        TripRecord {
                            peak_delay_seconds: trip.delay_seconds,
                            first_seen: now,
                            last_seen: now,
                            ended_at: None,
                            span: span.flatten(),
                            span_checked: span.is_some(),
                            tracked_seconds: receipt.map_or(0, |r| r.tracked_seconds),
                            birth_delay_seconds: receipt.map_or(0, |r| r.birth_delay_seconds),
                            latitude: position.map(|(lat, _)| lat),
                            longitude: position.map(|(_, lon)| lon),
                            trip,
                        },
                    );
                }
            }
        }
    }

    /// Remove trips from a feed's archive outright — for data we've decided never
    /// described a real run, so it shouldn't linger and decay as though it had.
    /// Returns how many records were actually removed (a `trip_id` may not have been
    /// archived yet), for the eviction metric.
    fn evict_archived(&self, idx: usize, trip_ids: &[String]) -> usize {
        if trip_ids.is_empty() {
            return 0;
        }
        let mut archive = self.archive.lock().unwrap();
        if let Some(records) = archive.get_mut(&idx) {
            let before = records.len();
            records.retain(|trip_id, _| !trip_ids.contains(trip_id));
            before - records.len()
        } else {
            0
        }
    }

    /// Whether agency `idx` currently has a **live** trip in the global top
    /// [`LEADERBOARD_SIZE`] by score — the signal that keeps a feed on the fast poll
    /// interval and earns it a static load and a positions fetch.
    ///
    /// Deliberately keyed on live trips only: a feed whose sole presence on the board
    /// is a finished trip decaying out has nothing left worth polling quickly for.
    /// The trips it competes *against*, though, are all of them — a live trip that a
    /// day's worth of decaying disasters have pushed off the board isn't hot either.
    ///
    /// Same O(n) count-who-beats-me trick as before rather than a sort: take this
    /// feed's best live score and count how many records anywhere beat it.
    fn on_leaderboard(&self, idx: usize) -> bool {
        let now = unix_now();
        let archive = self.archive.lock().unwrap();
        let Some(best) = archive.get(&idx).and_then(|records| {
            records
                .values()
                .filter(|record| record.ended_at.is_none())
                .map(|record| record.score(now))
                .max_by(f64::total_cmp)
        }) else {
            return false;
        };
        let ahead = archive
            .values()
            .flat_map(|records| records.values())
            .filter(|record| record.score(now) > best)
            .count();
        ahead < LEADERBOARD_SIZE
    }

    /// Age the archive: retire trips their feeds have stopped reporting, and drop the
    /// ones that have decayed past being worth keeping. Runs on the leaderboard tick.
    ///
    /// Three passes, in order:
    ///
    /// 1. **Retire.** A trip unmentioned for [`TRIP_END_GRACE`] has finished running.
    ///    Its decay clock is stamped at its *last sighting*, not now, so a slow feed
    ///    doesn't buy its trips extra time on the wall.
    /// 2. **Expire.** Past [`score::MAX_RETENTION_SECS`] the score is exactly zero, so
    ///    the record can go. This is the 24-hour horizon: nothing homesteads the wall
    ///    of shame.
    /// 3. **Cap.** Keep only the top [`ARCHIVE_CAP`] finished trips. The horizon alone
    ///    bounds memory, but loosely — at a day of every agency's late trips. This
    ///    keeps only the ones that could still plausibly rank.
    fn sweep_archive(&self) {
        let now = unix_now();
        let mut finished_by_grace = 0u64;
        let mut archive = self.archive.lock().unwrap();

        for records in archive.values_mut() {
            for record in records.values_mut() {
                if record.ended_at.is_none()
                    && now.saturating_sub(record.last_seen) > TRIP_END_GRACE
                {
                    record.ended_at = Some(record.last_seen);
                    finished_by_grace += 1;
                }
            }
            records.retain(|_, record| {
                record.ended_at.is_none_or(|ended| {
                    (now.saturating_sub(ended) as i64) < score::MAX_RETENTION_SECS
                })
            });
        }

        let mut finished: Vec<f64> = archive
            .values()
            .flat_map(|records| records.values())
            .filter(|record| record.ended_at.is_some())
            .map(|record| record.score(now))
            .collect();
        if finished.len() > ARCHIVE_CAP {
            finished.sort_unstable_by(|a, b| b.total_cmp(a));
            let cutoff = finished[ARCHIVE_CAP];
            for records in archive.values_mut() {
                records.retain(|_, record| record.ended_at.is_none() || record.score(now) > cutoff);
            }
        }

        archive.retain(|_, records| !records.is_empty());
        drop(archive);
        self.metrics.record_finished("grace", finished_by_grace);
    }

    /// Fetch every trip-updates URL for a feed and compute its delayed trips,
    /// alongside how many vehicles (trip updates) the feed carried.
    ///
    /// The network fetch runs under a [`limiter`](Self::limiter) permit; the
    /// CPU-bound decode + delay computation is offloaded to the blocking pool.
    async fn fetch_delayed_trips(
        &self,
        config: &AgencyConfig,
        gtfs: Option<Arc<Gtfs>>,
    ) -> Result<PollSuccess> {
        let entities = {
            let _permit = self.limiter.acquire().await.expect("semaphore stays open");
            let mut entities = Vec::new();
            let mut last_err = None;
            let mut ok = 0usize;
            for url in &config.realtime_urls.trip_updates_url {
                match realtime::fetch_feed(&self.client, &self.auth, url).await {
                    Ok(feed) => {
                        entities.extend(feed.entity);
                        ok += 1;
                    }
                    // A feed that merges several sub-feeds (Puget Sound polls one URL
                    // per OBA agency, MTA subway one per line) must not be sunk by a
                    // single flaky or auth-gated sub-feed — one `401` would otherwise
                    // propagate and *retire* the whole source. Log and press on with
                    // whatever answered; only propagate if *nothing* did, so a truly
                    // dead single-URL feed still hits the fatal-status retirement.
                    Err(err) => {
                        eprintln!(
                            "[{}] trip updates fetch failed for {url}: {err:#}",
                            config.display_name
                        );
                        last_err = Some(err);
                    }
                }
            }
            if ok == 0
                && let Some(err) = last_err
            {
                return Err(err);
            }
            entities
        };

        let vehicle_count = entities.iter().filter(|e| e.trip_update.is_some()).count();
        let feed = feed_message(entities);

        let gtfs_missing = gtfs.is_none();
        let (delays, needs_schedule) = tokio::task::spawn_blocking(move || {
            let delays = delay::delayed_trips(&feed, gtfs.as_deref());
            // Worth loading static only when we got nothing *and* had no schedule
            // to compare against — i.e. a times-only feed we haven't loaded yet.
            let needs_schedule =
                gtfs_missing && delays.trips.is_empty() && delay::needs_static_schedule(&feed);
            (delays, needs_schedule)
        })
        .await?;
        Ok(PollSuccess {
            trips: delays.trips,
            observations: delays.observations,
            vehicle_count,
            needs_schedule,
        })
    }

    /// Fetch a hot feed's vehicle-positions feed and store the per-trip
    /// coordinates, so [`leaderboard_snapshot`](Self::leaderboard_snapshot) can
    /// place its delayed vehicles on the map. Also **verifies each ranked trip is on
    /// its route**: a trip whose live vehicle sits more than [`OFF_ROUTE_KM`] from
    /// its shape is bad data (a mismatched trip/vehicle) and is dropped from the
    /// board so it never reaches the leaderboard. A fetch failure is logged and
    /// leaves the last positions in place. Runs under the shared fetch
    /// [`limiter`](Self::limiter).
    async fn update_vehicle_positions(&self, idx: usize) {
        let config = &self.configs[idx];
        let urls = &config.realtime_urls.vehicle_positions_url;
        if urls.is_empty() {
            return;
        }

        let mut positions = HashMap::new();
        {
            let _permit = self.limiter.acquire().await.expect("semaphore stays open");
            for url in urls {
                match realtime::fetch_feed(&self.client, &self.auth, url).await {
                    Ok(feed) => positions.extend(delay::vehicle_positions(&feed)),
                    Err(err) => eprintln!(
                        "[{}] vehicle positions fetch failed: {err:#}",
                        config.display_name
                    ),
                }
            }
        }

        if positions.is_empty() {
            return;
        }
        self.positions
            .lock()
            .unwrap()
            .insert(idx, positions.clone());
        self.drop_offroute_trips(idx, positions).await;
    }

    /// Drop from a feed's board any ranked trip its live vehicle position shows to be
    /// bad data, in two ways — both a trip/vehicle mismatch and a stationary vehicle
    /// out-growing the field:
    ///
    /// - **off-route** — the vehicle sits more than [`OFF_ROUTE_KM`] from its route
    ///   shape, so the `trip_id` doesn't describe the run it's driving;
    /// - **at a terminal** — the vehicle sits within [`TERMINAL_KM`] of either end of
    ///   its shape (its start or final terminal), i.e. it's parked at a layover, not
    ///   late en route; its reported delay is spurious.
    ///
    /// The two cases get **different treatment in the archive**, because they mean
    /// different things now that finished trips linger there:
    ///
    /// - an **off-route** vehicle says the `trip_id` never described the run being
    ///   driven, so the record is *evicted* — it's bad data, and bad data must not be
    ///   left to decay across the wall of shame for a day;
    /// - a vehicle **at a terminal** has, in the overwhelming majority of cases, just
    ///   *arrived*: this is what the end of a trip looks like. So the record is
    ///   stamped **finished** on the spot, freezing the last honest interior delay and
    ///   starting its decay immediately, rather than waiting out [`TRIP_END_GRACE`]
    ///   for the feed to drop it.
    ///
    /// Only checks trips we have both a position and a shape for; anything
    /// unverifiable is left in place. Either way the trip is only held off the *live*
    /// board — its provenance history keeps accruing (see `poll_once`), so a vehicle
    /// that pulls away from a layover and is genuinely late again is picked straight
    /// back up on its watched record. The shape lookups + distance math run on the
    /// blocking pool.
    async fn drop_offroute_trips(&self, idx: usize, positions: VehiclePositions) {
        let Some(gtfs) = self.loaded_static(idx) else {
            return;
        };

        // The (trip_id, position) pairs we can actually verify this poll.
        let to_check: Vec<(String, (f64, f64))> = {
            let boards = self.boards.lock().unwrap();
            boards
                .get(&idx)
                .map(|trips| {
                    trips
                        .iter()
                        .filter_map(|t| positions.get(&t.trip_id).map(|p| (t.trip_id.clone(), *p)))
                        .collect()
                })
                .unwrap_or_default()
        };
        if to_check.is_empty() {
            return;
        }

        // Each dropped trip tagged with why, for the log.
        let dropped: Vec<(String, &'static str)> = tokio::task::spawn_blocking(move || {
            to_check
                .into_iter()
                .filter_map(|(trip_id, (lat, lon))| {
                    let shape = gtfs.trip_shape(&trip_id)?;
                    if distance_to_path_km(lat, lon, &shape).is_some_and(|km| km > OFF_ROUTE_KM) {
                        return Some((trip_id, "off-route"));
                    }
                    // Distance to a single point (an endpoint) is just point-to-point.
                    let near_end = |end: (f64, f64)| {
                        distance_to_path_km(lat, lon, &[end]).is_some_and(|km| km < TERMINAL_KM)
                    };
                    if near_end(shape[0]) || near_end(shape[shape.len() - 1]) {
                        return Some((trip_id, "at-terminal"));
                    }
                    None
                })
                .collect()
        })
        .await
        .unwrap_or_default();

        if !dropped.is_empty() {
            let name = &self.configs[idx].display_name;
            let offroute = dropped
                .iter()
                .filter(|(_, why)| *why == "off-route")
                .count();
            eprintln!(
                "[{name}] dropped {offroute} off-route + {} at-terminal trip(s) from board",
                dropped.len() - offroute
            );
            let mut boards = self.boards.lock().unwrap();
            if let Some(trips) = boards.get_mut(&idx) {
                trips.retain(|t| !dropped.iter().any(|(id, _)| *id == t.trip_id));
            }
            drop(boards);

            // Off-route is bad data: erase it. At-terminal is an arrival: freeze it.
            let bogus: Vec<String> = dropped
                .iter()
                .filter(|(_, why)| *why == "off-route")
                .map(|(id, _)| id.clone())
                .collect();
            let evicted = self.evict_archived(idx, &bogus);
            self.metrics.record_evicted("off_route", evicted as u64);

            let now = unix_now();
            let mut finished = 0u64;
            let mut archive = self.archive.lock().unwrap();
            if let Some(records) = archive.get_mut(&idx) {
                for (trip_id, _) in dropped.iter().filter(|(_, why)| *why == "at-terminal") {
                    if let Some(record) = records.get_mut(trip_id)
                        && record.ended_at.is_none()
                    {
                        record.ended_at = Some(now);
                        finished += 1;
                    }
                }
            }
            drop(archive);
            self.metrics.record_finished("terminal", finished);
        }
    }

    /// Record a successful poll against a source's health. `late_trips` is the board
    /// *after* vetting; `vetted_out` is how many late trips the history refused.
    fn record_success(
        &self,
        idx: usize,
        vehicle_count: usize,
        late_trips: usize,
        vetted_out: usize,
    ) {
        let mut status = self.status.lock().unwrap();
        let runtime = &mut status[idx];
        runtime.last_poll = Some(unix_now());
        runtime.last_success = Some(true);
        runtime.last_error = None;
        runtime.vehicles_now = vehicle_count;
        runtime.late_trips = late_trips;
        runtime.vetted_out = vetted_out;
        runtime.peak_vehicles = runtime.peak_vehicles.max(vehicle_count);
        // A retried source that answers is back in business: return it to the
        // normal rotation (only `Active` and retrying `Failed` sources poll at
        // all, so this can't resurrect an auth-gated or realtime-less feed).
        if let SourceState::Failed(code) = runtime.state {
            eprintln!(
                "[{}] recovered from HTTP {code}, back in the rotation",
                self.configs[idx].display_name
            );
            runtime.state = SourceState::Active;
        }
        drop(status);
        self.metrics.record_poll(true);
        self.metrics.record_vetted_out(vetted_out as u64);
    }

    /// Record a failed poll. `fatal` carries the disabling HTTP status when the
    /// source is being taken out of the rotation. Returns whether that status
    /// *newly* retired the source — a source already `Failed` is just failing its
    /// periodic retry, which shouldn't re-log or re-count as a retirement.
    fn record_failure(&self, idx: usize, err: &anyhow::Error, fatal: Option<u16>) -> bool {
        let mut status = self.status.lock().unwrap();
        let runtime = &mut status[idx];
        runtime.last_poll = Some(unix_now());
        runtime.last_success = Some(false);
        runtime.last_error = Some(format!("{err:#}"));
        runtime.vehicles_now = 0;
        runtime.late_trips = 0;
        runtime.vetted_out = 0;
        let newly_failed = fatal.is_some() && !matches!(runtime.state, SourceState::Failed(_));
        if let Some(code) = fatal {
            runtime.state = SourceState::Failed(code);
            runtime.hot = false;
        }
        drop(status);
        self.metrics.record_poll(false);
        if newly_failed {
            self.metrics.record_retirement();
        }
        newly_failed
    }

    /// Flip a source's "downloading/importing static GTFS" flag for the status
    /// page. Set on either side of a zip fetch + SQLite build.
    fn set_loading(&self, idx: usize, loading: bool) {
        self.status.lock().unwrap()[idx].loading = loading;
    }

    /// Advance the leaderboard stream one tick and push the delta to every
    /// connected client.
    ///
    /// One delta serves all of them: they were all sent the same previous tick, so
    /// they all share its base. That's what lets the fan-out stay a single
    /// broadcast rather than a per-connection diff.
    ///
    /// The tick runs whether or not anyone is listening — the stream's retained
    /// state is what a *future* client is served as its full, so letting it go
    /// stale to save a sort would just hand the next visitor an old board.
    fn broadcast_update(&self) {
        let entries = rows(&self.leaderboard_snapshot().entries);
        let head = head([
            ("generated_at", json!(unix_now())),
            ("debug_enabled", json!(self.debug)),
        ]);
        let delta = self.board.lock().unwrap().advance(head, entries);
        let _ = self.updates.send(delta.into());
    }

    /// The same for the source-status stream, on its own (faster) tick.
    fn broadcast_status(&self) {
        let report = self.status_report();
        let head = head([
            ("generated_at", json!(report.generated_at)),
            ("summary", json!(report.summary)),
        ]);
        let delta = self
            .source_status
            .lock()
            .unwrap()
            .advance(head, rows(&report.sources));
        let _ = self.status_updates.send(delta.into());
    }

    /// Subscribe to leaderboard deltas. Each websocket client gets its own receiver.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<str>> {
        self.updates.subscribe()
    }

    /// Subscribe to source-status deltas.
    pub fn subscribe_status(&self) -> broadcast::Receiver<Arc<str>> {
        self.status_updates.subscribe()
    }

    /// The whole leaderboard as one self-contained message — what a client is sent
    /// on connect, before any delta can mean anything.
    pub fn board_full(&self) -> String {
        self.board.lock().unwrap().full()
    }

    /// The whole source-status report as one self-contained message. Served over
    /// HTTP rather than on the socket because it's ~176 KB and the socket has no
    /// compression, while the HTTP layer does (see [`crate::api`]).
    pub fn status_full(&self) -> String {
        self.source_status.lock().unwrap().full()
    }

    /// A cheap liveness/readiness check for `GET /api/healthz`.
    ///
    /// "Running well" here means the poll loop is actually turning, not merely that
    /// the process is up and axum answers — a wedged scheduler would still serve
    /// this endpoint while every feed went stale. So the signal is the freshest
    /// successful poll across all sources: if the newest one is older than
    /// [`HEALTH_STALE_AFTER`] (or none has happened yet), we're not healthy.
    ///
    /// Deliberately not built on [`status_report`](Self::status_report) — a load
    /// balancer may hit this every few seconds, and this is one lock and a scan of
    /// small `Copy` fields, not a 504-row serialization.
    pub fn health(&self) -> Health {
        let status = self.status.lock().unwrap();

        let active = status
            .iter()
            .filter(|runtime| runtime.state == SourceState::Active)
            .count();
        // The most recent *successful* poll anywhere. A failed poll still sets
        // last_poll, but a scheduler that's only failing isn't healthy, so we key
        // on success.
        let last_poll = status
            .iter()
            .filter(|runtime| runtime.last_success == Some(true))
            .filter_map(|runtime| runtime.last_poll)
            .max();

        let now = unix_now();
        let poll_age = last_poll.map(|t| now.saturating_sub(t));
        let ok = poll_age.is_some_and(|age| age <= HEALTH_STALE_AFTER);

        Health {
            ok,
            active_sources: active,
            last_success_age_seconds: poll_age,
            reason: if ok {
                None
            } else if last_poll.is_none() {
                Some("no feed has been polled successfully yet (warming up)")
            } else {
                Some("no successful poll within the freshness window (scheduler stalled?)")
            },
        }
    }

    /// Build the current global leaderboard: the worst [`LEADERBOARD_SIZE`] trips
    /// across every agency, ranked by [score](crate::score) — mostly delay, adjusted
    /// for how badly the delay breaks the trip and how much of it we watched happen,
    /// and faded out over 24 hours once the trip has finished running.
    ///
    /// Everything a row needs is already frozen into its [`TripRecord`], so this is a
    /// sort and a clone under one lock — no lookups against the live positions map or
    /// the trip history, neither of which still remembers a trip that ended hours ago.
    pub fn leaderboard_snapshot(&self) -> LeaderboardSnapshot {
        let now = unix_now();
        let archive = self.archive.lock().unwrap();
        let entries = ranked_records(&archive, now)
            .into_iter()
            .take(LEADERBOARD_SIZE)
            .enumerate()
            .map(|(rank, (_, idx, record))| {
                let config = &self.configs[idx];
                let trip = &record.trip;
                LeaderboardEntry {
                    rank: rank + 1,
                    agency: config.display_name.clone(),
                    slug: config.slug.clone(),
                    trip_id: trip.trip_id.clone(),
                    route: trip.route.clone(),
                    vehicle_type: trip.route_kind.map(gtfs::RouteKind::label),
                    headsign: trip.headsign.clone(),
                    next_stop: trip.next_stop.clone(),
                    vehicle: trip.vehicle.clone(),
                    delay_seconds: trip.delay_seconds,
                    peak_delay_seconds: record.peak_delay_seconds,
                    ended_at: record.ended_at,
                    source: trip.source.label(),
                    tracked_seconds: record.tracked_seconds,
                    birth_delay_seconds: record.birth_delay_seconds,
                    latitude: record.latitude,
                    longitude: record.longitude,
                    // Debug mode only — see the field's doc for why it isn't always on.
                    score_breakdown: self.debug.then(|| record.score_inputs(now).breakdown()),
                }
            })
            .collect();

        LeaderboardSnapshot {
            generated_at: now,
            entries,
            debug_enabled: self.debug,
        }
    }

    /// Snapshot every source's health for the `/status` endpoint.
    ///
    /// Every polled source (and every auth-gated / failed one) is reported, but
    /// the static-only `NoRealtime` feeds are trimmed to the largest
    /// [`NO_REALTIME_DISPLAY`] by scheduled-trip count — so the page highlights
    /// the biggest agencies we're missing realtime for, not every tiny feed.
    pub fn status_report(&self) -> StatusReport {
        let status = self.status.lock().unwrap();

        // The set of static-only feeds big enough to surface: the top N by trip
        // count. Feeds not yet counted (or that can't be counted) fall out.
        let shown_no_rt: std::collections::HashSet<usize> = {
            let mut ranked: Vec<(usize, usize)> = status
                .iter()
                .enumerate()
                .filter(|(_, runtime)| runtime.state == SourceState::NoRealtime)
                .filter_map(|(idx, runtime)| runtime.total_trips.map(|n| (idx, n)))
                .collect();
            ranked.sort_unstable_by_key(|&(_, trips)| Reverse(trips));
            ranked
                .into_iter()
                .take(NO_REALTIME_DISPLAY)
                .map(|(idx, _)| idx)
                .collect()
        };

        let mut summary = StatusSummary::default();

        let sources: Vec<SourceStatus> = self
            .configs
            .iter()
            .enumerate()
            .filter(|(idx, _)| {
                status[*idx].state != SourceState::NoRealtime || shown_no_rt.contains(idx)
            })
            .map(|(idx, config)| {
                let runtime = &status[idx];
                let (state, failed_status) = match runtime.state {
                    SourceState::Active => ("active", None),
                    SourceState::RequiresAuth => ("requires_auth", None),
                    SourceState::NoRealtime => ("no_realtime", None),
                    SourceState::NoVehiclePositions => ("no_vehicle_positions", None),
                    SourceState::Failed(code) => ("failed", Some(code)),
                };

                match runtime.state {
                    SourceState::Active => summary.active += 1,
                    SourceState::RequiresAuth => summary.requires_auth += 1,
                    SourceState::NoRealtime => summary.no_realtime += 1,
                    SourceState::NoVehiclePositions => summary.no_vehicle_positions += 1,
                    SourceState::Failed(_) => summary.failed += 1,
                }
                if runtime.hot {
                    summary.hot += 1;
                }
                if runtime.loading {
                    summary.loading += 1;
                }
                summary.vehicles_now += runtime.vehicles_now;

                let polled = runtime.state == SourceState::Active;

                SourceStatus {
                    slug: config.slug.clone(),
                    display_name: config.display_name.clone(),
                    country: config.country_code.clone(),
                    state,
                    failed_status,
                    hot: runtime.hot,
                    loading: runtime.loading,
                    poll_interval_seconds: polled.then_some(runtime.interval.as_secs()),
                    last_poll: runtime.last_poll,
                    last_success: runtime.last_success,
                    last_error: runtime.last_error.clone(),
                    vehicles_now: runtime.vehicles_now,
                    late_trips: runtime.late_trips,
                    vetted_out: runtime.vetted_out,
                    peak_vehicles: runtime.peak_vehicles,
                    total_trips: runtime.total_trips,
                }
            })
            .collect();

        summary.total_sources = sources.len();
        summary.static_loaded = self
            .static_gtfs
            .lock()
            .unwrap()
            .values()
            .filter(|g| g.is_some())
            .count();
        let (used, peak) = gtfs::sqlite_memory();
        summary.sqlite_bytes = used;
        summary.sqlite_peak_bytes = peak;
        summary.process_rss_bytes = gtfs::process_rss();

        StatusReport {
            generated_at: unix_now(),
            summary,
            sources,
        }
    }

    /// The shared metrics registry, for the `/metrics` handler and the API layer's
    /// own request counters.
    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }

    /// Take a one-shot snapshot of every gauge-style metric from the scheduler's
    /// live state, for [`Metrics::render`] at scrape time. Each lock is taken
    /// briefly and independently; nothing here is held across an `.await`.
    ///
    /// Deliberately not built on [`status_report`](Self::status_report): that trims
    /// the `no_realtime` feeds to the largest [`NO_REALTIME_DISPLAY`] for the page, so
    /// its summary would *undercount* them. Metrics want the true totals, so this
    /// counts every source directly.
    pub fn gauge_values(&self) -> GaugeValues {
        let now = unix_now();
        let mut values = GaugeValues::default();

        {
            let status = self.status.lock().unwrap();
            let mut newest_success: Option<u64> = None;
            for runtime in status.iter() {
                let bucket = match runtime.state {
                    SourceState::Active => 0,
                    SourceState::RequiresAuth => 1,
                    SourceState::NoRealtime => 2,
                    SourceState::NoVehiclePositions => 3,
                    SourceState::Failed(_) => 4,
                };
                values.sources_by_state[bucket] += 1;
                if runtime.hot {
                    values.sources_hot += 1;
                }
                if runtime.loading {
                    values.sources_loading += 1;
                }
                values.vehicles += runtime.vehicles_now as i64;
                values.late_trips += runtime.late_trips as i64;
                if let Some(n) = runtime.total_trips {
                    values.scheduled_trips += n as i64;
                }
                if runtime.last_success == Some(true)
                    && let Some(t) = runtime.last_poll
                {
                    newest_success = newest_success.max(Some(t));
                }
            }
            values.last_successful_poll_timestamp = newest_success.map_or(0, |t| t as i64);
            // Same rule as `health()`: healthy iff a success landed within the window.
            values.healthy =
                newest_success.is_some_and(|t| now.saturating_sub(t) <= HEALTH_STALE_AFTER) as i64;
        }

        values.sources_static_loaded = self
            .static_gtfs
            .lock()
            .unwrap()
            .values()
            .filter(|g| g.is_some())
            .count() as i64;

        {
            let archive = self.archive.lock().unwrap();
            for records in archive.values() {
                for record in records.values() {
                    values.archive_trips += 1;
                    if record.ended_at.is_some() {
                        values.archive_finished += 1;
                    }
                }
            }
        }

        // The public board — top and threshold delay, and how many are decaying out.
        let snapshot = self.leaderboard_snapshot();
        values.leaderboard_entries = snapshot.entries.len() as i64;
        values.leaderboard_finished = snapshot
            .entries
            .iter()
            .filter(|e| e.ended_at.is_some())
            .count() as i64;
        values.leaderboard_top_delay_seconds =
            snapshot.entries.first().map_or(0, |e| e.peak_delay_seconds);
        values.leaderboard_min_delay_seconds =
            snapshot.entries.last().map_or(0, |e| e.peak_delay_seconds);

        values.ws_connections = [
            self.updates.receiver_count() as i64,
            self.status_updates.receiver_count() as i64,
        ];

        let (sqlite_used, sqlite_peak) = gtfs::sqlite_memory();
        values.sqlite_bytes = sqlite_used;
        values.sqlite_peak_bytes = sqlite_peak;
        values.process_rss_bytes = gtfs::process_rss().unwrap_or(0) as i64;

        values
    }

    /// The geographic route path for one trip on one source, as ordered
    /// `(lat, lon)` points — what the map draws behind the delayed vehicle.
    ///
    /// On-demand and allocation-frugal: it only works off an **already-loaded**
    /// static schedule (the #1 vehicle's feed is hot, so its static is loaded),
    /// and the point list is read straight from the cached zip's `shapes.txt` for
    /// this one trip and never retained (see [`Gtfs::trip_shape`]). The zip read is
    /// CPU/IO-bound, so it runs on the blocking pool. Returns `None` when the
    /// source is unknown, its static isn't loaded yet, or the trip has no shape.
    pub async fn trip_shape(&self, slug: &str, trip_id: &str) -> Option<Vec<(f64, f64)>> {
        let idx = self.configs.iter().position(|c| c.slug == slug)?;
        let gtfs = self.loaded_static(idx)?;
        let trip_id = trip_id.to_string();
        tokio::task::spawn_blocking(move || gtfs.trip_shape(&trip_id))
            .await
            .ok()
            .flatten()
    }

    /// Collect everything used to compute one leaderboard entry into a zip archive
    /// under [`DEBUG_DIR`], for offline debugging — returns the archive's path.
    ///
    /// Deliberately over-collects (this is a developer tool, never user-facing):
    /// the agency config, the current per-source health, the **live** re-fetched
    /// trip-updates and vehicle-positions feeds (raw protobuf bytes *and* a decoded
    /// pretty-print), the recomputed [`DelayedTrip`] and leaderboard entry, the
    /// trip's static schedule rows, and a copy of the cached static GTFS zip +
    /// SQLite index — plus the operator's free-text `message`. The realtime feeds
    /// are re-fetched at capture time so the archive reflects the feed state *now*,
    /// when the anomaly is visible, not whenever the report is later opened.
    ///
    /// Gated on [`debug`](Self::debug); a no-op error otherwise.
    pub async fn capture_debug(&self, slug: &str, trip_id: &str, message: &str) -> Result<String> {
        if !self.debug {
            anyhow::bail!("debug capture is disabled (set AMD_DEBUG=1 to enable)");
        }
        let idx = self
            .configs
            .iter()
            .position(|c| c.slug == slug)
            .ok_or_else(|| anyhow::anyhow!("unknown source slug {slug}"))?;
        let config = &self.configs[idx];

        // Re-fetch the realtime feeds *now* so the archive captures the current
        // state. Each fetch is best-effort: a failure is recorded, not fatal.
        let mut tu_raw: Vec<(String, std::result::Result<Vec<u8>, String>)> = Vec::new();
        for url in &config.realtime_urls.trip_updates_url {
            let bytes = realtime::fetch_bytes(&self.client, &self.auth, url)
                .await
                .map_err(|e| format!("{e:#}"));
            tu_raw.push((url.clone(), bytes));
        }
        let mut vp_raw: Vec<(String, std::result::Result<Vec<u8>, String>)> = Vec::new();
        for url in &config.realtime_urls.vehicle_positions_url {
            let bytes = realtime::fetch_bytes(&self.client, &self.auth, url)
                .await
                .map_err(|e| format!("{e:#}"));
            vp_raw.push((url.clone(), bytes));
        }

        let decode_all = |raw: &[(String, std::result::Result<Vec<u8>, String>)]| {
            let mut entity = Vec::new();
            for (_, bytes) in raw {
                if let Ok(bytes) = bytes
                    && let Ok(feed) = FeedMessage::decode(bytes.as_slice())
                {
                    entity.extend(feed.entity);
                }
            }
            feed_message(entity)
        };
        let tu_feed = decode_all(&tu_raw);
        let vp_feed = decode_all(&vp_raw);

        // Recompute against the (possibly-loaded) static schedule so the archive
        // shows what the pipeline currently produces for this trip.
        let gtfs = self.loaded_static(idx);
        let delayed = delay::delayed_trips(&tu_feed, gtfs.as_deref());
        let computed = delayed.trips.iter().find(|t| t.trip_id == trip_id);
        let observation = delayed
            .observations
            .iter()
            .find(|o| o.trip_id == trip_id)
            .map(|o| {
                serde_json::json!({
                    "delay_seconds": o.delay_seconds,
                    "stop_sequence": o.stop_sequence,
                })
            });
        // What the delay history has watched this trip do — the record the vetting
        // gate acted on, and the first thing to look at when a capture asks why an
        // entry did (or didn't) make the board.
        let provenance = self.history.provenance(idx, trip_id).map(|p| {
            serde_json::json!({
                "tracked_seconds": p.tracked_seconds,
                "birth_delay_seconds": p.birth_delay_seconds,
            })
        });
        // The archive record behind this row: its lifecycle (when it first ranked,
        // when it was last seen, whether it has finished) and the full score
        // arithmetic. This is the first thing to look at when the question is "why is
        // this ranked here" or "why is this still on the board" — the ranking is a
        // score now, not a delay sort, and a finished trip is ranked on a number that
        // is still changing after the trip itself has stopped.
        let record_json = {
            let now = unix_now();
            let archive = self.archive.lock().unwrap();
            archive
                .get(&idx)
                .and_then(|records| records.get(trip_id))
                .map(|record| {
                    serde_json::json!({
                        "first_seen": record.first_seen,
                        "last_seen": record.last_seen,
                        "ended_at": record.ended_at,
                        "seconds_since_end": record.ended_at.map(|e| now.saturating_sub(e)),
                        "peak_delay_seconds": record.peak_delay_seconds,
                        "scheduled_duration_seconds": record.span.and_then(|s| s.duration_seconds),
                        "stops_total": record.span.map(|s| s.stops_total),
                        "stops_remaining": record.span.and_then(|s| s.stops_remaining),
                        "span_checked": record.span_checked,
                        "score": record.score(now),
                        "score_breakdown": record.score_inputs(now).breakdown(),
                    })
                })
        };

        let positions = delay::vehicle_positions(&vp_feed);
        let live_pos = positions.get(trip_id).copied();

        let snapshot = self.leaderboard_snapshot();
        let lb_entry = snapshot
            .entries
            .iter()
            .find(|e| e.slug == slug && e.trip_id == trip_id);

        let source_json = {
            let status = self.status.lock().unwrap();
            let r = &status[idx];
            let state = match r.state {
                SourceState::Active => "active".to_string(),
                SourceState::RequiresAuth => "requires_auth".to_string(),
                SourceState::NoRealtime => "no_realtime".to_string(),
                SourceState::NoVehiclePositions => "no_vehicle_positions".to_string(),
                SourceState::Failed(code) => format!("failed({code})"),
            };
            serde_json::json!({
                "slug": config.slug,
                "display_name": config.display_name,
                "country_code": config.country_code,
                "static_url": config.static_url,
                "trip_updates_urls": config.realtime_urls.trip_updates_url,
                "vehicle_positions_urls": config.realtime_urls.vehicle_positions_url,
                "requires_auth": config.requires_auth(),
                "state": state,
                "hot": r.hot,
                "loading": r.loading,
                "poll_interval_seconds": r.interval.as_secs(),
                "last_success": r.last_success,
                "last_error": r.last_error,
                "vehicles_now": r.vehicles_now,
                "late_trips": r.late_trips,
                "peak_vehicles": r.peak_vehicles,
                "total_trips": r.total_trips,
                "static_loaded": gtfs.is_some(),
            })
        };

        let computed_json = computed.map(|t| {
            serde_json::json!({
                "trip_id": t.trip_id,
                "route": t.route,
                "vehicle_type": t.route_kind.map(gtfs::RouteKind::label),
                "delay_seconds": t.delay_seconds,
                "source": t.source.label(),
                "headsign": t.headsign,
                "next_stop": t.next_stop,
                "vehicle": t.vehicle,
            })
        });

        let feed_summary = |raw: &[(String, std::result::Result<Vec<u8>, String>)]| {
            raw.iter()
                .map(|(url, bytes)| match bytes {
                    Ok(b) => serde_json::json!({ "url": url, "ok": true, "bytes": b.len() }),
                    Err(e) => serde_json::json!({ "url": url, "ok": false, "error": e }),
                })
                .collect::<Vec<_>>()
        };

        let meta = serde_json::json!({
            "captured_at": unix_now(),
            "message": message,
            "trip_id": trip_id,
            "source": source_json,
            "computed_delayed_trip": computed_json,
            "raw_observation": observation,
            "delay_history": provenance,
            "leaderboard_record": record_json,
            "delayed_trips_total": delayed.trips.len(),
            "leaderboard_entry": lb_entry,
            "on_leaderboard": lb_entry.is_some(),
            "live_position": live_pos.map(|(lat, lon)| serde_json::json!([lat, lon])),
            "static_trip": gtfs.as_ref().map(|g| g.debug_dump(trip_id)),
            "feeds": {
                "trip_updates": feed_summary(&tu_raw),
                "vehicle_positions": feed_summary(&vp_raw),
            },
        });

        // Assemble the in-memory archive members. Big on-disk files (the static
        // zip + SQLite) are copied in the blocking writer rather than read here.
        let matching: Vec<_> = tu_feed
            .entity
            .iter()
            .filter(|e| {
                e.trip_update
                    .as_ref()
                    .and_then(|tu| tu.trip.trip_id.as_deref())
                    == Some(trip_id)
            })
            .collect();

        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        files.push((
            "message.txt".to_string(),
            format!(
                "Debug capture for {} — trip {trip_id}\ncaptured_at (unix): {}\n\n{message}\n",
                config.display_name,
                unix_now(),
            )
            .into_bytes(),
        ));
        files.push((
            "meta.json".to_string(),
            serde_json::to_vec_pretty(&meta).unwrap_or_default(),
        ));
        for (i, (url, bytes)) in tu_raw.into_iter().enumerate() {
            match bytes {
                Ok(b) => files.push((format!("trip_updates/feed_{i}.pb"), b)),
                Err(e) => files.push((
                    format!("trip_updates/feed_{i}.error.txt"),
                    format!("{url}\n{e}").into_bytes(),
                )),
            }
        }
        files.push((
            "trip_updates/decoded_full.txt".to_string(),
            format!("{tu_feed:#?}").into_bytes(),
        ));
        files.push((
            "trip_updates/decoded_matching_trip.txt".to_string(),
            format!("{matching:#?}").into_bytes(),
        ));
        for (i, (url, bytes)) in vp_raw.into_iter().enumerate() {
            match bytes {
                Ok(b) => files.push((format!("vehicle_positions/feed_{i}.pb"), b)),
                Err(e) => files.push((
                    format!("vehicle_positions/feed_{i}.error.txt"),
                    format!("{url}\n{e}").into_bytes(),
                )),
            }
        }
        files.push((
            "vehicle_positions/decoded_full.txt".to_string(),
            format!("{vp_feed:#?}").into_bytes(),
        ));

        // Copy the cached static GTFS zip + SQLite index verbatim, when present.
        let copies: Vec<(String, std::path::PathBuf)> = [
            ("static/gtfs.zip", format!("{slug}.zip")),
            ("static/index.sqlite", format!("{slug}.sqlite")),
        ]
        .into_iter()
        .map(|(name, file)| (name.to_string(), Path::new(CACHE_DIR).join(file)))
        .filter(|(_, path)| path.exists())
        .collect();

        let safe_trip: String = trip_id
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();
        let out = Path::new(DEBUG_DIR).join(format!("{slug}_{safe_trip}_{}.zip", unix_now()));
        let out_display = out.display().to_string();

        tokio::task::spawn_blocking(move || write_debug_archive(&out, files, copies))
            .await?
            .with_context(|| format!("writing debug archive {out_display}"))?;
        println!(
            "[{}] debug capture written to {out_display}",
            config.display_name
        );
        Ok(out_display)
    }

    /// The static GTFS for a feed if it's already loaded, else `None`.
    fn loaded_static(&self, idx: usize) -> Option<Arc<Gtfs>> {
        self.static_gtfs
            .lock()
            .unwrap()
            .get(&idx)
            .cloned()
            .flatten()
    }

    /// Download and parse a feed's static GTFS when it's needed for richer labels.
    ///
    /// Loaded at most once per feed until [maintenance](Self::maintain_one) drops
    /// it as stale, at which point the next hot poll reloads it from the freshly
    /// re-fetched zip. Each feed has exactly one poll task, so this is never
    /// called concurrently for the same feed.
    async fn ensure_static_loaded(&self, idx: usize) {
        if self.static_gtfs.lock().unwrap().contains_key(&idx) {
            return;
        }

        let config = &self.configs[idx];
        self.set_loading(idx, true);
        let loaded = Gtfs::load(
            &config.slug,
            &config.display_name,
            &config.all_static_urls(),
            &self.client,
            Path::new(CACHE_DIR),
        )
        .await;
        self.set_loading(idx, false);
        self.metrics.record_static_load(loaded.is_ok());
        let value = match loaded {
            Ok(gtfs) => {
                println!("Loaded static GTFS for {}", config.display_name);
                self.status.lock().unwrap()[idx].total_trips = Some(gtfs.trip_count());
                Some(Arc::new(gtfs))
            }
            Err(err) => {
                eprintln!("[{}] static GTFS load failed: {err:#}", config.display_name);
                None
            }
        };
        self.static_gtfs.lock().unwrap().insert(idx, value);
    }

    /// Background maintenance: keep every agency's `total_trips` populated and its
    /// cached static GTFS fresh.
    ///
    /// The first pass is a one-time **census** that counts every agency's trips;
    /// each later pass (every [`MAINTENANCE_INTERVAL`]) re-fetches and re-counts
    /// only feeds whose cached zip has gone stale ([`gtfs::STATIC_TTL`]), so
    /// static schedules never drift far from the realtime feeds they're compared
    /// against — even across restarts, since staleness is judged by file mtime.
    async fn run_maintenance(self: Arc<Self>) {
        let limiter = Arc::new(Semaphore::new(STATIC_FETCH_CONCURRENCY));
        let mut first = true;
        loop {
            let handles: Vec<_> = (0..self.configs.len())
                .map(|idx| tokio::spawn(Arc::clone(&self).maintain_one(idx, Arc::clone(&limiter))))
                .collect();
            for handle in handles {
                let _ = handle.await;
            }
            if first {
                println!("Trip census complete");
                first = false;
            }
            tokio::time::sleep(MAINTENANCE_INTERVAL).await;
        }
    }

    /// Ensure one feed's trip count is known and its static cache is fresh,
    /// fetching (and re-counting) only when the count is missing or the cache has
    /// gone stale. A stale refresh also drops any loaded parsed copy so the next
    /// hot poll reloads it from the new zip.
    async fn maintain_one(self: Arc<Self>, idx: usize, limiter: Arc<Semaphore>) {
        let config = &self.configs[idx];

        // Static-only (`NoRealtime`) feeds are never polled or schedule-compared,
        // so their cache never needs refreshing — count their trips once for the
        // size ranking, then leave them alone. Polled feeds still refresh on the
        // TTL so their static schedule doesn't drift from the realtime feed.
        let (need_count, no_realtime) = {
            let runtime = &self.status.lock().unwrap()[idx];
            (
                runtime.total_trips.is_none(),
                runtime.state == SourceState::NoRealtime,
            )
        };
        // A multi-zip feed (MTA Bus) is stale if *any* of its backing zips is — one
        // borough refreshing forces the merged index to rebuild.
        let mut stale = false;
        if !no_realtime {
            for index in 0..config.all_static_urls().len() {
                let zip_path = gtfs::zip_cache_path(Path::new(CACHE_DIR), &config.slug, index);
                if gtfs::is_stale(&zip_path, gtfs::STATIC_TTL).await {
                    stale = true;
                    break;
                }
            }
        }
        if !stale && !need_count {
            return;
        }

        let count = {
            let _permit = limiter.acquire().await.expect("semaphore stays open");
            self.set_loading(idx, true);
            let result = gtfs::count_trips(
                &config.slug,
                &config.display_name,
                &config.all_static_urls(),
                &self.client,
                Path::new(CACHE_DIR),
            )
            .await;
            self.set_loading(idx, false);
            result
        };
        self.metrics.record_census(count.is_ok());
        match count {
            Ok(n) => self.status.lock().unwrap()[idx].total_trips = Some(n),
            Err(err) => eprintln!(
                "[{}] static census/refresh failed: {err:#}",
                config.display_name
            ),
        }

        if stale {
            self.static_gtfs.lock().unwrap().remove(&idx);
        }
    }

    /// Every [`PRINT_INTERVAL`], print the current leaderboard and push it to any
    /// connected websocket clients. Sharing this tick is what throttles the
    /// websocket to one update every 15s, regardless of how often feeds poll.
    async fn run_ticker(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(PRINT_INTERVAL);
        ticker.tick().await; // consume the immediate first tick
        loop {
            ticker.tick().await;
            // Retire trips whose feeds have stopped reporting them and expire the ones
            // that have decayed out, so the board about to be rendered is already aged.
            self.sweep_archive();
            self.broadcast_update();
        }
    }

    /// Every [`STATUS_INTERVAL`], advance the source-status stream. Its own tick,
    /// because the status page wants to see a poll land within a second or two,
    /// while the leaderboard is deliberately slow.
    ///
    /// This is the tick the delta protocol was written for: the full report is
    /// ~176 KB, and re-sending it at this rate is ~7.4 GB/day per viewer. The delta
    /// is whatever polls happened to land in the last two seconds — a few hundred
    /// bytes.
    async fn run_status_ticker(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(STATUS_INTERVAL);
        loop {
            ticker.tick().await;
            self.broadcast_status();
        }
    }
}

/// Wrap decoded entities in a `FeedMessage`. GTFS-realtime requires a header, but
/// nothing downstream reads ours — we only ever assemble feeds to run through the
/// delay pipeline (merging several trip-updates URLs, or re-decoding for a debug
/// capture), so a minimal `2.0` header is all it needs.
fn feed_message(entity: Vec<gtfs_rt::FeedEntity>) -> FeedMessage {
    FeedMessage {
        header: FeedHeader {
            gtfs_realtime_version: "2.0".to_string(),
            incrementality: None,
            timestamp: None,
        },
        entity,
    }
}

/// One tick's rows, serialized for a [`DeltaStream`].
fn rows<T: Serialize>(items: &[T]) -> Vec<Value> {
    items
        .iter()
        .filter_map(|item| serde_json::to_value(item).ok())
        .collect()
}

/// One tick's message-level fields — the parts of a message that aren't rows.
fn head<const N: usize>(fields: [(&str, Value); N]) -> Map<String, Value> {
    fields
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

/// Shortest distance in km from a point to a polyline (a route shape), via a local
/// equirectangular projection — accurate at the city scale we care about and far
/// cheaper than per-segment haversine. Measures to the nearest *segment*, not just
/// the nearest vertex, so a sparse shape doesn't read as far from an on-route
/// vehicle. `None` for an empty path.
fn distance_to_path_km(lat: f64, lon: f64, path: &[(f64, f64)]) -> Option<f64> {
    if path.is_empty() {
        return None;
    }
    const R_KM: f64 = 6371.0;
    let cos_lat = lat.to_radians().cos();
    let project = |la: f64, lo: f64| (lo.to_radians() * cos_lat * R_KM, la.to_radians() * R_KM);

    let (px, py) = project(lat, lon);
    let mut best = f64::INFINITY;
    let mut prev: Option<(f64, f64)> = None;
    for &(la, lo) in path {
        let (bx, by) = project(la, lo);
        let d = match prev {
            // Distance to the segment from the previous point to this one.
            Some((ax, ay)) => {
                let (dx, dy) = (bx - ax, by - ay);
                let len2 = dx * dx + dy * dy;
                let t = if len2 > 0.0 {
                    (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let (cx, cy) = (ax + t * dx, ay + t * dy);
                ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
            }
            None => ((px - bx).powi(2) + (py - by).powi(2)).sqrt(),
        };
        best = best.min(d);
        prev = Some((bx, by));
    }
    Some(best)
}

/// Write a debug-capture zip: every `(name, bytes)` in `files` as a stored/deflated
/// member, then each `(name, path)` in `copies` streamed verbatim from disk (the
/// large static zip + SQLite, already-compressed, so stored). Blocking; called from
/// [`Scheduler::capture_debug`] via the blocking pool.
fn write_debug_archive(
    out: &Path,
    files: Vec<(String, Vec<u8>)>,
    copies: Vec<(String, std::path::PathBuf)>,
) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let file = std::fs::File::create(out)?;
    let mut zip = zip::ZipWriter::new(file);
    let deflated = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    // Already-compressed payloads (a GTFS zip, a SQLite db): store, don't re-deflate.
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

    for (name, bytes) in files {
        zip.start_file(&name, deflated)?;
        zip.write_all(&bytes)?;
    }
    for (name, path) in copies {
        zip.start_file(&name, stored)?;
        let mut src =
            std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        std::io::copy(&mut src, &mut zip)?;
    }
    zip.finish()?;
    Ok(())
}

/// Pull the HTTP status out of a poll error, if the failure was an HTTP response
/// (as opposed to a timeout, DNS failure, or decode error, which carry none).
fn http_status(err: &anyhow::Error) -> Option<u16> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<reqwest::Error>())
        .and_then(|reqwest_err| reqwest_err.status())
        .map(|status| status.as_u16())
}

/// Whether debug capture is on, from the `AMD_DEBUG` env var. Any value other than
/// empty / `0` / `false` / `no` (case-sensitive) turns it on.
fn debug_enabled() -> bool {
    match std::env::var("AMD_DEBUG") {
        Ok(v) => !matches!(v.trim(), "" | "0" | "false" | "no"),
        Err(_) => false,
    }
}

/// Current wall-clock time as Unix seconds (0 if the clock predates the epoch).
fn unix_now() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

/// Every archived trip across all agencies, highest-scoring first, tagged with its
/// score and agency index. Shared by the websocket snapshot and the console printer.
///
/// The score is computed once per record and carried, rather than recomputed inside
/// the comparator — a sort calls its comparator O(n log n) times, and the score isn't
/// free.
fn ranked_records(
    archive: &HashMap<usize, HashMap<String, TripRecord>>,
    now: u64,
) -> Vec<(f64, usize, &TripRecord)> {
    let mut ranked: Vec<(f64, usize, &TripRecord)> = archive
        .iter()
        .flat_map(|(&idx, records)| {
            records
                .values()
                .map(move |record| (record.score(now), idx, record))
        })
        .collect();
    ranked.sort_by(|(a, _, _), (b, _, _)| b.total_cmp(a));
    ranked
}

/// Render a delay in seconds as e.g. `"1h 12m 30s"`.
fn format_delay(seconds: i64) -> String {
    let (h, m, s) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, _) => format!("{m}m {s}s"),
        _ => format!("{h}h {m}m {s}s"),
    }
}

/// Start the polling system: spawn one task per pollable feed (first polls
/// staggered across [`BASE_INTERVAL`]) plus the printer/broadcast ticker, and
/// return the shared [`Scheduler`] handle for the API layer to read. Must be
/// called from within a Tokio runtime.
pub fn start(configs: Vec<AgencyConfig>, auth: Arc<FeedAuth>) -> Result<Arc<Scheduler>> {
    // Reclaim any temp files a previous run left mid-build before we start writing
    // new ones — nothing is building yet, so every `*.tmp`/`*.sqltmp` present is stale.
    gtfs::sweep_stale_temp_files(Path::new(CACHE_DIR));

    let client = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()?;
    let debug = debug_enabled();
    if debug {
        println!("Debug capture enabled (AMD_DEBUG): per-row capture buttons active");
    }
    let scheduler = Arc::new(Scheduler::new(configs, client, auth, debug));

    let pollable = scheduler.pollable();
    let count = pollable.len();
    for (position, idx) in pollable.into_iter().enumerate() {
        let stagger = BASE_INTERVAL.mul_f64(position as f64 / count as f64);
        tokio::spawn(Arc::clone(&scheduler).run_feed(idx, stagger));
    }

    tokio::spawn(Arc::clone(&scheduler).run_ticker());
    tokio::spawn(Arc::clone(&scheduler).run_status_ticker());
    tokio::spawn(Arc::clone(&scheduler).run_maintenance());
    Ok(scheduler)
}
