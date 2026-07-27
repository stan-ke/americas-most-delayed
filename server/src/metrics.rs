//! Prometheus metrics for the whole pipeline, exposed at `GET /metrics`.
//!
//! This is instrumentation for the **operator's own** Prometheus, scraped
//! server-side over the VPS's private/monitoring path — not the public API the
//! frontend hits. So the egress discipline the rest of the crate lives by (see
//! [`crate::wire`]) doesn't apply here the way it does to `/api/*`: a scrape every
//! 15–60s from one Prometheus is not 500 browser tabs. That frees the endpoint to
//! be comprehensive.
//!
//! The metrics split cleanly into two kinds, which is the whole shape of this file:
//!
//! - **Counters** — cumulative process-lifetime events (polls, evictions, static
//!   loads, HTTP hits). These are incremented at the point the event happens, from
//!   the scheduler's hot paths and the API handlers, into the atomic handles held
//!   here. Every counter/label combination is materialized at startup so a series
//!   reads `0` from the first scrape rather than popping into existence later.
//! - **Gauges** — the current state of the world (sources by state, vehicles now,
//!   the top delay, RSS). These can't be "incremented"; they *are* whatever the
//!   scheduler's shared state says right now. Rather than mirror-updating a gauge on
//!   every state change, [`Scheduler::gauge_values`](crate::scheduler::Scheduler::gauge_values)
//!   takes one cheap snapshot at **scrape time** and [`Metrics::snapshot`] writes it
//!   into the gauge handles just before the registry is encoded. A gauge is
//!   therefore always at most one scrape stale and never drifts.
//!
//! Following the same "expose the timestamp, derive the age" rule the wire protocol
//! uses, health is published as `amd_last_successful_poll_timestamp_seconds` (a unix
//! time) rather than an age, so Prometheus derives freshness with
//! `time() - amd_last_successful_poll_timestamp_seconds` and the value only moves
//! when a poll actually lands.

use prometheus_client::encoding::{EncodeLabelSet, text::encode};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

