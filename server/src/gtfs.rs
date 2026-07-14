//! Static (non-realtime) GTFS: downloading, indexing, and querying the schedule.
//!
//! A downloaded GTFS zip is imported **once** into a per-feed SQLite database
//! (`./feeds/<slug>.sqlite`) holding the handful of columns the leaderboard needs
//! (agency, routes, stops, trips, stop_times, shapes). Every later query — a
//! stop name, a scheduled arrival, a route shape — is answered by an indexed
//! lookup straight off disk, so the big tables (`stop_times`, `shapes`, together
//! millions of rows on large feeds) **never live on the heap**. Only a read-only
//! connection with a small page cache stays resident per loaded feed; SQLite pages
//! the data in on demand and evicts it, which is the whole point — we trade a bit
//! of disk (the `.sqlite` sidecar) for a flat, bounded memory footprint.
//!
//! The import is derived from the cached zip and rebuilt only when the zip is
//! newer (or the db is missing), so the existing zip download/refresh/census path
//! is untouched. The realtime side lives in [`crate::realtime`] and
//! [`crate::delay`].

use anyhow::{Context, Result};
use chrono::{NaiveDate, TimeZone};
use chrono_tz::Tz;
use reqwest::Client;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::collections::HashMap;
use std::io::{BufReader, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// How long a cached static GTFS zip is treated as fresh before it's re-fetched
/// to stay in sync with the realtime feed. Freshness is judged by the file's
/// mtime, so this holds across restarts and downtime — a cache left sitting for
/// longer than this is re-downloaded on first use, even if nothing ran meanwhile.
pub const STATIC_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Monotonic counter giving each in-flight download/import a unique temp filename,
/// so concurrent writes of the same feed can't collide while building the cache.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Per-connection page cache, in KiB (negative = KiB rather than pages).
///
/// Every loaded feed holds its own read-only connection, so this multiplies by the
/// number of feeds loaded — it is the one term in our memory use that grows without
/// bound as more agencies go hot. It was 2 MiB, which measured at ~2.8 MB of heap
/// per loaded feed and made SQLite a third of RSS at 141 feeds (heading for ~800 MB
/// across every feed). Shrinking it doesn't really *lose* the cache: the pages stay
/// hot in the **kernel's** page cache, which is shared across all feeds, evictable
/// under pressure, and not charged to our RSS — so this mostly moves caching
/// somewhere strictly better.
const PAGE_CACHE_KIB: i64 = 256;

/// Per-connection `mmap_size`. **Deliberately 0 (off).**
///
/// Mapping the database looks appealing for a read-only workload — reads come
/// straight from a file-backed mapping instead of being copied into SQLite's heap.
/// Measured, it's a bad trade *for us*: the mapped pages are charged to our RSS
/// (they showed up as 657 MB of `Private_Clean` across 139 feeds, and `mmap_size`
/// is per connection, so it scales with feeds loaded). They're evictable, so it
/// isn't a real leak — but the kernel page-caches those file pages **anyway** when
/// we read them normally, giving the same caching benefit without charging them to
/// this process or burning address space. Off is both lighter and easier to reason
/// about. Left here, with the numbers, so nobody re-adds it hopefully.
const MMAP_BYTES: i64 = 0;

/// A global ceiling on SQLite's heap across *every* connection — a backstop behind
/// [`PAGE_CACHE_KIB`]. Rather than trusting N per-connection budgets to add up to
/// something sane, this bounds the sum directly: over the limit, SQLite reclaims
/// pages from caches instead of growing. Soft (it degrades to more disk reads, never
/// to an allocation failure), and a no-op if the bundled SQLite lacks memory
/// management — so it can only help.
const SQLITE_HEAP_LIMIT: i64 = 96 * 1024 * 1024;

/// Install the global SQLite heap ceiling. Idempotent, cheap, and safe to call more
/// than once; done on the first feed load.
fn set_heap_limit() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: sets a global allocator limit; takes no connection and no lock,
        // and is documented as callable from any thread at any time.
        unsafe { libsqlite3_sys::sqlite3_soft_heap_limit64(SQLITE_HEAP_LIMIT) };
    });
}

/// Table definitions for a feed's SQLite index. Secondary indexes on the big
/// tables are created *after* bulk insert (much faster); the `*_id` primary keys
/// give the small tables their lookup index for free. `stop_times` keeps its
/// implicit `rowid` (used to find a trip's final stop in file order).
const SCHEMA: &str = "\
CREATE TABLE agency(timezone TEXT);
CREATE TABLE routes(route_id TEXT PRIMARY KEY, short_name TEXT, long_name TEXT);
CREATE TABLE stops(stop_id TEXT PRIMARY KEY, stop_name TEXT);
CREATE TABLE trips(trip_id TEXT PRIMARY KEY, route_id TEXT, trip_headsign TEXT, direction_id INTEGER, shape_id TEXT);
CREATE TABLE stop_times(trip_id TEXT, stop_sequence INTEGER, stop_id TEXT, time INTEGER);
CREATE TABLE shapes(shape_id TEXT, seq INTEGER, lat REAL, lon REAL);
";

