# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Working agreement

**Never create pull requests on your own accord.** All work stays local — edit the
working tree, and commit only when asked. Opening a PR (or pushing a branch to set
one up) is the user's call, every time, not a way to wrap up a task.

## What this is

"America's Most Delayed" — a wall of shame for late public transit. A single Rust
binary crate (`server`, edition 2024) that continuously polls hundreds of
GTFS-realtime feeds, prints a live leaderboard of the most-delayed trips across
every agency it can reach, and serves that data over an HTTP + WebSocket API.

It ships as **two halves**: this crate (the dynamic API, on a VPS) and the pages in
**`../static/`** (on GitHub Pages, free). The server serves no HTML — see "The two
halves" below, and `../static/README.md` for deployment.

## Commands

- Build: `cargo build`
- Run: `cargo run` — long-running. It fetches the live feed catalogs (Transitland
  Atlas + MobilityData) over the network at startup, then polls live feeds forever
  (Ctrl-C to stop) while
  serving the API on `:8080`. A background census downloads every agency's static
  GTFS zip once into `./feeds/` (git-ignored) to count its trips, so the first run
  pulls a lot over the network; cached zips are reused until they age past 24h.
- Debug capture: `AMD_DEBUG=1 cargo run` adds a per-row 🐛 button to the
  leaderboard page that zips one entry's full data (config, live GTFS-RT, static
  GTFS, computed delay) into `./debug/` (git-ignored) for offline debugging. See
  the API/frontend section.
- Lint / format: `cargo clippy`, `cargo fmt`

- The pages: serve `../static/` with any static file server (`python3 -m http.server
  3000`). `config.js` points itself at `http://localhost:8080` when the page is
  served from localhost, so a local `cargo run` is all it needs.


Note when running under a wrapper: `cargo run` spawns the `server` binary as a
child, so a `timeout`/kill on `cargo` can orphan the running server. Run the
built binary directly (`./target/debug/server`) when you need a hard timeout.

## Architecture

The pipeline is: **catalog → agency configs → dynamic parallel polling →
realtime feeds → delay computation → provenance vetting → scoring + retention →
leaderboard → delta stream → API**.

The whole binary is **async** (Tokio, `#[tokio::main]`). `main.rs` awaits the
catalog fetch, then `scheduler::start` spawns the polling tasks and returns the
shared `Arc<Scheduler>`; `api::serve` runs the server on that handle, and the
process stays alive for as long as it serves.

### Agency configuration

`AgencyConfig` (in `agency.rs`) is the unit of "one feed we monitor" — a plain
struct with the slug, display name, static-GTFS URL, `GtfsRtUrls` (the
trip-updates and vehicle-positions feed URLs), and dedup metadata. A single
agency's data is assembled from multiple catalog rows, so each catalog provider
accumulates one up incrementally before handing over a complete config.

`catalogs/mobilitydata.rs` fetches MobilityData's `feeds_v2.csv` and folds its rows
into one `Build` per real agency, **keyed by the static feed's id**: a static row
keys on its own id, a realtime row on its `static_reference` (each row's scalar
fields fill the `Build` first-seen-wins; realtime URLs from every matching row
accumulate). A `Build` with both a static URL and a trip-updates URL becomes a
normal pollable config. One that has a static feed but **no paired realtime feed**
isn't dropped: if its catalog status is `active`, it's kept as a **static-only
config** (empty realtime URLs) so it can surface in `/status` as `no_realtime` —
this is how a large agency the catalog is missing GTFS-realtime for (e.g. TTC, CTA,
Muni, GO Transit — the reason NJ Transit is hand-configured) becomes visible rather
than silently absent. Deprecated/inactive/dev static feeds are dropped; only groups
with neither a realtime nor a static feed are truly skipped.

`catalogs/transitland.rs` is a second catalog source: the **Transitland Atlas**
(<https://github.com/transitland/transitland-atlas>), published as a GitHub repo
of **DMFR** JSON files. The provider downloads the repo zip, reads every
`feeds/*.dmfr.json`, and pairs feeds through **operators** rather than a
`static_reference` column — an operator's `associated_feeds` (explicit, or
implicit when the operator is nested inside a feed) point at a static
`static_current` feed and/or a realtime `realtime_trip_updates` feed. In two
passes it builds a **pollable** config when a static and a realtime feed are
present (deduped on the realtime URL so a regional feed shared by many operators is
polled once), then a **static-only** (`no_realtime`) config for operators that have
a static feed but no realtime — mirroring MobilityData, so a big agency Transitland
lacks realtime for still surfaces. A multi-modal operator is **`decompose`d into
one config per realtime mode** (subway / bus / rail…, keyed by `mode_key` on the
feed id) rather than collapsing to whichever feed is listed first: each mode group
takes the static whose id best matches that mode (`pick_static`), merges all its
trip-updates URLs (the scheduler polls the whole `Vec`), and is name/slug-suffixed
with a mode label so the same-agency dedup keeps the siblings apart (they'd
otherwise collapse — see the parenthetical rule in `main.rs`). This is how SEPTA
surfaces as *(bus)* + *(rail)* and MTA as *(subway)* + *(bus)*. A mode whose
realtime feeds are **all** auth-gated (MTA buses via BusTime) keeps those feeds and
becomes `requires_auth` rather than vanishing; a mode with any pollable feed drops
its auth-gated ones. Feeds flagged stale in
`tags.status` (outdated/archived/unpublished) are skipped. Country isn't in DMFR,
so `country_code` decodes the Onestop ID's geohash (`o-<geohash>-<name>`) with the
`geohash` crate and reverse-geocodes the point to an ISO country code with the
`reverse_geocoder` crate (offline, worldwide) — a general lat/lon→country helper,
not North-America-specific. The decoded point is also kept as the config's
`location` (a [`GeoPoint`] pair of degrees, with a haversine `distance_km`), used
for dedup; MobilityData fills `location` from each feed's bounding-box center.

`main.rs` draws from an ordered, editable `CATALOG_SOURCES` list of a
`CatalogSource` enum (currently `[Transitland, MobilityData]`) — reorder to change
which catalog wins duplicates, or delete a line to use just one. `collect_agencies`
tries `agency::nj_transit()` first (it's in no catalog, so it always wins), then
each source in order; a source that fails to load is logged and skipped, not fatal.
It **country-filters to North America first**, then dedupes across sources in two
passes:

- **Exact-match pass** — drop a config whose slug, static-feed URL, or realtime
  URL an earlier (more-preferred) one already claimed. Precise: shared URLs are
  definitely the same feed.
- **Same-agency pass** — collapse the *same real agency* listed under different
  names/ids/URLs across catalogs (e.g. "Valley Metro" and "Valley Metro (VM)").
  `Identity::same_agency` matches on: equal base name (parenthetical stripped) +
  country, **compatible parentheticals** (equal, or absent on one side), and
  **locations within `DEDUP_RADIUS_KM` (150km)**. The location check is what keeps
  genuinely distinct same-named agencies apart — two far-flung "Valley Transit"s,
  or BC Transit's regional systems (whose differing parentheticals also separate
  them). It's generic, not per-agency. Within a match it keeps the **most useful**
  feed (pollable > auth-gated > static-only), so a dedup never trades a live feed
  for a dead one. (A feed missing a `location` can't be matched here — ~13% of
  MobilityData feeds lack a bounding box — so a few cross-catalog dups survive.)