/// The OpenMetrics content type [`text::encode`](encode) produces (it emits a
/// trailing `# EOF`). Prometheus scrapers advertise and accept this.
pub const CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// `{result="success|failure"}`-style single-label sets. `&'static str` implements
/// `EncodeLabelValue`, so the label values stay allocation-free.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ResultLabel {
    result: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct StateLabel {
    state: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ReasonLabel {
    reason: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct StreamLabel {
    stream: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct EndpointLabel {
    endpoint: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct VersionLabel {
    version: &'static str,
}

/// The `active`/`requires_auth`/... labels a source's state serializes to — the same
/// strings `/status` uses, kept in one place so the two agree.
pub const SOURCE_STATES: [&str; 5] = [
    "active",
    "requires_auth",
    "no_realtime",
    "no_vehicle_positions",
    "failed",
];

/// The two websocket streams, by the label they report under.
const WS_STREAMS: [&str; 2] = ["leaderboard", "status"];

/// The `/api/*` endpoints counted by `amd_http_requests_total`. Materialized up
/// front so each reads `0` before its first hit.
const HTTP_ENDPOINTS: [&str; 7] = [
    "status",
    "status_live",
    "subscribe",
    "shape",
    "healthz",
    "debug_capture",
    "metrics",
];

/// A plain snapshot of everything the gauges report — filled by the scheduler at
/// scrape time and handed to [`Metrics::snapshot`]. Keeping it a dumb data struct is
/// what stops `prometheus_client` types from leaking into the scheduler.
#[derive(Debug, Default, Clone)]
pub struct GaugeValues {
    /// Sources in each [`SOURCE_STATES`] state, in that order.
    pub sources_by_state: [i64; 5],
    pub sources_hot: i64,
    pub sources_loading: i64,
    pub sources_static_loaded: i64,
    /// Trip updates (vehicles) all feeds are publishing right now.
    pub vehicles: i64,
    /// Trips currently ranked late across all feeds (sum of per-feed board sizes).
    pub late_trips: i64,
    /// Trips in every agency's static schedule — the fleet's total scale.
    pub scheduled_trips: i64,
    /// Trips held in the archive (ranked recently), and how many have finished.
    pub archive_trips: i64,
    pub archive_finished: i64,
    /// The public leaderboard: entries shown, how many are finished/decaying, and the
    /// worst and threshold delays on it.
    pub leaderboard_entries: i64,
    pub leaderboard_finished: i64,
    pub leaderboard_top_delay_seconds: i64,
    pub leaderboard_min_delay_seconds: i64,
    /// 1 when the poll loop is turning, else 0 (the `/healthz` verdict).
    pub healthy: i64,
    /// Unix time of the freshest successful poll anywhere; 0 if none yet.
    pub last_successful_poll_timestamp: i64,
    /// Live websocket subscribers, `[leaderboard, status]`.
    pub ws_connections: [i64; 2],
    pub process_rss_bytes: i64,
    pub sqlite_bytes: i64,
    pub sqlite_peak_bytes: i64,
}

/// All Prometheus state: the registry plus every metric handle. Cheap to share —
/// each handle is internally reference-counted, so the scheduler and the API
/// handlers hold clones that write the same atomics. Held behind an `Arc` on the
/// [`Scheduler`](crate::scheduler::Scheduler).
pub struct Metrics {
    registry: Registry,

    // ---- Counters (incremented at the event). ----
    /// `amd_polls_total{result}` — poll attempts by outcome.
    polls: Family<ResultLabel, Counter>,
    /// `amd_source_retirements_total` — sources dropped out of the poll rotation
    /// after a fatal HTTP status. Counted once per retirement, not once per failed
    /// retry, so a source that flaps counts each time it goes down.
    source_retirements: Counter,
    /// `amd_trips_vetted_out_total` — late trips the provenance gate refused to
    /// vouch for (see [`crate::history`]).
    trips_vetted_out: Counter,
    /// `amd_trips_evicted_total{reason}` — archived trips erased as bad data.
    trips_evicted: Family<ReasonLabel, Counter>,
    /// `amd_trips_finished_total{reason}` — trips stamped finished (arrival vs. the
    /// grace-period sweep).
    trips_finished: Family<ReasonLabel, Counter>,
    /// `amd_static_loads_total{result}` — full static-GTFS loads (the SQLite build).
    static_loads: Family<ResultLabel, Counter>,
    /// `amd_static_census_total{result}` — background census/refresh counts.
    static_census: Family<ResultLabel, Counter>,
    /// `amd_http_requests_total{endpoint}` — `/api/*` requests served.
    http_requests: Family<EndpointLabel, Counter>,
    /// `amd_shape_requests_total{result}` — shape lookups, hit (has a polyline) vs.
    /// miss (static not loaded / no shape).
    shape_requests: Family<ResultLabel, Counter>,
    /// `amd_debug_captures_total{result}` — debug-capture attempts.
    debug_captures: Family<ResultLabel, Counter>,
    /// `amd_websocket_connections_opened_total{stream}` — sockets opened, ever.
    ws_opened: Family<StreamLabel, Counter>,

    // ---- Gauges (written from a snapshot at scrape time). ----
    sources: Family<StateLabel, Gauge>,
    sources_hot: Gauge,
    sources_loading: Gauge,
    sources_static_loaded: Gauge,
    vehicles: Gauge,
    late_trips: Gauge,
    scheduled_trips: Gauge,
    archive_trips: Gauge,
    archive_finished: Gauge,
    leaderboard_entries: Gauge,
    leaderboard_finished: Gauge,
    leaderboard_top_delay: Gauge,
    leaderboard_min_delay: Gauge,
    healthy: Gauge,
    last_successful_poll: Gauge,
    ws_connections: Family<StreamLabel, Gauge>,
    process_rss: Gauge,
    sqlite_bytes: Gauge,
    sqlite_peak: Gauge,
}

impl Metrics {
    /// Build the registry and register every metric. `debug` (the `AMD_DEBUG` flag)
    /// and the start time are constant for the process, so they're set once here.
    pub fn new(debug: bool, start_time: u64) -> Self {
        let mut registry = Registry::with_prefix("amd");

        let polls = Family::<ResultLabel, Counter>::default();
        registry.register("polls", "Feed poll attempts by outcome", polls.clone());
        // Materialize both outcomes so each starts at 0.
        for result in ["success", "failure"] {
            let _ = polls.get_or_create(&ResultLabel { result });
        }

        let source_retirements = Counter::default();
        registry.register(
            "source_retirements",
            "Sources dropped from the poll rotation after a fatal HTTP status (401/404)",
            source_retirements.clone(),
        );

        let trips_vetted_out = Counter::default();
        registry.register(
            "trips_vetted_out",
            "Late trips the delay-provenance gate refused to vouch for",
            trips_vetted_out.clone(),
        );

        let trips_evicted = Family::<ReasonLabel, Counter>::default();
        registry.register(
            "trips_evicted",
            "Archived trips erased as bad data, by reason",
            trips_evicted.clone(),
        );
        for reason in ["off_route", "falsified"] {
            let _ = trips_evicted.get_or_create(&ReasonLabel { reason });
        }

        let trips_finished = Family::<ReasonLabel, Counter>::default();
        registry.register(
            "trips_finished",
            "Trips stamped finished, by reason",
            trips_finished.clone(),
        );
        for reason in ["terminal", "grace"] {
            let _ = trips_finished.get_or_create(&ReasonLabel { reason });
        }

        let static_loads = Family::<ResultLabel, Counter>::default();
        registry.register(
            "static_loads",
            "Full static-GTFS loads (SQLite index build), by outcome",
            static_loads.clone(),
        );
        for result in ["ok", "error"] {
            let _ = static_loads.get_or_create(&ResultLabel { result });
        }

        let static_census = Family::<ResultLabel, Counter>::default();
        registry.register(
            "static_census",
            "Background static-GTFS census/refresh counts, by outcome",
            static_census.clone(),
        );
        for result in ["ok", "error"] {
            let _ = static_census.get_or_create(&ResultLabel { result });
        }

        let http_requests = Family::<EndpointLabel, Counter>::default();
        registry.register(
            "http_requests",
            "API requests served, by endpoint",
            http_requests.clone(),
        );
        for endpoint in HTTP_ENDPOINTS {
            let _ = http_requests.get_or_create(&EndpointLabel { endpoint });
        }

        let shape_requests = Family::<ResultLabel, Counter>::default();
        registry.register(
            "shape_requests",
            "Route-shape lookups, hit (polyline returned) vs. miss",
            shape_requests.clone(),
        );
        for result in ["hit", "miss"] {
            let _ = shape_requests.get_or_create(&ResultLabel { result });
        }

        let debug_captures = Family::<ResultLabel, Counter>::default();
        registry.register(
            "debug_captures",
            "Debug-capture attempts, by outcome",
            debug_captures.clone(),
        );
        for result in ["ok", "error"] {
            let _ = debug_captures.get_or_create(&ResultLabel { result });
        }

        let ws_opened = Family::<StreamLabel, Counter>::default();
        registry.register(
            "websocket_connections_opened",
            "Websocket connections opened, by stream",
            ws_opened.clone(),
        );
        for stream in WS_STREAMS {
            let _ = ws_opened.get_or_create(&StreamLabel { stream });
        }

        // ---- Gauges. ----
        let sources = Family::<StateLabel, Gauge>::default();
        registry.register("sources", "Configured sources by state", sources.clone());
        for state in SOURCE_STATES {
            let _ = sources.get_or_create(&StateLabel { state });
        }

        let sources_hot = Gauge::default();
        registry.register(
            "sources_hot",
            "Sources with a vehicle currently on the leaderboard",
            sources_hot.clone(),
        );
        let sources_loading = Gauge::default();
        registry.register(
            "sources_loading",
            "Sources currently downloading/importing their static GTFS",
            sources_loading.clone(),
        );
        let sources_static_loaded = Gauge::default();
        registry.register(
            "sources_static_loaded",
            "Feeds with their static schedule loaded (an open SQLite connection)",
            sources_static_loaded.clone(),
        );

        let vehicles = Gauge::default();
        registry.register(
            "vehicles",
            "Trip updates (vehicles) all feeds are publishing right now",
            vehicles.clone(),
        );
        let late_trips = Gauge::default();
        registry.register(
            "late_trips",
            "Trips currently ranked late across all feeds",
            late_trips.clone(),
        );
        let scheduled_trips = Gauge::default();
        registry.register(
            "scheduled_trips",
            "Trips in every agency's static schedule (fleet scale)",
            scheduled_trips.clone(),
        );
        let archive_trips = Gauge::default();
        registry.register(
            "archive_trips",
            "Trips held in the leaderboard archive (live plus finished)",
            archive_trips.clone(),
        );
        let archive_finished = Gauge::default();
        registry.register(
            "archive_finished_trips",
            "Archived trips that have finished running and are decaying out",
            archive_finished.clone(),
        );

        let leaderboard_entries = Gauge::default();
        registry.register(
            "leaderboard_entries",
            "Entries on the public leaderboard right now",
            leaderboard_entries.clone(),
        );
        let leaderboard_finished = Gauge::default();
        registry.register(
            "leaderboard_finished_entries",
            "Leaderboard entries whose trip has finished running",
            leaderboard_finished.clone(),
        );
        let leaderboard_top_delay = Gauge::default();
        registry.register_with_unit(
            "leaderboard_top_delay",
            "Peak delay of the #1 leaderboard entry",
            prometheus_client::registry::Unit::Seconds,
            leaderboard_top_delay.clone(),
        );
        let leaderboard_min_delay = Gauge::default();
        registry.register_with_unit(
            "leaderboard_min_delay",
            "Peak delay of the last leaderboard entry (the threshold to make the board)",
            prometheus_client::registry::Unit::Seconds,
            leaderboard_min_delay.clone(),
        );

        let healthy = Gauge::default();
        registry.register(
            "healthy",
            "1 when the poll loop is turning (the /healthz verdict), else 0",
            healthy.clone(),
        );
        let last_successful_poll = Gauge::default();
        registry.register_with_unit(
            "last_successful_poll_timestamp",
            "Unix time of the freshest successful poll anywhere (0 if none yet)",
            prometheus_client::registry::Unit::Seconds,
            last_successful_poll.clone(),
        );

        let ws_connections = Family::<StreamLabel, Gauge>::default();
        registry.register(
            "websocket_connections",
            "Live websocket subscribers, by stream",
            ws_connections.clone(),
        );
        for stream in WS_STREAMS {
            let _ = ws_connections.get_or_create(&StreamLabel { stream });
        }

        let process_rss = Gauge::default();
        registry.register_with_unit(
            "process_resident_memory",
            "Resident set size of the whole process (Linux only)",
            prometheus_client::registry::Unit::Bytes,
            process_rss.clone(),
        );
        let sqlite_bytes = Gauge::default();
        registry.register_with_unit(
            "sqlite_memory",
            "Heap SQLite is holding across all open connections",
            prometheus_client::registry::Unit::Bytes,
            sqlite_bytes.clone(),
        );
        let sqlite_peak = Gauge::default();
        registry.register_with_unit(
            "sqlite_memory_peak",
            "High-water mark of SQLite heap usage",
            prometheus_client::registry::Unit::Bytes,
            sqlite_peak.clone(),
        );

        // Constants for the life of the process.
        let build_info = Family::<VersionLabel, Gauge>::default();
        registry.register(
            "build_info",
            "Build information; value is always 1",
            build_info.clone(),
        );
        build_info
            .get_or_create(&VersionLabel {
                version: env!("CARGO_PKG_VERSION"),
            })
            .set(1);

        let debug_enabled: Gauge = Gauge::default();
        registry.register(
            "debug_enabled",
            "1 when debug capture (AMD_DEBUG) is on, else 0",
            debug_enabled.clone(),
        );
        debug_enabled.set(debug as i64);

        let start: Gauge = Gauge::default();
        registry.register_with_unit(
            "start_timestamp",
            "Unix time the process started (uptime = time() - this)",
            prometheus_client::registry::Unit::Seconds,
            start.clone(),
        );
        start.set(start_time as i64);

        Metrics {
            registry,
            polls,
            source_retirements,
            trips_vetted_out,
            trips_evicted,
            trips_finished,
            static_loads,
            static_census,
            http_requests,
            shape_requests,
            debug_captures,
            ws_opened,
            sources,
            sources_hot,
            sources_loading,
            sources_static_loaded,
            vehicles,
            late_trips,
            scheduled_trips,
            archive_trips,
            archive_finished,
            leaderboard_entries,
            leaderboard_finished,
            leaderboard_top_delay,
            leaderboard_min_delay,
            healthy,
            last_successful_poll,
            ws_connections,
            process_rss,
            sqlite_bytes,
            sqlite_peak,
        }
    }

    // ---- Counter hooks, called from the hot paths. ----

    /// Record a poll attempt. `success` folds into `amd_polls_total{result}`.
    pub fn record_poll(&self, success: bool) {
        let result = if success { "success" } else { "failure" };
        self.polls.get_or_create(&ResultLabel { result }).inc();
    }

    /// A source was retired after a fatal HTTP status.
    pub fn record_retirement(&self) {
        self.source_retirements.inc();
    }

    /// `n` late trips were refused by the provenance gate this poll.
    pub fn record_vetted_out(&self, n: u64) {
        if n > 0 {
            self.trips_vetted_out.inc_by(n);
        }
    }

    /// `n` archived trips were evicted as bad data for `reason`
    /// (`off_route`/`falsified`).
    pub fn record_evicted(&self, reason: &'static str, n: u64) {
        if n > 0 {
            self.trips_evicted
                .get_or_create(&ReasonLabel { reason })
                .inc_by(n);
        }
    }

    /// `n` trips were stamped finished for `reason` (`terminal`/`grace`).
    pub fn record_finished(&self, reason: &'static str, n: u64) {
        if n > 0 {
            self.trips_finished
                .get_or_create(&ReasonLabel { reason })
                .inc_by(n);
        }
    }

    /// A full static-GTFS load finished (`ok`/`error`).
    pub fn record_static_load(&self, ok: bool) {
        let result = if ok { "ok" } else { "error" };
        self.static_loads
            .get_or_create(&ResultLabel { result })
            .inc();
    }

    /// A background census/refresh count finished (`ok`/`error`).
    pub fn record_census(&self, ok: bool) {
        let result = if ok { "ok" } else { "error" };
        self.static_census
            .get_or_create(&ResultLabel { result })
            .inc();
    }

    /// An `/api/*` request was served. `endpoint` is one of [`HTTP_ENDPOINTS`].
    pub fn record_http(&self, endpoint: &'static str) {
        self.http_requests
            .get_or_create(&EndpointLabel { endpoint })
            .inc();
    }

    /// A route-shape lookup resolved (`hit` = a polyline was returned).
    pub fn record_shape(&self, hit: bool) {
        let result = if hit { "hit" } else { "miss" };
        self.shape_requests
            .get_or_create(&ResultLabel { result })
            .inc();
    }

    /// A debug-capture attempt finished (`ok`/`error`).
    pub fn record_debug_capture(&self, ok: bool) {
        let result = if ok { "ok" } else { "error" };
        self.debug_captures
            .get_or_create(&ResultLabel { result })
            .inc();
    }

    /// A websocket connection was opened on `stream` (`leaderboard`/`status`).
    pub fn record_ws_open(&self, stream: &'static str) {
        self.ws_opened.get_or_create(&StreamLabel { stream }).inc();
    }

    /// Write a fresh gauge snapshot into the handles, then render the whole registry
    /// as OpenMetrics text. Called once per scrape.
    pub fn render(&self, values: &GaugeValues) -> String {
        for (state, &count) in SOURCE_STATES.iter().zip(values.sources_by_state.iter()) {
            self.sources.get_or_create(&StateLabel { state }).set(count);
        }
        self.sources_hot.set(values.sources_hot);
        self.sources_loading.set(values.sources_loading);
        self.sources_static_loaded.set(values.sources_static_loaded);
        self.vehicles.set(values.vehicles);
        self.late_trips.set(values.late_trips);
        self.scheduled_trips.set(values.scheduled_trips);
        self.archive_trips.set(values.archive_trips);
        self.archive_finished.set(values.archive_finished);
        self.leaderboard_entries.set(values.leaderboard_entries);
        self.leaderboard_finished.set(values.leaderboard_finished);
        self.leaderboard_top_delay
            .set(values.leaderboard_top_delay_seconds);
        self.leaderboard_min_delay
            .set(values.leaderboard_min_delay_seconds);
        self.healthy.set(values.healthy);
        self.last_successful_poll
            .set(values.last_successful_poll_timestamp);
        for (stream, &count) in WS_STREAMS.iter().zip(values.ws_connections.iter()) {
            self.ws_connections
                .get_or_create(&StreamLabel { stream })
                .set(count);
        }
        self.process_rss.set(values.process_rss_bytes);
        self.sqlite_bytes.set(values.sqlite_bytes);
        self.sqlite_peak.set(values.sqlite_peak_bytes);

        let mut buffer = String::new();
        // Encoding into a String is infallible; the registry has no faulty encoders.
        encode(&mut buffer, &self.registry).expect("encoding metrics into a String cannot fail");
        buffer
    }
}