/// An indexed static GTFS feed, backed by an on-disk SQLite database.
///
/// Holds essentially no feed data in memory: a read-only connection (whose page
/// cache is bounded by a `cache_size` pragma) and the agency timezone, resolved
/// once. Every query is an indexed disk lookup — see the module docs.
pub struct Gtfs {
    /// Read-only SQLite handle over the imported feed. Wrapped in a `Mutex` so
    /// `Arc<Gtfs>` is `Send + Sync` for the blocking pool. Each loaded feed has
    /// its own connection, so different feeds never contend; a single feed's
    /// queries run serially within one poll anyway, so the lock is uncontended.
    db: Mutex<Connection>,
    /// Agency timezone (from the first `agency.txt` row), resolved once at load.
    /// Anchors seconds-since-midnight schedule times; `None` if it doesn't parse.
    tz: Option<Tz>,
}

/// One `trips.txt` row, as returned by [`Gtfs::trip`]. (A trip's `shape_id` isn't
/// here — [`Gtfs::trip_shape`] joins straight through it in one query.)
pub struct Trip {
    pub route_id: String,
    pub trip_headsign: Option<String>,
    pub direction_id: Option<Direction>,
}

/// GTFS `direction_id`: 0 is outbound, 1 is inbound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Outbound,
    Inbound,
}

impl Gtfs {
    /// Download (and cache) a static GTFS zip, then open its SQLite index —
    /// building the index from the zip on first use, or whenever the zip is newer.
    ///
    /// The download is async; the (one-time) import and the connection open are
    /// CPU/IO-bound and run on the blocking pool so they never stall the runtime.
    pub async fn load(
        slug: &str,
        display_name: &str,
        static_url: &str,
        client: &Client,
        cache_dir: &Path,
    ) -> Result<Gtfs> {
        let zip_path = download_static_feed(slug, display_name, static_url, client, cache_dir)
            .await
            .with_context(|| format!("downloading static GTFS for {display_name}"))?;

        let db_path = cache_dir.join(format!("{slug}.sqlite"));
        let display_name = display_name.to_string();
        tokio::task::spawn_blocking(move || open_or_build(zip_path, db_path, display_name)).await?
    }

    /// Run a prepared single-row query and return its first column as an optional
    /// string — the shape of every name lookup here.
    fn query_string(&self, sql: &str, params: impl rusqlite::Params) -> Option<String> {
        let conn = self.db.lock().ok()?;
        conn.prepare_cached(sql)
            .ok()?
            .query_row(params, |r| r.get::<_, Option<String>>(0))
            .optional()
            .ok()?
            .flatten()
    }

    /// Best human-readable name for a route: short name, then long name. `None`
    /// when unknown or unnamed — callers fall back to the raw id.
    pub fn route_name(&self, route_id: &str) -> Option<String> {
        self.query_string(
            "SELECT coalesce(short_name, long_name) FROM routes WHERE route_id = ?1",
            [route_id],
        )
    }

    /// Rider-facing name of a stop, if known.
    pub fn stop_name(&self, stop_id: &str) -> Option<String> {
        self.query_string("SELECT stop_name FROM stops WHERE stop_id = ?1", [stop_id])
    }

    /// The agency's timezone, needed to anchor seconds-since-midnight schedule
    /// times. Resolved once at load, so this is a cheap field read.
    pub fn timezone(&self) -> Option<Tz> {
        self.tz
    }

