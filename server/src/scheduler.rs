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
use crate::delay::{self, DelayedTrip, TripObservation, VehiclePositions};
use crate::gtfs::{self, Gtfs};
use crate::history::TripHistory;
use crate::realtime;
use crate::wire::DeltaStream;

/// Ring-buffer capacity for the live-update broadcast. A websocket client that
/// falls this far behind is resynced from a fresh snapshot rather than the
/// buffered deltas, so this only needs to absorb brief bursts.
const UPDATE_BUFFER: usize = 64;

/// HTTP statuses that mark a source as permanently broken: unauthorized (`401`)
/// or gone (`404`). We stop polling a source the first time it returns one.
const FATAL_STATUSES: [u16; 2] = [401, 404];

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
    pub headsign: Option<String>,
    pub next_stop: Option<String>,
    pub vehicle: Option<String>,
    pub delay_seconds: i64,
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
    /// Disabled after the feed returned a [`FATAL_STATUSES`] code; never retried.
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

/// The full `/status` response.
#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub generated_at: u64,
    pub summary: StatusSummary,
    pub sources: Vec<SourceStatus>,
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
    /// Caps how many feed fetches are in flight at once.
    limiter: Semaphore,
    /// Latest delayed trips per agency index; the leaderboard is derived from it.
    boards: Mutex<HashMap<usize, Vec<DelayedTrip>>>,
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
    /// Whether debug capture (`AMD_DEBUG`) is on. Gates [`capture_debug`](Self::capture_debug)
    /// and is surfaced to the frontend via each snapshot's `debug_enabled`.
    debug: bool,
}

impl Scheduler {
    fn new(configs: Vec<AgencyConfig>, client: Client, debug: bool) -> Self {
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
            limiter: Semaphore::new(MAX_CONCURRENT_POLLS),
            boards: Mutex::new(HashMap::new()),
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

    /// One feed's polling loop: poll, then sleep its current interval, forever —
    /// until a fatal status retires it. The initial `stagger` spreads the first
    /// polls across [`BASE_INTERVAL`] so we don't fire hundreds of requests at
    /// once.
    async fn run_feed(self: Arc<Self>, idx: usize, stagger: Duration) {
        tokio::time::sleep(stagger).await;
        let mut interval = BASE_INTERVAL;
        while let Some(next) = self.poll_once(idx, interval).await {
            interval = next;
            tokio::time::sleep(interval).await;
        }
    }

    /// Poll one feed once: update its board and health, and return the interval
    /// until its next poll — or `None` to retire it (after a fatal status).
    async fn poll_once(&self, idx: usize, current_interval: Duration) -> Option<Duration> {
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
                let vetted_out =
                    self.history
                        .vet(idx, &observations, &mut trips, unix_now() as i64);
                self.record_success(idx, vehicle_count, trips.len(), vetted_out);
                self.boards.lock().unwrap().insert(idx, trips);
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
                // A 401/404 means this feed is gone or gated: retire it. Its board
                // is cleared and its task ends.
                if let Some(status) = http_status(&err).filter(|s| FATAL_STATUSES.contains(s)) {
                    eprintln!(
                        "[{}] disabling source after HTTP {status}",
                        config.display_name
                    );
                    self.record_failure(idx, &err, Some(status));
                    self.boards.lock().unwrap().remove(&idx);
                    self.positions.lock().unwrap().remove(&idx);
                    self.history.forget_source(idx);
                    return None;
                }
                self.record_failure(idx, &err, None);
            }
        }

        // Is this feed hot (a vehicle in the global top N)? That both keeps it on
        // the fast interval and earns it a static-feed load for richer labels —
        // and its live vehicle positions, so the map can show the delayed vehicle.
        // Fetching positions also prunes off-route trips from the board, so we
        // recompute hotness afterward: a feed whose only delayed trips were bogus
        // drops out of the top N and backs off.
        if leaderboard_contains(&self.boards.lock().unwrap(), idx) {
            self.ensure_static_loaded(idx).await;
            self.update_vehicle_positions(idx).await;
        }
        let hot = leaderboard_contains(&self.boards.lock().unwrap(), idx);
        let next_interval = if hot {
            BASE_INTERVAL
        } else {
            (current_interval * 2).min(MAX_INTERVAL)
        };