Feeds the scheduler won't poll — **auth-gated** (`requires_auth()`) and
**static-only** (`!has_trip_updates()`) — still pass through, so `/status` reports
them as `requires_auth` / `no_realtime`. The scheduler decides pollability per
source: only feeds with a trip-updates URL and no auth requirement enter the poll
rotation (see `SourceState` below).

New catalog sources should implement the `GtfsCatalogProvider` trait
(`catalogs/catalog.rs`) and be added as a `CatalogSource` variant in `main.rs`.

### Feed authentication (`auth.rs`)

A handful of agencies gate their realtime feeds behind an API key. `auth.rs` is the
one place that knows *how* those credentials are applied — but the **rules are data,
not code**: both the injection rules and the gated agency configs live in a
checked-in, secret-free **`auth.json`** (path overridable with `AMD_AUTH_FILE`).
Adding an authenticated agency is a JSON edit plus a secret in `keys.env` — no
recompile. A missing or malformed `auth.json` degrades gracefully (logged, treated
as empty) rather than aborting startup.

The **secrets stay out of the source tree** *and* out of `auth.json`: `FeedAuth::load`
reads them at startup from a **git-ignored `keys.env`** (`KEY=value` lines; path
overridable with `AMD_KEYS_FILE`), so the keys an operator is handed are dropped into
`keys.env` and never committed. `auth.json` references credentials only by **name**;
the value is looked up in `keys.env` at fetch/build time. A missing `keys.env` is
fine — the gated feeds are simply skipped.