    /// Scheduled arrival for a stop on a trip, as seconds since local midnight.
    ///
    /// Matched by `stop_sequence` first (the reliable key), then by `stop_id` — the
    /// `ORDER BY` floats a sequence match to the top so it wins when present; a
    /// missing key binds NULL and matches nothing. `time` was baked to
    /// arrival-else-departure at import.
    pub fn scheduled_arrival_secs(
        &self,
        trip_id: &str,
        stop_sequence: Option<u32>,
        stop_id: Option<&str>,
    ) -> Option<u32> {
        let conn = self.db.lock().ok()?;
        conn.prepare_cached(
            "SELECT time FROM stop_times WHERE trip_id = ?1 AND (stop_sequence = ?2 OR stop_id = ?3) \
             ORDER BY (stop_sequence = ?2) DESC LIMIT 1",
        )
        .ok()?
        .query_row(
            params![trip_id, stop_sequence.map(|s| s as i64), stop_id],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()
        .ok()?
        .flatten()
        .map(|t| t as u32)
    }

    /// Destination name (the trip's final stop, by file order), a headsign fallback.
    pub fn trip_destination(&self, trip_id: &str) -> Option<String> {
        self.query_string(
            "SELECT s.stop_name FROM stop_times st JOIN stops s ON s.stop_id = st.stop_id \
             WHERE st.trip_id = ?1 ORDER BY st.rowid DESC LIMIT 1",
            [trip_id],
        )
    }

    /// Name of the stop at `stop_sequence` on a trip, for the rare realtime feed
    /// that gives a sequence but no `stop_id`.
    pub fn stop_name_at_sequence(&self, trip_id: &str, stop_sequence: u32) -> Option<String> {
        self.query_string(
            "SELECT s.stop_name FROM stop_times st JOIN stops s ON s.stop_id = st.stop_id \
             WHERE st.trip_id = ?1 AND st.stop_sequence = ?2 LIMIT 1",
            params![trip_id, stop_sequence as i64],
        )
    }

    /// One trip's row (route, headsign, direction), or `None` if unknown.
    pub fn trip(&self, trip_id: &str) -> Option<Trip> {
        let conn = self.db.lock().ok()?;
        conn.prepare_cached(
            "SELECT route_id, trip_headsign, direction_id FROM trips WHERE trip_id = ?1",
        )
        .ok()?
        .query_row([trip_id], |r| {
            Ok(Trip {
                route_id: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                trip_headsign: r.get::<_, Option<String>>(1)?,
                direction_id: r.get::<_, Option<i64>>(2)?.and_then(direction_from_int),
            })
        })
        .optional()
        .ok()?
    }

    /// Total distinct trips in the schedule — the agency's scale metric.
    pub fn trip_count(&self) -> usize {
        let conn = self.db.lock().ok();
        conn.and_then(|c| {
            c.query_row("SELECT count(*) FROM trips", [], |r| r.get::<_, i64>(0))
                .ok()
        })
        .map(|n| n as usize)
        .unwrap_or(0)
    }

    /// The geographic path a trip follows, as ordered `(lat, lon)` points — for
    /// drawing the route line behind the delayed vehicle.
    ///
    /// Draws the trip's **own** `shape_id`, which is the most accurate path for that
    /// specific run. Only when the trip has no shape of its own does it fall back to
    /// the **canonical** shape for the trip's route + direction (the pattern shared
    /// by the most trips), so a trip whose feed omits shapes still gets a sensible
    /// route line. Read on demand via the `shapes(shape_id)` index and retained by
    /// nobody. `None` if no shape is available at all.
    pub fn trip_shape(&self, trip_id: &str) -> Option<Vec<(f64, f64)>> {
        let conn = self.db.lock().ok()?;
        let points: Vec<(f64, f64)> = conn
            .prepare_cached(
                "SELECT lat, lon FROM shapes WHERE shape_id = coalesce(\
                   (SELECT t.shape_id FROM trips t WHERE t.trip_id = ?1 \
                     AND EXISTS (SELECT 1 FROM shapes s WHERE s.shape_id = t.shape_id)), \
                   (SELECT shape_id FROM trips \
                     WHERE route_id = (SELECT route_id FROM trips WHERE trip_id = ?1) \
                       AND coalesce(direction_id, -1) = \
                           coalesce((SELECT direction_id FROM trips WHERE trip_id = ?1), -1) \
                       AND shape_id IS NOT NULL \
                     GROUP BY shape_id ORDER BY count(*) DESC LIMIT 1)) \
                 ORDER BY seq",
            )
            .ok()?
            .query_map([trip_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .ok()?
            .filter_map(Result::ok)
            .collect();
        (!points.is_empty()).then_some(points)
    }

    /// The trip's terminal stops — its first (lowest `stop_sequence`) and last
    /// (highest) — each as `(sequence, stop_id)`. Used to recognize a vehicle at the
    /// origin or the destination of its trip, where a schedule-comparison delay is
    /// unreliable.
    pub fn terminal_stops(&self, trip_id: &str) -> Option<TerminalStops> {
        let conn = self.db.lock().ok()?;
        conn.prepare_cached(
            "SELECT min(stop_sequence), max(stop_sequence), \
                    (SELECT stop_id FROM stop_times WHERE trip_id = ?1 \
                       ORDER BY stop_sequence LIMIT 1), \
                    (SELECT stop_id FROM stop_times WHERE trip_id = ?1 \
                       ORDER BY stop_sequence DESC LIMIT 1) \
             FROM stop_times WHERE trip_id = ?1",
        )
        .ok()?
        .query_row([trip_id], |r| {
            Ok(TerminalStops {
                first: (r.get::<_, Option<i64>>(0)?.map(|s| s as u32), r.get(2)?),
                last: (r.get::<_, Option<i64>>(1)?.map(|s| s as u32), r.get(3)?),
            })
        })
        .optional()
        .ok()?
        // A trip with no stop_times yields NULL min/max: not a real terminal.
        .filter(|t| t.first.0.is_some())
    }

    /// Everything the delay math sees for one trip in this static schedule, as a
    /// JSON blob for a debug capture: the agency timezone, the trip's row, and its
    /// full ordered `stop_times`. Best-effort — a lock or query failure yields
    /// `null` rather than erroring, since this only feeds a diagnostic archive.
    pub fn debug_dump(&self, trip_id: &str) -> serde_json::Value {
        use serde_json::json;
        let Ok(conn) = self.db.lock() else {
            return serde_json::Value::Null;
        };

        let trip = conn
            .prepare_cached(
                "SELECT route_id, trip_headsign, direction_id, shape_id FROM trips WHERE trip_id = ?1",
            )
            .ok()
            .and_then(|mut stmt| {
                stmt.query_row([trip_id], |r| {
                    Ok(json!({
                        "route_id": r.get::<_, Option<String>>(0)?,
                        "trip_headsign": r.get::<_, Option<String>>(1)?,
                        "direction_id": r.get::<_, Option<i64>>(2)?,
                        "shape_id": r.get::<_, Option<String>>(3)?,
                    }))
                })
                .optional()
                .ok()
                .flatten()
            });

        let mut stop_times = Vec::new();
        if let Ok(mut stmt) = conn.prepare_cached(
            "SELECT stop_sequence, stop_id, time FROM stop_times WHERE trip_id = ?1 ORDER BY stop_sequence",
        ) && let Ok(rows) = stmt.query_map([trip_id], |r| {
            Ok(json!({
                "stop_sequence": r.get::<_, Option<i64>>(0)?,
                "stop_id": r.get::<_, Option<String>>(1)?,
                "time": r.get::<_, Option<i64>>(2)?,
            }))
        }) {
            stop_times.extend(rows.flatten());
        }

        json!({
            "timezone": self.tz.map(|tz| tz.to_string()),
            "trip": trip,
            "stop_times_count": stop_times.len(),
            "stop_times": stop_times,
        })
    }
}

/// A trip's endpoints, as returned by [`Gtfs::terminal_stops`]: the first and last
/// stop, each `(stop_sequence, stop_id)`.
pub struct TerminalStops {
    pub first: (Option<u32>, Option<String>),
    pub last: (Option<u32>, Option<String>),
}

/// Where a trip is signed for: its `trip_headsign`, falling back to its
/// destination (final stop).
pub fn trip_headsign(gtfs: &Gtfs, trip_id: &str, trip: &Trip) -> Option<String> {
    trip.trip_headsign
        .clone()
        .or_else(|| gtfs.trip_destination(trip_id))
}

/// Turn a service date plus seconds-since-local-midnight into a UTC unix
/// timestamp, interpreting the local day in `tz`.
///
/// GTFS times may exceed 24h (a trip that spills past midnight), which is why we
/// add the seconds to the day's midnight rather than constructing a wall-clock
/// time.
pub fn local_time_to_unix(
    tz: Tz,
    service_date: NaiveDate,
    secs_since_midnight: u32,
) -> Option<i64> {
    let midnight = service_date.and_hms_opt(0, 0, 0)?;
    let local_midnight = tz.from_local_datetime(&midnight).earliest()?;
    Some(local_midnight.timestamp() + secs_since_midnight as i64)
}

/// How much heap SQLite is holding across *every* open connection right now, and
/// its high-water mark, in bytes — page caches, prepared-statement caches, schema.
///
/// This is the honest way to answer "is SQLite our memory problem?". Each loaded
/// feed keeps its own read-only connection with a bounded page cache (see
/// [`open_or_build`]), so the figure scales with feeds *loaded*, not feeds polled.
/// Reported in `/status`'s summary alongside process RSS so the two can be compared
/// directly — without that comparison, tuning SQLite is guesswork.
pub fn sqlite_memory() -> (i64, i64) {
    // SAFETY: both are pure reads of SQLite's global allocator counters, safe to
    // call from any thread at any time (they take no connection and no lock).
    unsafe {
        (
            libsqlite3_sys::sqlite3_memory_used(),
            libsqlite3_sys::sqlite3_memory_highwater(0),
        )
    }
}

/// Resident set size of this process in bytes, read from `/proc/self/statm`
/// (Linux). `None` elsewhere, or if the file can't be read.
pub fn process_rss() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    // Field 2 is resident pages.
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

/// Open a feed's SQLite index read-only, (re)building it from the cached zip when
/// the db is missing or older than the zip. Blocking; run via [`Gtfs::load`].
fn open_or_build(zip_path: PathBuf, db_path: PathBuf, display_name: String) -> Result<Gtfs> {
    if needs_rebuild(&db_path, &zip_path) {
        build_sqlite(&zip_path, &db_path, &display_name)
            .with_context(|| format!("building SQLite index for {display_name}"))?;
    }

    set_heap_limit();

    let conn = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening SQLite index for {display_name}"))?;
    // Keep a loaded feed's schedule off our heap: page in from disk as queried, and
    // read through an mmap so the pages are file-backed (kernel-owned, evictable)
    // rather than anonymous. This is what stops memory scaling with feeds loaded —
    // see the constants.
    conn.pragma_update(None, "cache_size", -PAGE_CACHE_KIB)?;
    conn.pragma_update(None, "mmap_size", MMAP_BYTES)?;

    let tz = conn
        .query_row(
            "SELECT timezone FROM agency WHERE timezone IS NOT NULL LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok());

    Ok(Gtfs {
        db: Mutex::new(conn),
        tz,
    })
}

/// Whether the SQLite index must be rebuilt: it's missing, or the cached zip it
/// was derived from is newer (a maintenance refresh re-downloaded a fresh zip).
fn needs_rebuild(db_path: &Path, zip_path: &Path) -> bool {
    match (mtime(db_path), mtime(zip_path)) {
        (Some(db), Some(zip)) => db < zip,
        _ => true,
    }
}

/// A file's modification time, or `None` if it can't be read.
fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Import a cached GTFS zip into a fresh SQLite database, streaming each CSV
/// member row-by-row so no whole table is ever collected in memory. Built to a
/// unique temp file and atomically renamed into place, so a concurrent build or a
/// crash mid-import can't leave a half-written index.
fn build_sqlite(zip_path: &Path, db_path: &Path, display_name: &str) -> Result<()> {
    println!("[{display_name}] building SQLite index from static GTFS");
    let mut archive = open_zip(zip_path)
        .with_context(|| format!("opening static GTFS zip for {display_name}"))?;

    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let file_name = db_path.file_name().and_then(|s| s.to_str()).unwrap_or("db");
    let tmp = db_path.with_file_name(format!("{file_name}.{seq}.sqltmp"));
    let _ = std::fs::remove_file(&tmp);

    let conn = Connection::open(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    // Import speed: no rollback journal and no fsync — the db is a rebuildable
    // derivative, so durability doesn't matter; on any error we drop the temp.
    conn.execute_batch(
        "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA temp_store=MEMORY;",
    )?;
    conn.execute_batch(SCHEMA)?;
    conn.execute_batch("BEGIN")?;

    // agency: only the timezone, first non-empty row wins at query time.
    insert_member(
        &conn,
        &mut archive,
        "agency.txt",
        "INSERT INTO agency(timezone) VALUES(?1)",
        |row, stmt| match row.get_non_empty("agency_timezone") {
            Some(tz) => stmt.execute(params![tz]).map(|_| ()),
            None => Ok(()),
        },
    )?;

    // routes / stops / trips are keyed; `OR REPLACE` makes later rows win on a
    // duplicate id.
    insert_member(
        &conn,
        &mut archive,
        "routes.txt",
        "INSERT OR REPLACE INTO routes(route_id, short_name, long_name) VALUES(?1,?2,?3)",
        |row, stmt| match row.get_non_empty("route_id") {
            Some(id) => stmt
                .execute(params![
                    id,
                    row.get_non_empty("route_short_name"),
                    row.get_non_empty("route_long_name"),
                ])
                .map(|_| ()),
            None => Ok(()),
        },
    )?;

    insert_member(
        &conn,
        &mut archive,
        "stops.txt",
        "INSERT OR REPLACE INTO stops(stop_id, stop_name) VALUES(?1,?2)",
        |row, stmt| match row.get_non_empty("stop_id") {
            Some(id) => stmt
                .execute(params![id, row.get_non_empty("stop_name")])
                .map(|_| ()),
            None => Ok(()),
        },
    )?;

    insert_member(
        &conn,
        &mut archive,
        "trips.txt",
        "INSERT OR REPLACE INTO trips(trip_id, route_id, trip_headsign, direction_id, shape_id) \
         VALUES(?1,?2,?3,?4,?5)",
        |row, stmt| match row.get_non_empty("trip_id") {
            Some(trip_id) => {
                let direction = row
                    .get("direction_id")
                    .and_then(parse_direction)
                    .map(direction_to_int);
                stmt.execute(params![
                    trip_id,
                    row.get("route_id").unwrap_or_default(),
                    row.get_non_empty("trip_headsign"),
                    direction,
                    row.get_non_empty("shape_id"),
                ])
                .map(|_| ())
            }
            None => Ok(()),
        },
    )?;

    // stop_times: the big one. Bake arrival-else-departure into a single `time`.
    insert_member(
        &conn,
        &mut archive,
        "stop_times.txt",
        "INSERT INTO stop_times(trip_id, stop_sequence, stop_id, time) VALUES(?1,?2,?3,?4)",
        |row, stmt| {
            let Some(trip_id) = row.get_non_empty("trip_id") else {
                return Ok(());
            };
            let seq = row
                .get("stop_sequence")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0);
            let time = row
                .get("arrival_time")
                .and_then(parse_gtfs_time)
                .or_else(|| row.get("departure_time").and_then(parse_gtfs_time))
                .map(|t| t as i64);
            stmt.execute(params![
                trip_id,
                seq as i64,
                row.get("stop_id").unwrap_or_default(),
                time,
            ])
            .map(|_| ())
        },
    )?;

    // shapes: the other big one, only rows with a usable id and coordinates.
    insert_member(
        &conn,
        &mut archive,
        "shapes.txt",
        "INSERT INTO shapes(shape_id, seq, lat, lon) VALUES(?1,?2,?3,?4)",
        |row, stmt| {
            let Some(shape_id) = row.get_non_empty("shape_id") else {
                return Ok(());
            };
            let (Some(lat), Some(lon)) = (
                row.get("shape_pt_lat")
                    .and_then(|s| s.trim().parse::<f64>().ok()),
                row.get("shape_pt_lon")
                    .and_then(|s| s.trim().parse::<f64>().ok()),
            ) else {
                return Ok(());
            };
            let seq = row
                .get("shape_pt_sequence")
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(0);
            stmt.execute(params![shape_id, seq as i64, lat, lon])
                .map(|_| ())
        },
    )?;

    conn.execute_batch("COMMIT")?;
    // Build the big-table indexes after bulk load, not during it.
    conn.execute_batch(
        "CREATE INDEX idx_stop_times_trip ON stop_times(trip_id);
         CREATE INDEX idx_shapes_shape ON shapes(shape_id);",
    )?;
    drop(conn);

    std::fs::rename(&tmp, db_path).with_context(|| format!("finalizing {}", db_path.display()))?;
    Ok(())
}

/// Stream one CSV member into the database via a prepared statement, invoking
/// `bind` once per row (which reads the row and executes the insert). Rows are
/// processed one at a time, so peak memory is a single row — never the whole
/// table. Stops at the first insert error and reports it.
fn insert_member(
    conn: &Connection,
    archive: &mut Zip,
    member: &str,
    sql: &str,
    mut bind: impl FnMut(&Row, &mut rusqlite::Statement<'_>) -> rusqlite::Result<()>,
) -> Result<()> {
    let mut stmt = conn.prepare(sql)?;
    let mut first_err: rusqlite::Result<()> = Ok(());
    for_each_row(archive, member, |row| {
        if first_err.is_err() {
            return;
        }
        if let Err(err) = bind(row, &mut stmt) {
            first_err = Err(err);
        }
    })?;
    first_err.with_context(|| format!("inserting rows of {member}"))
}

/// Count the trips in an agency's static schedule — the distinct `trip_id`s in
/// `trips.txt` — refreshing the cached zip if stale but **retaining nothing in
/// memory**. This gives every agency a scale metric without paying to fully
/// import (and index) its static feed.
pub async fn count_trips(
    slug: &str,
    display_name: &str,
    static_url: &str,
    client: &Client,
    cache_dir: &Path,
) -> Result<usize> {
    let zip_path = download_static_feed(slug, display_name, static_url, client, cache_dir)
        .await
        .with_context(|| format!("downloading static GTFS for {display_name}"))?;
    tokio::task::spawn_blocking(move || count_trips_in_zip(&zip_path)).await?
}

/// Count distinct non-empty `trip_id`s in `trips.txt`, matching the semantics of
/// the `trips` table so the count is identical whether or not the feed is indexed.
fn count_trips_in_zip(zip_path: &Path) -> Result<usize> {
    let mut archive = open_zip(zip_path)?;
    let mut ids = std::collections::HashSet::new();
    for_each_row(&mut archive, "trips.txt", |row| {
        if let Some(id) = row.get_non_empty("trip_id") {
            ids.insert(id);
        }
    })?;
    Ok(ids.len())
}

/// Whether a cached file is missing or older than `ttl` (by mtime), and so needs
/// re-fetching. A missing or unreadable file counts as stale; a future mtime
/// (clock skew) counts as fresh rather than thrashing the download.
pub async fn is_stale(path: &Path, ttl: Duration) -> bool {
    match tokio::fs::metadata(path).await {
        Ok(meta) => match meta.modified() {
            Ok(modified) => modified.elapsed().map(|age| age >= ttl).unwrap_or(false),
            Err(_) => false,
        },
        Err(_) => true,
    }
}

/// Whether these bytes actually begin with a ZIP signature: a local file header
/// (`PK\x03\x04`), or the end-of-central-directory record of an empty archive
/// (`PK\x05\x06`).
///
/// Worth checking because a **200 OK is not a GTFS zip**. Several agencies (the
/// Availtec/InfoPoint stack behind The Rapid and TARC, among others) answer a zip
/// request with a 200 carrying a plain-text error — e.g. *"Failed response to
/// GTFS-Zip request: Reason=The process cannot access the file … because it is being
/// used by another process"* — so `error_for_status()` waves it through.
fn looks_like_zip(bytes: &[u8]) -> bool {
    matches!(bytes.get(..4), Some(b"PK\x03\x04") | Some(b"PK\x05\x06"))
}

/// A short printable slice of a non-zip response, so the log says what the server
/// actually sent instead of a bare "invalid Zip archive".
fn body_preview(bytes: &[u8]) -> String {
    let head = &bytes[..bytes.len().min(180)];
    String::from_utf8_lossy(head)
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Whether a cached file on disk still starts with a ZIP signature.
///
/// A cache entry that isn't a zip is *poison*: the cache is TTL'd by mtime, so a
/// junk body written once is trusted as "fresh" for a full [`STATIC_TTL`] (24h) and
/// the agency stays dark for a day over what is usually a momentary upstream glitch.
/// Checking the magic on the way out of the cache means such an entry re-downloads
/// immediately instead.
/// Reads only the 4-byte signature — this runs on every cache hit, and the cached
/// zips run to tens of megabytes.
async fn cached_zip_is_valid(path: &Path) -> bool {
    use tokio::io::AsyncReadExt;
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return false;
    };
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).await.is_ok() && looks_like_zip(&magic)
}

/// Return the path to the agency's cached static GTFS zip, downloading it first
/// if the cache is missing, stale ([`STATIC_TTL`]), or holding a non-zip body.
async fn download_static_feed(
    slug: &str,
    display_name: &str,
    static_url: &str,
    client: &Client,
    cache_dir: &Path,
) -> Result<PathBuf> {
    let zip_path = cache_dir.join(format!("{slug}.zip"));
    if zip_path.exists() {
        if !cached_zip_is_valid(&zip_path).await {
            // Poison from an earlier run (see [`looks_like_zip`]). Drop it rather than
            // leave junk in the cache pretending to be this agency's schedule.
            eprintln!("[{display_name}] cached static GTFS is not a zip — discarding");
            let _ = tokio::fs::remove_file(&zip_path).await;
        } else if !is_stale(&zip_path, STATIC_TTL).await {
            return Ok(zip_path);
        }
    }

    // Some catalogs point at a GTFS zip *nested inside* another zip with a URL
    // fragment: `https://…/gtfs_public.zip#google_bus.zip` (SEPTA ships bus and
    // rail this way). Fetch the outer archive, then unwrap the named inner zip so
    // the cache always holds a flat GTFS zip the rest of the loader can read.
    let (fetch_url, inner_zip) = match static_url.split_once('#') {
        Some((base, frag)) if frag.ends_with(".zip") => (base, Some(frag)),
        _ => (static_url, None),
    };

    println!("Fetching static GTFS for {display_name} from {static_url}");
    let bytes = client
        .get(fetch_url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;

    // Refuse to cache a body that isn't a zip. Some agencies answer with 200 OK and
    // a plain-text error (see [`looks_like_zip`]); without this we'd write that text
    // to `<slug>.zip` and, because the cache is mtime-TTL'd, trust it for 24h — one
    // momentary file-lock upstream taking the agency dark for a day. Bail instead:
    // nothing is written, so the next pass simply retries.
    if !looks_like_zip(&bytes) {
        anyhow::bail!(
            "{fetch_url} returned {} bytes that are not a ZIP archive \
             (HTTP 200 with an error body?): {}",
            bytes.len(),
            body_preview(&bytes),
        );
    }

    let bytes = match inner_zip {
        Some(inner) => extract_inner_zip(&bytes, inner)
            .with_context(|| format!("extracting {inner} from {fetch_url}"))?,
        None => bytes.to_vec(),
    };

    tokio::fs::create_dir_all(cache_dir)
        .await
        .with_context(|| format!("creating cache dir {}", cache_dir.display()))?;

    // Write to a unique temp file, then atomically rename into place: a
    // concurrent fetch of the same feed can never leave a half-written zip, and
    // any reader already parsing the old zip keeps its open handle intact.
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = cache_dir.join(format!("{slug}.{seq}.tmp"));
    tokio::fs::write(&tmp, &bytes)
        .await
        .with_context(|| format!("writing {}", tmp.display()))?;
    tokio::fs::rename(&tmp, &zip_path)
        .await
        .with_context(|| format!("finalizing {}", zip_path.display()))?;

    Ok(zip_path)
}

/// Pull a single nested zip member (matched by basename) out of an outer zip's
/// bytes, returning the inner zip's bytes — see the `#fragment` handling in
/// [`download_static_feed`].
fn extract_inner_zip(outer_bytes: &[u8], inner_name: &str) -> Result<Vec<u8>> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(outer_bytes)).context("opening outer GTFS zip")?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if entry.name().rsplit('/').next() == Some(inner_name) {
            let mut bytes = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut bytes)?;
            return Ok(bytes);
        }
    }
    anyhow::bail!("no member named {inner_name} in the outer zip")
}

type Zip = zip::ZipArchive<BufReader<std::fs::File>>;

/// Open a cached GTFS zip for reading.
fn open_zip(zip_path: &Path) -> Result<Zip> {
    let file =
        std::fs::File::open(zip_path).with_context(|| format!("opening {}", zip_path.display()))?;
    zip::ZipArchive::new(BufReader::new(file)).context("reading zip archive")
}

/// Index of the archive member whose basename is `name`, or `None` if absent.
/// Matching by basename lets feeds that nest their files in a subdirectory still
/// resolve.
fn member_index(archive: &mut Zip, name: &str) -> Result<Option<usize>> {
    for i in 0..archive.len() {
        let entry = archive.by_index(i)?;
        if entry.name().rsplit('/').next() == Some(name) {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

/// Read a CSV member and invoke `handle` once per data row, **streaming** the
/// member straight out of the zip (decompressing on the fly) instead of buffering
/// it whole — so a giant `stop_times.txt` is processed a row at a time and never
/// lands in memory as one blob. The `csv` reader keeps only its own small internal
/// buffer plus the current record. Absent members are treated as empty (no rows).
fn for_each_row(archive: &mut Zip, name: &str, mut handle: impl FnMut(&Row)) -> Result<()> {
    let Some(index) = member_index(archive, name)? else {
        return Ok(());
    };
    let entry = archive.by_index(index)?;

    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(bom_stripped(entry)?);

    let columns: HashMap<String, usize> = reader
        .headers()
        .with_context(|| format!("reading header of {name}"))?
        .iter()
        .enumerate()
        .map(|(i, h)| (h.trim().to_string(), i))
        .collect();

    let mut record = csv::StringRecord::new();
    while reader
        .read_record(&mut record)
        .with_context(|| format!("reading a row of {name}"))?
    {
        handle(&Row {
            columns: &columns,
            record: &record,
        });
    }
    Ok(())
}

/// A CSV row plus its header-name→index map, for name-based field access.
struct Row<'a> {
    columns: &'a HashMap<String, usize>,
    record: &'a csv::StringRecord,
}

impl Row<'_> {
    /// The raw value of a column by header name, or `None` if the column or cell
    /// is absent.
    fn get(&self, column: &str) -> Option<&str> {
        let idx = *self.columns.get(column)?;
        self.record.get(idx)
    }

    /// Like [`get`](Row::get), but treats empty strings as absent — GTFS fields
    /// are often present-but-blank.
    fn get_non_empty(&self, column: &str) -> Option<String> {
        self.get(column)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }
}

/// Wrap a byte reader so a leading UTF-8 byte-order mark is skipped, buffering at
/// most the three BOM bytes — the streaming equivalent of stripping a BOM off a
/// fully-read buffer, so the first CSV header isn't mangled.
fn bom_stripped(mut reader: impl Read) -> Result<impl Read> {
    let mut prefix = [0u8; 3];
    let n = fill(&mut reader, &mut prefix)?;
    let keep: &[u8] = if n == 3 && prefix == [0xEF, 0xBB, 0xBF] {
        &[]
    } else {
        &prefix[..n]
    };
    // Re-prepend whatever wasn't a BOM, then stream the rest of the member.
    Ok(Cursor::new(keep.to_vec()).chain(reader))
}

/// Read up to `buf.len()` bytes, tolerating short reads and a source shorter than
/// the buffer (unlike `read_exact`, which errors at EOF). Returns how many bytes
/// were read.
fn fill(reader: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(filled)
}

/// Parse a GTFS `direction_id` cell ("0" → outbound, "1" → inbound).
fn parse_direction(value: &str) -> Option<Direction> {
    match value.trim() {
        "0" => Some(Direction::Outbound),
        "1" => Some(Direction::Inbound),
        _ => None,
    }
}

/// `direction_id` as stored in SQLite (outbound → 0, inbound → 1).
fn direction_to_int(direction: Direction) -> i64 {
    match direction {
        Direction::Outbound => 0,
        Direction::Inbound => 1,
    }
}

/// Decode a stored `direction_id` integer back into a [`Direction`].
fn direction_from_int(value: i64) -> Option<Direction> {
    match value {
        0 => Some(Direction::Outbound),
        1 => Some(Direction::Inbound),
        _ => None,
    }
}

/// Parse a GTFS `HH:MM:SS` time into seconds since local midnight. Hours may
/// exceed 24 for trips that run past midnight; fields may be zero-padded or
/// space-padded.
fn parse_gtfs_time(value: &str) -> Option<u32> {
    let mut parts = value.trim().split(':');
    let hours: u32 = parts.next()?.trim().parse().ok()?;
    let minutes: u32 = parts.next()?.trim().parse().ok()?;
    let seconds: u32 = parts.next()?.trim().parse().ok()?;
    Some(hours * 3600 + minutes * 60 + seconds)
}