        let mut status = self.status.lock().unwrap();
        status[idx].hot = hot;
        status[idx].interval = next_interval;
        Some(next_interval)
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
            for url in config.realtime_urls.trip_updates_url() {
                entities.extend(realtime::fetch_feed(&self.client, url).await?.entity);
            }
            entities
        };

        let vehicle_count = entities.iter().filter(|e| e.trip_update.is_some()).count();
        let feed = FeedMessage {
            header: FeedHeader {
                gtfs_realtime_version: "2.0".to_string(),
                incrementality: None,
                timestamp: None,
            },
            entity: entities,
        };

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
        let urls = config.realtime_urls.vehicle_positions_url();
        if urls.is_empty() {
            return;
        }

        let mut positions = HashMap::new();
        {
            let _permit = self.limiter.acquire().await.expect("semaphore stays open");
            for url in urls {
                match realtime::fetch_feed(&self.client, url).await {
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

    /// Drop from a feed's board any ranked trip whose live vehicle is more than
    /// [`OFF_ROUTE_KM`] from its route shape — a trip/vehicle mismatch we don't want
    /// on the leaderboard. Only checks trips we have both a position and a shape for;
    /// anything unverifiable is left in place. The shape lookups + distance math run
    /// on the blocking pool.
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

        let offroute: Vec<String> = tokio::task::spawn_blocking(move || {
            to_check
                .into_iter()
                .filter(|(trip_id, (lat, lon))| {
                    gtfs.trip_shape(trip_id)
                        .and_then(|shape| distance_to_path_km(*lat, *lon, &shape))
                        .is_some_and(|km| km > OFF_ROUTE_KM)
                })
                .map(|(trip_id, _)| trip_id)
                .collect()
        })
        .await
        .unwrap_or_default();

        if !offroute.is_empty() {
            let name = &self.configs[idx].display_name;
            eprintln!(
                "[{name}] dropped {} off-route trip(s) from board",
                offroute.len()
            );
            let mut boards = self.boards.lock().unwrap();
            if let Some(trips) = boards.get_mut(&idx) {
                trips.retain(|t| !offroute.contains(&t.trip_id));
            }
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
    }

    /// Record a failed poll. `fatal` carries the disabling HTTP status when the
    /// source is being retired.
    fn record_failure(&self, idx: usize, err: &anyhow::Error, fatal: Option<u16>) {
        let mut status = self.status.lock().unwrap();
        let runtime = &mut status[idx];
        runtime.last_poll = Some(unix_now());
        runtime.last_success = Some(false);
        runtime.last_error = Some(format!("{err:#}"));
        runtime.vehicles_now = 0;
        runtime.late_trips = 0;
        runtime.vetted_out = 0;
        if let Some(code) = fatal {
            runtime.state = SourceState::Failed(code);
            runtime.hot = false;
        }
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

    /// Build the current global leaderboard: the worst [`LEADERBOARD_SIZE`]
    /// delayed trips across every agency, ranked most-delayed first.
    pub fn leaderboard_snapshot(&self) -> LeaderboardSnapshot {
        let boards = self.boards.lock().unwrap();
        let positions = self.positions.lock().unwrap();
        let entries = ranked_trips(&boards)
            .into_iter()
            .take(LEADERBOARD_SIZE)
            .enumerate()
            .map(|(rank, (idx, trip))| {
                let config = &self.configs[idx];
                let position = positions.get(&idx).and_then(|feed| feed.get(&trip.trip_id));
                let provenance = self.history.provenance(idx, &trip.trip_id);
                LeaderboardEntry {
                    rank: rank + 1,
                    agency: config.display_name.clone(),
                    slug: config.slug.clone(),
                    trip_id: trip.trip_id.clone(),
                    route: trip.route.clone(),
                    headsign: trip.headsign.clone(),
                    next_stop: trip.next_stop.clone(),
                    vehicle: trip.vehicle.clone(),
                    delay_seconds: trip.delay_seconds,
                    source: trip.source.label(),
                    tracked_seconds: provenance.map_or(0, |p| p.tracked_seconds),
                    birth_delay_seconds: provenance.map_or(0, |p| p.birth_delay_seconds),
                    latitude: position.map(|(lat, _)| *lat),
                    longitude: position.map(|(_, lon)| *lon),
                }
            })
            .collect();

        LeaderboardSnapshot {
            generated_at: unix_now(),
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
        for url in config.realtime_urls.trip_updates_url() {
            let bytes = realtime::fetch_bytes(&self.client, url)
                .await
                .map_err(|e| format!("{e:#}"));
            tu_raw.push((url.clone(), bytes));
        }
        let mut vp_raw: Vec<(String, std::result::Result<Vec<u8>, String>)> = Vec::new();
        for url in config.realtime_urls.vehicle_positions_url() {
            let bytes = realtime::fetch_bytes(&self.client, url)
                .await
                .map_err(|e| format!("{e:#}"));
            vp_raw.push((url.clone(), bytes));
        }

        let header = || FeedHeader {
            gtfs_realtime_version: "2.0".to_string(),
            incrementality: None,
            timestamp: None,
        };
        let decode_all = |raw: &[(String, std::result::Result<Vec<u8>, String>)]| {
            let mut entity = Vec::new();
            for (_, bytes) in raw {
                if let Ok(bytes) = bytes
                    && let Ok(feed) = FeedMessage::decode(bytes.as_slice())
                {
                    entity.extend(feed.entity);
                }
            }
            FeedMessage {
                header: header(),
                entity,
            }
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
                "trip_updates_urls": config.realtime_urls.trip_updates_url(),
                "vehicle_positions_urls": config.realtime_urls.vehicle_positions_url(),
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
            &config.static_url,
            &self.client,
            Path::new(CACHE_DIR),
        )
        .await;
        self.set_loading(idx, false);
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
        let zip_path = Path::new(CACHE_DIR).join(format!("{}.zip", config.slug));

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
        let stale = !no_realtime && gtfs::is_stale(&zip_path, gtfs::STATIC_TTL).await;
        if !stale && !need_count {
            return;
        }

        let count = {
            let _permit = limiter.acquire().await.expect("semaphore stays open");
            self.set_loading(idx, true);
            let result = gtfs::count_trips(
                &config.slug,
                &config.display_name,
                &config.static_url,
                &self.client,
                Path::new(CACHE_DIR),
            )
            .await;
            self.set_loading(idx, false);
            result
        };
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
            print_leaderboard(&self.boards.lock().unwrap(), &self.configs);
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

/// Whether agency `idx` currently has a vehicle in the global top
/// [`LEADERBOARD_SIZE`] by delay.
///
/// Rather than sorting every trip in the system on each poll, we take this
/// agency's worst delay and count how many trips across all agencies are
/// strictly worse: if fewer than [`LEADERBOARD_SIZE`] beat it, it's on the
/// board. That's a single O(n) pass with no allocation.
fn leaderboard_contains(boards: &HashMap<usize, Vec<DelayedTrip>>, idx: usize) -> bool {
    let Some(best) = boards
        .get(&idx)
        .and_then(|trips| trips.iter().map(|t| t.delay_seconds).max())
    else {
        return false;
    };
    let ahead = boards
        .values()
        .flatten()
        .filter(|t| t.delay_seconds > best)
        .count();
    ahead < LEADERBOARD_SIZE
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

/// Every delayed trip across all agencies, most-delayed first, tagged with its
/// agency index. Shared by the websocket snapshot and the console printer.
fn ranked_trips(boards: &HashMap<usize, Vec<DelayedTrip>>) -> Vec<(usize, &DelayedTrip)> {
    let mut ranked: Vec<(usize, &DelayedTrip)> = boards
        .iter()
        .flat_map(|(&idx, trips)| trips.iter().map(move |trip| (idx, trip)))
        .collect();
    ranked.sort_by_key(|(_, trip)| Reverse(trip.delay_seconds));
    ranked
}

/// Print the top delayed trips across every agency.
fn print_leaderboard(boards: &HashMap<usize, Vec<DelayedTrip>>, configs: &[AgencyConfig]) {
    let ranked = ranked_trips(boards);

    println!("\n=== America's Most Delayed ===");
    if ranked.is_empty() {
        println!("(no delays reported right now)");
    }
    for (rank, (idx, trip)) in ranked.iter().take(LEADERBOARD_SIZE).enumerate() {
        let agency = configs[*idx].display_name.as_str();
        let headsign = trip
            .headsign
            .as_deref()
            .map(|h| format!(" → {h}"))
            .unwrap_or_default();
        println!(
            "{}. {} — route {}{} — {} late",
            rank + 1,
            agency,
            trip.route,
            headsign,
            format_delay(trip.delay_seconds),
        );

        let mut details = Vec::new();
        if let Some(next_stop) = &trip.next_stop {
            details.push(format!("next stop {next_stop}"));
        }
        if let Some(vehicle) = &trip.vehicle {
            details.push(format!("bus {vehicle}"));
        }
        details.push(format!("trip {}", trip.trip_id));
        details.push(format!("[{}]", trip.source.label()));
        println!("     {}", details.join(" · "));
    }
    println!();
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
pub fn start(configs: Vec<AgencyConfig>) -> Result<Arc<Scheduler>> {
    let client = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()?;
    let debug = debug_enabled();
    if debug {
        println!("Debug capture enabled (AMD_DEBUG): per-row capture buttons active");
    }
    let scheduler = Arc::new(Scheduler::new(configs, client, debug));

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