The mechanism is **host-matched injection**, decoupled from the catalog:
`FeedAuth::apply(client, url)` builds a request for `url` and, for any rule in
`auth.json`'s `injections` whose host matches, injects the credential as either a
request **header** (`{"header": "apiKey"}`) or a **query parameter**
(`{"query": "key"}`). It's called on *every* outbound realtime fetch in
`realtime.rs`; a URL matching no rule — nearly all of them — passes through
untouched. **Path-embedded credentials** (TriMet's app id) need no bespoke Rust:
they use a `{KEY}` placeholder in the agency's URLs, which `FeedAuth::authed_agencies`
substitutes from `keys.env` at build time (an unresolved placeholder drops the
agency). To authenticate a new header/query feed: add its secret to `keys.env` and
one entry to `injections`.

The `Scheduler` holds the shared `Arc<FeedAuth>` and threads it into every
`realtime::fetch_feed`/`fetch_bytes` call. The gated agencies themselves are the
`agencies` array in `auth.json`, built by `FeedAuth::authed_agencies` (prepended
after NJ Transit so they win cross-catalog dedup over the catalogs' `requires_auth`
copies). Each entry with a `requires_key` is built only when that key is present:
STM and OC Transpo (header), MTA Bus Time and the Puget Sound OneBusAway server
(query — Puget Sound merges *all* of its per-agency feeds into one config), and
TriMet (path app id via `{TRIMET_APP_ID}`). The **MTA subway** feeds are listed with
no `requires_key` — they're *open* (no key) but the catalogs mislabel them
`no_realtime`, so they're always built. **MTA Bus's** one realtime feed (obanyc)
spans all five boroughs, but each borough publishes a separate static zip, so its
config lists the other four in `extra_static_urls` (merged into one schedule index —
see `gtfs.rs`), letting non-Manhattan trips resolve stop names and route shapes. The credential-name set for the startup
summary is derived from the config (injection keys + `requires_key`s + `{KEY}`
placeholders), not a hand-maintained list.

### The score, and why finished trips stay (`score.rs`)

The board is **not** a sort on `delay_seconds` any more. It ranks on a hidden
**score**, and a trip that has *finished running* keeps its place and decays off over
24 hours. Read `score.rs`'s module doc for the reasoning; the shape is:

```
score = minutes_late
      × severity   (1.0 … 1.8)   delay ÷ scheduled trip length, saturating at 150%
      × reach      (1.0 … 1.5)   stops still ahead, log-scaled, saturating at 30
      × confidence (0.6 … 1.0)   share of the delay that accrued under observation,
                                 plus how long we watched (full credit at 30m)
      × decay      (0.0 … 1.0)   1.0 while live; see below once finished
```

Three rules hold this together and are worth not breaking:

- **Every factor is a multiplier on minutes late, floored at (or near) 1.0.** That's
  what keeps the board recognisably a delay ranking: nothing here can promote a
  4-minute trip over a 90-minute one, the factors only reorder trips already in the
  same league.
- **An unknown input scores 1.0 — neutral, never a penalty.** A feed with no static
  schedule loaded, or one that publishes no stop sequence, must not be quietly
  demoted for what we don't know about it.
- **The score is hidden.** It isn't displayed, and outside debug mode it isn't even
  serialized. It's an ordering, not a statistic, and an unverifiable number is worse
  than useless on a page whose entire claim is that its delays are real. What the page
  shows is the delay, the provenance receipts, and how long ago a finished trip ended.

**Decay** is `exp(-age/4h) × (1 - age/24h)`, clamped to zero at 24h. Two curves,
because neither alone works: the exponential gives the steep early falloff (an
ordinary finished trip is out of contention within the hour) over a long tail, and
the linear taper forces it to actually reach **zero** — an exponential never does,
and "asymptotically small" is a different promise from "gone". Without the taper a
sufficiently absurd delay could homestead the wall forever and the board would become
a museum. Retention: ~75% at 1h, ~41% at 3h, ~17% at 6h, ~2.5% at 12h, nothing at 24h.
So a trip still there the next morning out-scored the live board by ~40× — which is
the point.

`peak_delay_seconds` (the worst vetted delay a trip reached), not the latest reading,
is what's scored and displayed. For a live trip the growth bound in `history.rs` makes
these nearly identical; it matters for a finished trip, whose final frame may be a
revised-down estimate that isn't what the run should be remembered for.

### The dynamic polling scheduler (`scheduler.rs`) — the core

This is where the "big picture" lives; understanding it requires reading
`scheduler.rs` together with `delay.rs`.

- **Each feed runs as its own async task** (`run_feed`), looping "poll, then
  sleep its current interval". There's no shared queue or worker pool; instead a
  `tokio::sync::Semaphore` of `MAX_CONCURRENT_POLLS` permits caps how many feed
  fetches are in flight at once (networking is the bottleneck). The CPU-bound
  decode + delay computation — plus the one-time static→SQLite import and the
  per-poll schedule queries — is handed to `spawn_blocking` so it never stalls the
  runtime.
- Each feed carries **its own poll interval**. A feed is polled every
  `BASE_INTERVAL` (20s) while one of its vehicles sits in the global top
  `LEADERBOARD_SIZE` (25); when it drops out, the interval doubles each miss up
  to `MAX_INTERVAL` (5 min); the moment it re-enters the top, it snaps back to
  base. This is the whole point of the design — quiet feeds cost almost no
  network.
- **`boards` is not the leaderboard.** `boards` holds each feed's *live* working set,
  replaced wholesale every poll; the board is built from **`archive`**
  (`HashMap<agency_idx, HashMap<trip_id, TripRecord>>`), which trips **outlive their
  own runs** in. This is the restructure the decaying score needed: the old design
  could only show what a feed was reporting right now, so the worst trip of the day
  vanished the instant it finally pulled in. Each poll folds its board into the
  archive (`record_board`, an upsert — a known trip keeps its identity, peak, birth
  and first sighting). A `TripRecord` **freezes** everything a finished entry will
  still need — position, provenance receipts, static span — because by the time it's
  rendered hours later the positions map has moved on and `TripHistory` has forgotten
  the trip. `sweep_archive` (on the 15s ticker) then ages it in three passes: *retire*
  a trip unmentioned for `TRIP_END_GRACE` (10 min, comfortably over `MAX_INTERVAL`)
  by stamping `ended_at` at its **last sighting** — not at when we noticed, so a slow
  feed doesn't buy its trips extra time on the wall; *expire* anything past 24h, where
  the score is already exactly zero; and *cap* the finished set to `ARCHIVE_CAP`
  (400), since the 24h horizon bounds memory only loosely.
- Trips leave the archive by three routes besides decaying out, and the distinctions
  are load-bearing: a trip the history **falsifies** mid-life is *evicted* (it would
  otherwise stop being refreshed, get stamped "finished", and spend a day decaying on
  a delay we now know was a stale label — this is why `TripHistory::vet` returns the
  refused **ids**, not just a count); an **off-route** vehicle is *evicted* for the
  same reason; but a vehicle **at a terminal** is stamped **finished on the spot**,
  because that is what an arrival looks like — freezing the last honest interior delay
  and starting its decay immediately rather than waiting out the grace period.
  `record_board` clears `ended_at` on any trip still being reported, so a feed that
  skipped a beat — or a vehicle that pulls away from a layover and is genuinely late
  again — gets its place back instead of decaying while still running.
- The **top-25 membership signal drives three decisions at once**: the fast poll
  interval, lazy static-GTFS loading, and fetching **live vehicle positions**.
  `on_leaderboard` answers "is this feed hot?" with an allocation-free O(n) count (how
  many records beat this feed's best), not a sort — it runs on every poll. It's keyed
  on **score, not delay** (the two no longer agree, and a short trip that scores its
  way onto the board must still earn the positions fetch that verifies it), and on
  this feed's best **live** trip only — a feed whose sole presence is a finished trip
  fading out has nothing worth polling fast for — while competing against *all*
  records, live and finished. `poll_once` therefore calls `record_board` **before**
  asking whether the feed is hot: a trip not yet in the archive can't be seen to be
  winning.
  When a feed is hot, `update_vehicle_positions` also fetches its GTFS-realtime
  `VehiclePositions` feed and stores per-`trip_id` coordinates (`positions`); the
  leaderboard snapshot joins those onto each entry's `latitude`/`longitude` so the
  frontend can map the delayed vehicle. Fetching only for hot feeds keeps this off
  the ~1200 cold feeds. (`(0,0)` "null island" fixes are dropped in `delay.rs`.)
  `update_vehicle_positions` also uses those coordinates to **drop two kinds of bad
  ranked trip from the board** (`drop_offroute_trips`), keeping the leaderboard to
  vehicles genuinely late *en route*: (1) **off-route** — a vehicle more than
  `OFF_ROUTE_KM` (2km) from its shape (`distance_to_path_km`, point-to-segment) is a
  mismatched trip/vehicle; (2) **at a terminal** — a vehicle within `TERMINAL_KM`
  (0.4km) of *either end* of its shape (its start or final terminal, i.e. the first
  or last shape point) is parked at a layover, not late en route, so its reported
  delay is spurious (a run that hasn't departed, or a finished run going stale — the
  same rationale as the schedule-based terminal-stop rule in `delay.rs`, but keyed on
  the vehicle's live *position* rather than its current stop's sequence, which catches
  a parked bus whose delay is read at an interior stop). A dropped trip is only held
  off the *live* board — its provenance history keeps accruing (the observation is
  vetted independently in `poll_once`), so once it pulls away from the terminal and is
  genuinely late it can rank on its watched record. In the archive the two cases
  diverge: off-route is evicted as bad data, at-terminal is stamped finished (see
  above). `poll_once` therefore recomputes
  hotness *after* fetching positions: a feed whose only delayed trips were off-route
  or terminal-parked drops out of the top N and backs off. A feed with trip updates but
  **no vehicle-positions feed** can't be verified this way, so it's excluded from
  polling entirely (`SourceState::NoVehiclePositions`) — only ~6% of feeds.
- A single `run_ticker` task renders the leaderboard every `PRINT_INTERVAL` (15s).
- The scheduler also tracks **per-source health** (`SourceRuntime`, parallel to
  `configs`): state, current interval, last poll outcome, live vehicle count, peak,
  hot flag, `late_trips` (how many delayed trips the last poll produced — a
  found-late count surfaced on the status page so a big agency stuck at 0 stands
  out as suspicious), `vetted_out` (how many late trips the delay history refused
  to vouch for on that poll — see below), and a transient `loading` flag (set only while the source is
  actively downloading + importing its static GTFS — a census count or a full
  load — so the status page can show that work in progress; orthogonal to
  `state`). `SourceState` is one of `Active` (in the poll rotation), `RequiresAuth`
  (auth-gated, never polled), `NoRealtime` (a static-only feed with no realtime to
  poll — see below), `NoVehiclePositions` (has trip updates but no vehicle-positions
  feed to verify routes against, so excluded), or `Failed(status)`. A poll that returns a `FATAL_STATUSES`
  code (401/404) retires the source: state → `Failed`, board cleared, and its task
  ends (never rescheduled). But a feed that merges **several trip-updates URLs**
  (Puget Sound polls one per OBA agency, MTA subway one per line) tolerates a
  per-URL failure: `fetch_delayed_trips` logs a failed sub-feed and presses on with
  whatever answered, propagating an error (and so risking retirement) only when
  *every* URL failed — otherwise one auth-gated OBA agency returning 401 would sink
  the whole merged source. Only `Active` sources are polled (`pollable()`).
  `status_report()` serializes all this (plus each source's `total_trips`) for
  `/status`, but **trims the `NoRealtime` feeds to the largest `NO_REALTIME_DISPLAY`
  (100) by `total_trips`** — so the status page highlights the biggest agencies
  we're missing realtime for, not every tiny static-only feed. `total_sources` in
  the summary reflects what's actually shown.
- A background **maintenance task** (`run_maintenance`, concurrency
  `STATIC_FETCH_CONCURRENCY`, separate from the poll limiter) does two jobs. Its
  first pass is a one-time **census** that gives *every* agency a `total_trips`
  scale metric by downloading its static zip once and counting distinct `trip_id`s
  (`gtfs::count_trips`) — cheap, retaining nothing in memory. This is also what
  sizes the `NoRealtime` feeds so `status_report` can rank them. Every later pass
  (`MAINTENANCE_INTERVAL`, 1h) re-fetches and re-counts only *polled* feeds whose
  cached zip has gone stale past `gtfs::STATIC_TTL` (24h), and drops their loaded
  parsed copy so the next hot poll reloads from the fresh zip — keeping static
  schedules from drifting out of sync with the realtime feeds. `NoRealtime` feeds
  are counted once and then never refreshed (there's no realtime to desync from).
  Staleness is judged by the cached file's **mtime**, so it holds across
  restarts/downtime; cache writes are atomic (temp file + rename) so concurrent
  fetches can't corrupt a zip.
- The ticker also pushes a fresh `LeaderboardSnapshot` to any connected websocket
  clients on the same `PRINT_INTERVAL` (15s) tick, via a `tokio::sync::broadcast`
  channel — so the websocket is throttled to one update every 15s no matter how
  often feeds poll (a new client also gets one snapshot immediately on connect).
  It's gated on `receiver_count() > 0`, so it costs nothing when nobody is
  subscribed.
- Shared mutable state (`boards`, `static_gtfs`, `status`) uses plain
  `std::sync::Mutex`, never held across an `.await` — so the per-feed task futures
  stay `Send` and the locks stay cheap.

Tuning constants (intervals, max concurrent polls, leaderboard size, cache dir)
are module constants at the top of `scheduler.rs`.

### The two halves: static (`../static/`) and dynamic (this crate)

The deployment is **split to save money**. The pages are static and go on **GitHub
Pages** (free); this server goes on a **VPS**, which bills **egress**. So the split
is drawn at exactly that line: anything that never changes is served by the free
half, and the paid half serves only what can't be precomputed — the live board,
source health, a route shape.

`../static/` (a sibling of `server/`, and the GitHub Pages root) holds
`index.html`, `status.html`, and `config.js` — the one file that changes at deploy
time, holding the API origin. There is **no build step** and nothing is
`include_str!`-baked into the binary any more: the server serves no HTML at all,
only `/api/*`, so `/` and `/status` are 404s. Because the pages come from a
different origin, `api.rs` mounts a `CorsLayer` (any origin — nothing we serve is
private) and a `CompressionLayer` (br/gzip). See `../static/README.md` for the
deploy steps; the API must be **https**, or the browser blocks the calls as mixed
content.

### Bytes on the wire (`wire.rs`) — read before adding an endpoint

**Egress is the bill.** The naive version of this API cost ~7.6 GB/day *per open
status tab*: the page re-polled a 176 KB report every 2 seconds. Nothing about that
report justified it — between two ticks, a handful of feeds had been polled and the
other ~495 were byte-identical.

So both live streams push **deltas**, at two levels: a row that didn't change isn't
sent, and a row that did carries only the **fields** that changed. The client holds
the last full state and merges (`{...old, ...new}`). `wire.rs` is the shared
machinery (`DeltaStream`), and its module doc is the protocol spec. The parts worth
knowing before you touch it:

- Every message carries a `seq`; a delta also carries the `base` seq it was diffed
  against. **No `base` means a full.** The client's rule: `seq <= mine` → ignore;
  `base > mine` → a tick was missed, resync from a full; else merge. This is what
  makes the connect race benign — a full fetched over HTTP can legitimately be
  *newer* than the first delta that arrives on the socket.
- **One delta serves every client**, because they all received the same previous
  tick. That's what keeps the fan-out a single `broadcast` rather than a
  per-connection diff.
- The tick advances **whether or not anyone is listening**. The stream's retained
  state is what the *next* client is served as its full, so skipping the work when
  idle would just hand the next visitor a stale board.
- A field that changes **to null must travel as an explicit `null`**, or the merge
  keeps the stale value (a `last_error` that cleared, a vehicle that lost its GPS
  fix). Nulls are only stripped from rows the client has never seen.
- `SourceStatus.last_poll` is a **unix timestamp, not an age** — this is load-bearing.
  An age changes every tick for every source, so no row would ever be unchanged and
  the whole delta scheme would collapse back into re-sending the report. The page
  subtracts it from the message's `generated_at`.

Measured, at 504 sources: status **7.6 GB/day → ~105 MB/day** per viewer (and that
was during warmup, when every feed still polls at the 20s base interval — it settles
lower as feeds back off); leaderboard **52 → 22 MB/day**; a route shape
**27 KB → 2.4 KB**.

### Prometheus metrics (`metrics.rs`)

`GET /metrics` exposes the whole pipeline in **OpenMetrics** text via the **official
Prometheus Rust client** (`prometheus-client`). This is instrumentation for the
**operator's own** Prometheus, scraped server-side — *not* the public frontend API —
so the egress discipline the rest of the crate lives by (`wire.rs`) doesn't bind it:
one scrape every 15–60s from one Prometheus is not 500 browser tabs, which is what
frees it to be comprehensive.

If `METRICS_BEARER_AUTH` is set, `/metrics` requires an `Authorization` header
whose token matches that env var; when unset/empty, `/metrics` remains open.

The metrics split into the two kinds that shape the module:

- **Counters** — cumulative process-lifetime events, incremented at the point they
  happen from the scheduler's hot paths and the API handlers: `amd_polls_total`
  (by `result`), `amd_source_retirements_total`, `amd_trips_vetted_out_total`,
  `amd_trips_evicted_total` / `amd_trips_finished_total` (by `reason` —
  off_route/falsified, terminal/grace), `amd_static_loads_total` /
  `amd_static_census_total`, `amd_http_requests_total` (by `endpoint`),
  `amd_shape_requests_total`, `amd_debug_captures_total`,
  `amd_websocket_connections_opened_total`. **Every label combination is
  materialized at startup** so a series reads `0` from the first scrape rather than
  popping into existence on its first event.
- **Gauges** — the current state of the world, which can't be "incremented": sources
  by state (`amd_sources{state}`), hot/loading/static-loaded counts, `amd_vehicles`,
  `amd_late_trips`, `amd_scheduled_trips`, archive size, the leaderboard's top and
  threshold delay, live websocket subscribers, and the memory figures
  (`amd_process_resident_memory_bytes`, `amd_sqlite_memory_bytes`). These are **not**
  mirror-updated on every state change — `Scheduler::gauge_values` takes one cheap
  snapshot of the shared state at **scrape time**, `Metrics::render` writes it into
  the gauge handles, and the registry is encoded. A gauge is therefore always at most
  one scrape stale and never drifts. `gauge_values` counts sources directly rather
  than reusing `status_report`, whose `no_realtime` trim would undercount them.

Following the same **"expose the timestamp, derive the age"** rule the wire protocol
uses (an age would change every scrape, defeating nothing here but staying honest),
health is published as `amd_last_successful_poll_timestamp_seconds` and uptime via
`amd_start_timestamp_seconds` — Prometheus derives freshness/uptime with `time() -`
the timestamp. `Metrics` lives behind an `Arc` on the `Scheduler`; its counter/gauge
handles are internally reference-counted, so the hot paths and handlers hold cheap
clones writing the same atomics. Adding a metric is a handle on the struct, a
`register` call in `Metrics::new`, and either a `record_*` call at the event
(counter) or a field on `GaugeValues` filled in `gauge_values` (gauge).

### The API layer + frontend (`api.rs`, `../static/`)

A thin axum server over the shared `Arc<Scheduler>`. No pages, five data endpoints
(plus `/healthz` and `/metrics` for ops — see the metrics section):

- `GET /api/status` — the **full** `StatusReport` (per-source health: fetch
  frequency, success/failure, vehicles now, `total_trips` scale metric,
  `requires_auth` / `no_realtime` / `no_vehicle_positions` / `failed` state, plus an
  aggregate `summary`) with the `seq` a client needs before deltas mean anything.
  The `no_realtime` lines are the 100 biggest agencies the catalog lacks realtime
  for. **Fetched once per page load, not polled.** It's ~140 KB, which is precisely
  why it's on HTTP — the compression layer takes it to ~17 KB, and a websocket frame
  gets no compression.
- `WS /api/status/live` — source-health deltas, one every `STATUS_INTERVAL` (2s).
  Sends nothing on connect; the page already has its full.
- `WS /api/subscribe` — the leaderboard: a full board on connect, then a delta every
  `PRINT_INTERVAL` (15s). A client that lags past the broadcast buffer is
  **disconnected** rather than skipped — skipping a delta would leave its merged
  copy silently holding stale fields — and reconnects into a fresh full.
- `GET /api/shape/{slug}/{trip_id}` — one trip's route path as a **Google encoded
  polyline** (`wire::encode_polyline`), not an array of coordinate pairs: a shape is
  hundreds-to-thousands of points, and delta-encoding consecutive ones costs ~2 chars
  apiece instead of ~11 (measured 8.9× smaller raw, still 3× after gzip). Cached by
  the browser for a day — but **never when empty**, since "static isn't loaded yet"
  is a passing state and caching it would leave the map blank long after the shape
  exists.
- `POST /api/debug/capture` — **debug mode only** (see below). Body
  `{slug, trip_id, message}`; zips up everything behind one leaderboard entry into
  `./debug/` and returns `{ok, path, error}`. Errors (debug off, unknown slug)
  come back in the JSON body, not as an HTTP error, so the page shows them inline.
- `GET /metrics` — the Prometheus/OpenMetrics scrape (`metrics.rs`). Counters are
  read from the registry as-is; the gauges are snapshotted from the scheduler's live
  state at scrape time. Also `GET /healthz` — 200/503 liveness from
  `Scheduler::health` (the poll loop is turning), cheap enough to poll often.

The two HTML pages are plain vanilla-JS, no build step. The leaderboard page renders
the merged board as three stacked sections — the **#1 row**, a **Leaflet map** of one
delayed vehicle (using the snapshot's `latitude`/`longitude`), then the
**#2–25 rows**. **Up/down buttons** above the map step a *selected index* through
the leaderboard, so the map + detail line can show any ranked trip, not just #1
(the selection is clamped to the current board and persists across the 15s
pushes). The map re-centers its single marker on each tick and draws the
selected vehicle's **route line**, fetched on demand from
`GET /api/shape/{slug}/{trip_id}` → `Gtfs::trip_shape`, which returns the trip's
**own** `shape_id` (the accurate path for that run), falling back to the
**canonical** shape for its route + direction only when the trip has no shape of
its own. Each row's **Watched** column (and the map caption) shows the entry's
provenance — how long we've tracked the trip and how much of its lateness it picked
up *while we watched* — from the snapshot's `tracked_seconds` / `birth_delay_seconds`
(see the delay-provenance section). These are receipts, not a caveat: everything on
the board has already passed the vetting gate.

A row whose trip has **finished running** carries `ended_at` and renders dimmed and
italic (`tr.finished`), with "finished 14m ago" in place of a next stop and the map
caption saying "last seen at" rather than "vehicle at" — its position is frozen at
the final sighting, not where the bus is now. `ended_at` is a **unix timestamp, not
an age**, for exactly the reason `SourceStatus.last_poll` is (see `wire.rs`); the page
derives the age against the message's own `generated_at`, which also keeps a skewed
browser clock from rendering "finished -3m ago". The **Late** column shows
`peak_delay_seconds`, so the number on screen is the one the row was ranked on.

**Debug mode** (`AMD_DEBUG` env var — any value but empty/`0`/`false`/`no`; a
runtime flag, not a build flag, so it costs a single bool check when off and no
work until a capture is triggered) surfaces two per-row columns on the leaderboard
page: 🧮 **score** and 🐛 **capture**.

🧮 opens a dialog showing how the row's rank was computed — every factor with its
value and the inputs that produced it (`score_breakdown`, built by
`ScoreInputs::breakdown`). It's the audit trail for a ranking that is otherwise
deliberately hidden. The field is `skip_serializing_if = "Option::is_none"` and only
populated when debug is on, which is **not** cosmetic: it carries the decay factor,
which changes every tick for every finished row, so shipping it in production would
drag the whole board onto the wire each tick and undo `wire.rs` (measured: 25.7 KB
per delta with it, 4.0 KB without). The snapshot carries `debug_enabled` so the frontend reveals the
column (CSS `body.debug-on`). Clicking prompts for a free-text note and POSTs to
`/api/debug/capture`; `Scheduler::capture_debug` **over-collects** (deliberately —
this is a developer tool, never user-facing) into a zip: the agency config +
per-source health, the **live re-fetched** trip-updates and vehicle-positions
feeds (raw `.pb` bytes *and* a decoded pretty-print, plus the just-this-trip
subset), the recomputed `DelayedTrip` + leaderboard entry, the archive record behind
the row (`leaderboard_record` — its lifecycle timestamps, cached static span, and the
full score arithmetic, which is the first thing to look at when the question is "why
is this ranked here" or "why is this still on the board"), the trip's static
schedule rows (`Gtfs::debug_dump`), and a verbatim copy of the cached static GTFS
zip **and** SQLite index. The realtime feeds are re-fetched *at capture time* so
the archive reflects the feed state when the anomaly is visible, not whenever the
report is later opened. Archives can be large (hundreds of MB for a big agency,
since the zip + sqlite are both included and the decoded dump is verbose); `./debug/`
is git-ignored.

The status page fetches `/api/status` once and then follows `/api/status/live`, and
shows both a grid of square LEDs (color = state — with a pulsing cyan taking
precedence while a source is downloading/importing its static GTFS — ring =
on-leaderboard, one-shot blink = freshly polled, custom hover tooltip) and a
**sortable table** of the same data (click a column header; only the status cell is
tinted). The LED grid is kept in the **same order as the table**, so re-sorting the
table re-sorts the dots. Three of its columns (`status`, `age`, `hot`) are *derived*
client-side rather than sent — `age` most of all, see the `wire.rs` section. Edit
the pages under `../static/`; there's nothing to rebuild. Note: the leaderboard map
is the one place we load **external resources** (Leaflet + OpenStreetMap tiles from
a CDN — someone else's bandwidth, deliberately); the status page stays
self-contained.

The serializable public types (`LeaderboardSnapshot`, `StatusReport`, etc.) live
in `scheduler.rs` so the scheduler stays the single source of truth; `api.rs` only
does HTTP/WS plumbing, and `wire.rs` owns the delta format.

### Delay provenance (`history.rs`) — why the top entries are real

Read this before touching `delay.rs`. Every fake that has ever topped the board has
one shape: **the feed hands us a `trip_id` that no longer describes the run the
vehicle is driving.** MARTA's AVL finishes a bus's 10:22 run, sends it back out on
its 12:22 run, and keeps labelling it `11012496`; we compare a 12:40 bus against a
10:22 timetable and get a fake two hours. LADOT does the same via a stale block
assignment. In a *single frame* these are indistinguishable from a genuinely late
bus — predictions self-consistent, vehicle on its route, stop interior. And
"re-match the vehicle to its best-fitting scheduled trip" (what Transit appears to
do) is **worse than useless here**: it computes delay *modulo the headway*, so a bus
one headway late reports ~0 — it destroys precisely the large delays this project
exists to find. Don't reintroduce it.

Across *time* they separate trivially, because delay obeys a physical bound: **a
trip's delay can grow no faster than the clock.** A bus stuck motionless accumulates
one second of lateness per second; nothing accumulates more. So `TripHistory::vet`
(called on every poll, per feed) keeps a `TripTrack` per live `trip_id` and applies
three rules:

1. **Birth** — a trip first seen more than `CREDIBLE_BIRTH_DELAY` (10m) late is never
   credited. We have no evidence its delay is real rather than a stale label; and a
   run that never departed is a *cancellation* to the rider, not a two-hour ride.
2. **Growth** — delay may exceed neither `last_delay + elapsed` nor
   `birth_delay + age`, each plus `JUMP_SLACK` (15m, since a prediction may be
   revised in one step). The first catches a label flipping mid-run (MARTA jumps
   +121m between two polls ≤5m apart — not implausible but *impossible*); the second
   stops per-poll slack compounding into a large fake.
3. **Direction** — a trip's current stop never moves meaningfully backwards through
   its own sequence (`SEQ_TOLERANCE`, 3). A bus does not un-drive its route; MARTA's
   jumps from stop 68 to stop 2.

A violation is **sticky** — after a label goes stale the delays it reports are steady
and self-consistent, so re-testing each poll would let the fake straight back in. A
trip absent for `ABSENCE_RESET` (20m) is instead *forgotten* and, on return, must be
born credible again — which is exactly how LADOT's bus, reappearing mislabelled
mid-way through a later run, gets refused. Forgetting is also what bounds the memory
(only trips seen in the last 20 minutes are held).

This is why `delay::delayed_trips` returns a `FeedDelays` — the late `trips`, *and* a
`TripObservation` for **every** trip it could time, late or not. The on-time sightings
are the whole point: they're the evidence that lets the same trip be believed when it
later turns up an hour down. Keep observations cheap (no label lookups); only the
rankable trips get `describe`d.

Two accepted costs, both deliberate: after a restart nothing has history, so the board
fills over the first several minutes (capped near `CREDIBLE_BIRTH_DELAY` at first)
rather than instantly; and a genuinely late-*starting* run never scores. Each ranked
entry carries its receipts (`tracked_seconds`, `birth_delay_seconds`) into the
snapshot, and the leaderboard's **Watched** column shows them; `/status` shows
`vetted_out` per source.

### Realtime, static, and delay computation

- `realtime.rs`: async fetch + protobuf-decode of a GTFS-realtime feed
  (`fetch_feed` — used for both TripUpdates and VehiclePositions).
- `gtfs.rs`: async-downloads (and disk-caches to `./feeds/<slug>.zip`) the
  **static** GTFS schedule, then — on the blocking pool (`spawn_blocking`) — imports
  it **once** into a per-feed **SQLite** database (`./feeds/<slug>.sqlite`, via
  `rusqlite` with the `bundled` feature). Every schedule query (route/stop names,
  scheduled arrival times, trip rows, route shapes) is then an **indexed lookup
  straight off disk** through a read-only `Connection` (wrapped in a `Mutex` so
  `Arc<Gtfs>` stays `Send + Sync` for the blocking pool; each loaded feed has its own
  connection, so they never contend). The point is memory: the big tables
  (`stop_times`, `shapes` — millions of rows on large feeds) **never live on the
  heap**; SQLite pages them in on demand under a small `cache_size` pragma, so a
  loaded feed's resident footprint is flat and bounded. We trade a little disk (the
  `.sqlite` sidecar) for that.

  **Memory tuning — read before changing it, it's all measured.** The one term that
  scaled with feeds *loaded* was the per-connection page cache: at 2 MiB each it
  measured **~2.8 MB of heap per loaded feed**, making SQLite ~32% of RSS at 141
  feeds and heading for ~800 MB across every feed. Three levers, all now in
  `gtfs.rs`: `PAGE_CACHE_KIB` (256 KiB per connection, down from 2 MiB — the pages
  stay hot in the *kernel's* page cache, which is shared, evictable, and not charged
  to our RSS, so this mostly moves caching somewhere strictly better);
  `SQLITE_HEAP_LIMIT` (a **global** `sqlite3_soft_heap_limit64` backstop, so the sum
  across all connections is bounded directly rather than trusting N per-connection
  budgets to add up); and `MMAP_BYTES`, which is **deliberately 0** — see its doc
  comment, mmap charged 657 MB of mapped pages to our RSS and the kernel caches those
  file pages anyway. Net at 141 feeds: **RSS 568→395 MB, SQLite heap 181→28 MB**,
  and SQLite no longer grows with feeds loaded. `/status`'s summary reports
  `sqlite_bytes` / `sqlite_peak_bytes` / `process_rss_bytes` so this stays checkable
  instead of guessed at.

  **The allocator gives memory back — glibc didn't.** The heap above is bounded, but
  RSS wasn't: a cold start (download + import hundreds of feeds) sat at ~500 MB
  indefinitely while a warm restart (caches on disk, imports skipped) sat at ~200 MB,
  both flat for hours. That gap is *retained peak transient allocation*, not a leak —
  glibc's malloc scatters freed buffers across up to 8×ncpus arenas (128 on a 16-core
  box) and effectively never `madvise`s them back, so the process homesteads its
  cold-start high-water mark, and every 24h maintenance re-import ratchets it back up.
  The fix is the global allocator: `main.rs` sets **jemalloc** (`tikv-jemallocator`)
  with a compiled-in `_rjem_malloc_conf` of `background_thread:true` +
  `dirty_decay_ms`/`muzzy_decay_ms` of 5s, so a background thread purges dirty pages to
  the OS seconds after a burst frees them. This is what makes RSS *fall back* after
  warmup/maintenance instead of pinning at the peak — the property an indefinitely
  running process needs. (The symbol is `_rjem_malloc_conf`, not `malloc_conf`: tikv
  prefixes every jemalloc symbol with `_rjem_`; the env override is `_RJEM_MALLOC_CONF`.
  `#[used]` keeps the static from being stripped before it can override jemalloc's weak
  default.)

  Things that sound good here and **aren't**: consolidating the per-feed databases
  into one file (either a table per feed, or one table with a `dataset_id`) — a
  per-feed table explodes the schema every connection must parse, and a shared table
  makes `stop_times` tens of millions of rows (deeper B-trees ⇒ *more* pages touched
  per lookup), serializes all imports behind one writer, turns a feed refresh into a
  multi-million-row `DELETE` needing a multi-GB `VACUUM`, and puts every agency in one
  blast radius — to optimize a term that is now ~7% of RSS. `WAL` buys nothing (after
  import we are strictly read-only, and WAL exists to let readers run alongside a
  writer). `VACUUM` is disk, not RSS, and the DBs are bulk-loaded once into a fresh
  file, so they're already compact.

  `Gtfs` itself holds only the connection + the
  agency timezone (resolved once at load). Import streams each CSV member row-by-row
  into prepared inserts (never collecting a whole table), with secondary indexes on
  `stop_times`/`shapes` built after the bulk load; the `time` column bakes in
  arrival-else-departure. The `.sqlite` is **derived from the cached zip** and
  rebuilt only when the zip is newer (mtime check) or the db is missing — so the
  zip download/refresh/census path is untouched, and a maintenance refresh (which
  re-downloads a newer zip) transparently triggers a rebuild on the next load. The
  zip disk cache is mtime-TTL'd (`STATIC_TTL`, 24h) and written atomically.

  **A 200 OK is not a GTFS zip** (`looks_like_zip`, checked before anything is
  cached). Plenty of agencies answer a zip request with `200` carrying HTML (a login
  page, a CDN 404, a "lander" redirect), JSON, an empty body, or a plain-text error —
  the Availtec/InfoPoint stack behind The Rapid and TARC literally returns *"Failed
  response to GTFS-Zip request: Reason=The process cannot access the file … because it
  is being used by another process"*. `error_for_status()` waves all of that through.
  Writing it to `<slug>.zip` **poisons the cache**: because freshness is judged by
  mtime, the junk is trusted for a full 24h, so a momentary file-lock upstream takes
  the agency dark for a day. (15 of ~200 cached zips were poisoned this way when the
  check was added.) So: verify the ZIP signature before caching and `bail!` with a
  `body_preview` of what the server actually said — nothing is written, and the next
  pass retries — and verify it again on the way *out* of the cache, discarding a
  poisoned entry so an already-corrupted cache self-heals rather than staying dark.
  Don't "simplify" either check away; a truthful HTTP status is not something these
  feeds reliably provide.

  `trip_span` is one indexed pass giving a trip's scheduled running time, stop count,
  and stops still ahead of a given sequence — the scale `score.rs` judges a delay
  against. It's static data, so the scheduler looks it up **once per trip, ever** and
  caches it on the `TripRecord` (`span_checked` distinguishes "the agency's static
  isn't loaded yet, retry" from "the schedule doesn't know this trip", so we neither
  retry forever nor give up before the static arrives).
  `count_trips` counts a schedule's trips straight from the zip without building the
  index (for the census). `is_stale` is the shared freshness check. A static URL
  with a `#inner.zip` fragment (a GTFS zip nested inside another zip — e.g. SEPTA's
  `gtfs_public.zip#google_bus.zip`) is **unwrapped at download time** so the cache
  always holds a flat GTFS zip. `Gtfs::load` takes a **slice of static URLs** (almost
  always one): a feed with `extra_static_urls` (MTA Bus's five boroughs) downloads
  each to its own cache file (`<slug>.zip`, `<slug>.staticN.zip`) and **merges them
  all into one SQLite index** in a single import transaction — the ids across those
  zips are disjoint, so nothing collides. The index rebuilds when *any* backing zip
  is newer, and maintenance treats the feed as stale if any one has aged out;
  `count_trips` sums the per-zip counts. This is the general fix for one realtime
  stream backed by several separately-published static zips.
- `delay.rs`: turns a realtime feed into a `FeedDelays` — the late `DelayedTrip`s
  (fully labelled, leaderboard candidates) **plus a `TripObservation` for every trip
  it could time at all**, late or not, which is what `history.rs` needs (see above;
  the vetting gate is what actually decides which of these reach the board). Trips
  are `measure_delay`d cheaply and only the rankable ones are `describe`d, so the
  label lookups stay off the ~40k vehicles we see per cycle. **Static GTFS is
  optional here** — `delayed_trips` takes `Option<&Gtfs>`. Delay is derived by
  priority: (1) `TripUpdate.delay`, (2) `StopTimeEvent.delay`, (3)
  predicted-vs-static-schedule comparison. Only (3) needs the static feed, so a
  leaderboard can be built from realtime signals alone. This is why the scheduler
  can defer static loading until a feed is actually interesting: static feeds are
  large, and eagerly parsing hundreds of them would blow up memory and startup.
  The delay is read at the trip's **`current_stop`** — the stop whose predicted
  time is closest to *now*, i.e. where the vehicle physically is — not the next
  future stop, because some feeds emit corrupt *downstream* predictions (a stop
  flung hours out of position with a matching multi-hour delay) while the
  just-reached stop still reads correctly (this is what pinned King County Metro
  at a fake ~4h). The **next** stop is still what we *display* (`upcoming_stop`).
  `current_stop` also drops **stale "ghost" trips**: if even the nearest predicted
  stop is more than `STALE_PREDICTION_SECS` (1h) from now, the trip is a completed
  run the feed never expired (many feeds leave these in TripUpdates with all stops
  hours in the past) — its bogus timestamps would otherwise schedule-compare into
  huge fake delays (this is what put Santa Maria Area Transit's ghosts at a fake
  ~8h #1). A genuinely late bus, however late, is still at some stop *now*, so this
  never suppresses a real delay. Trips late by ≤0 are dropped, as are over-long
  ones — but with **two ceilings**: agency-*reported* delays (trip/stop-level) get
  the generous `MAX_PLAUSIBLE_DELAY` (8h), while *inferred* schedule-comparison
  delays get the tighter `MAX_INFERRED_DELAY` (3h), since a realtime `trip_id` that
  maps to a different scheduled trip (id/block reuse) or a service-date edge makes
  an on-time bus look uniformly hours late — a fake the ghost check can't catch
  (a stop still sits near now) but that a real agency would have reported directly.
  A delay read at either of the trip's **terminal stops** — origin or destination —
  is dropped outright, **whatever its source** (`at_terminal_stop` /
  `Gtfs::terminal_stops`, checked in `measure_delay` before any delay is derived).
  A vehicle parked at an endpoint is not a delayed bus: at the origin it simply never
  departed (a *cancellation* to the rider, not an hours-long ride), and at the
  destination it's a finished run whose timestamps are going stale. Both sit still
  while their reported delay climbs at a **full second per second** — the fastest any
  delay can grow — so they *out-grow every real bus and win the leaderboard*: with
  the provenance gate in place but this check still limited to inferred delays, the
  board's top was Cleveland parked at stop 1 of 98 and NJ Transit at stop 51 of 52,
  each reporting a `stop-level`/`trip-level` delay that sailed past a guard that only
  ran for `vs-schedule`. (It also pinned MARTA at a fake ~2h that the 3h ceiling let
  through.) A genuinely late bus is underway at an *interior* stop, so refusing the
  endpoints costs no real delay. Duplicate trips are also
  collapsed: some feeds emit the same `trip_id` as several entities in one message
  (OCTA repeats a trip up to 3×), so `delayed_trips` keeps one `DelayedTrip` per
  trip id (the largest delay) rather than showing duplicate leaderboard rows.
  `delay.rs` also exposes
  `vehicle_positions` (a `FeedMessage` → per-`trip_id` coordinates map), used to
  place hot feeds' delayed vehicles on the map, and `needs_static_schedule` (see
  the scheduler's static-load bootstrap below).

## Gotchas

- **Adding a field to a live payload costs egress on every viewer, forever.** The
  streams are deltas (`wire.rs`), so a field that changes *rarely* is nearly free
  and a field that changes *every tick* is expensive — it drags its whole row onto
  the wire each time. A field that changes every tick **for every row** (an age, a
  countdown, a "seconds since") is the pathological case: it defeats the row-level
  delta entirely and puts the full report back on the wire. Send the underlying
  timestamp and derive it in the page, as `last_poll` does. Same rule for a new
  endpoint: if a page would poll it on a timer, it should probably be a delta stream
  instead.
- **The leaderboard is cold after a restart, and that's correct.** Delay provenance
  (`history.rs`) only credits lateness it *watched accumulate*, and a fresh process
  has watched nothing — so for the first minutes the board is capped near
  `CREDIBLE_BIRTH_DELAY` (10m) and every entry reads "born ~9m late, watched ~1m".
  It climbs as trips are picked up on time and get worse under observation; a 60m
  entry needs ~45m of continuous observation of that trip. Don't "fix" this by
  loosening the birth rule — a board that fills instantly after a restart is a board
  that trusts delays it has no evidence for, which is precisely how the MARTA and
  LADOT fakes got to #1. If you need a populated board immediately (a demo), let it
  warm up rather than raising the constants. The 24h archive is **in-memory only** and
  dies with the process, so a restart also discards every finished trip — the decaying
  tail rebuilds over the following day. Persisting it is a real feature, not a bug
  fix, and would need the provenance receipts persisted alongside or the reloaded
  entries would be unauditable.
- **A field that changes every tick is still the pathological case, and the score is
  one.** It decays continuously for every finished row, so it is deliberately never
  serialized in production — the board conveys its ranking through array order and
  `rank`, not a number. The same reasoning forbids sending an "age since ended", a
  "time left before it decays off", or a live score to the page: send the timestamp
  and derive the rest client-side. If you add a factor to the score, putting it in the
  *payload* is a separate decision with a recurring egress cost.
- The ceilings in `delay.rs` (`MAX_PLAUSIBLE_DELAY`, `MAX_INFERRED_DELAY`), the
  terminal-stop rule, and the `STALE_PREDICTION_SECS` ghost check are now **backstops
  behind** the provenance gate, not the primary defense — they were each added to
  catch a specific fake from a single frame. They could probably be relaxed now, but
  they're cheap and independent, so they stay.
- Delay computation degrades gracefully without static GTFS, but a feed that
  *only* exposes schedule-comparison delays (neither delay field populated) reports
  nothing until its static feed loads — and static normally only loads once a feed
  reaches the top 25, which such a feed can never do without delays. The scheduler
  breaks this chicken-and-egg with a **one-shot eager static load**: when a poll
  yields zero delays and `delay::needs_static_schedule` says the feed is
  times-only, `poll_once` calls `ensure_static_loaded` so the *next* poll can
  schedule-compare (this is how MBTA, TTC, Capital Metro, VIA Metro… surface at
  all). Reaching the top 25 is therefore no longer a prerequisite for these
  agencies — but it does mean big time-only feeds each build their SQLite index
  early, so warmup does more one-time import work (and writes more `.sqlite` to
  disk) up front. That's disk and CPU, not resident memory: the imported schedule
  lives on disk and is queried, never held in RAM (see `gtfs.rs`).
- Dropping the country filter in `main.rs` is why ~600 agencies are monitored
  rather than only US ones; scope the filter there if that changes.
- The shared reqwest client sends a real `User-Agent` (`USER_AGENT` in
  `scheduler.rs`). Some hosts (e.g. viainfo.net, serving VIA Metropolitan's static
  GTFS) return **403 Forbidden** to a client with no UA, which silently starved
  those agencies of a `total_trips` census. Keep a UA set on any new outbound
  client.
