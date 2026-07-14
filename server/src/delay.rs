//! Turning a realtime feed into a list of delayed trips.

use std::collections::HashMap;

use crate::gtfs::{self, Gtfs, Trip};
use chrono::{NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use gtfs_rt::trip_update::StopTimeUpdate;
use gtfs_rt::trip_update::stop_time_update::ScheduleRelationship;
use gtfs_rt::{FeedMessage, TripUpdate};

/// Live vehicle coordinates keyed by `trip_id` — `(latitude, longitude)`.
pub type VehiclePositions = HashMap<String, (f64, f64)>;

/// Live vehicle coordinates from a GTFS-realtime `VehiclePositions` feed, keyed
/// by `trip_id` so a delayed trip can be matched to where its vehicle is. Only
/// entities carrying both a trip id and a position are included.
pub fn vehicle_positions(feed: &FeedMessage) -> VehiclePositions {
    feed.entity
        .iter()
        .filter_map(|entity| entity.vehicle.as_ref())
        .filter_map(|vehicle| {
            let trip_id = vehicle.trip.as_ref()?.trip_id.clone()?;
            let position = vehicle.position.as_ref()?;
            let (lat, lon) = (position.latitude as f64, position.longitude as f64);
            // Drop "null island" (0, 0) — a vehicle with no GPS fix, never a real
            // transit location — so it can't strand the map in the Atlantic.
            (lat != 0.0 || lon != 0.0).then_some((trip_id, (lat, lon)))
        })
        .collect()
}

/// A single trip that is running behind schedule.
#[derive(Debug, Clone)]
pub struct DelayedTrip {
    pub trip_id: String,
    /// Human-readable route label (short name, long name, or id as a fallback).
    pub route: String,
    pub delay_seconds: i64,
    /// How the delay figure was derived — handy while validating a new feed.
    pub source: DelaySource,
    /// Where the bus is signed for: headsign, or direction as a fallback.
    pub headsign: Option<String>,
    /// Name of the next stop the vehicle is heading toward.
    pub next_stop: Option<String>,
    /// Rider-facing vehicle identity: label (bus number), then license plate.
    pub vehicle: Option<String>,
}

/// One trip's delay as measured on a single poll — late, early, or on time.
///
/// The leaderboard only cares about late trips, but [`crate::history`] needs to see
/// *all* of them: an on-time sighting early in a run is the evidence that lets the
/// same trip be believed when it later turns up an hour down. Deliberately cheap
/// (no label lookups), since one is produced for every vehicle in every feed.
#[derive(Debug, Clone)]
pub struct TripObservation {
    pub trip_id: String,
    pub delay_seconds: i64,
    /// Sequence of the stop the delay was read at — where the vehicle is *now*.
    /// Lets the history spot a trip whose current stop jumps backwards, which a
    /// real bus never does.
    pub stop_sequence: Option<u32>,
}

/// Everything one poll of a feed produced: the late trips that may reach the
/// leaderboard, and an observation of *every* trip, for the delay history.
pub struct FeedDelays {
    pub trips: Vec<DelayedTrip>,
    pub observations: Vec<TripObservation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelaySource {
    /// `TripUpdate.delay` — a whole-trip delay reported directly by the agency.
    TripLevel,
    /// `StopTimeEvent.delay` on the next stop the vehicle is heading toward.
    StopLevel,
    /// Realtime predicted arrival vs the static schedule, for feeds that
    /// populate neither delay field but do give predicted times.
    ScheduledComparison,
}

impl DelaySource {
    /// Short tag for the leaderboard, marking how a delay was derived.
    pub fn label(self) -> &'static str {
        match self {
            DelaySource::TripLevel => "trip-level",
            DelaySource::StopLevel => "stop-level",
            DelaySource::ScheduledComparison => "vs-schedule",
        }
    }
}

/// Delays above this are almost certainly bad data (stale timestamps, a feed
/// reporting seconds-since-epoch as a delay, clock skew) rather than a genuinely
/// late vehicle. No local transit trip runs 8 hours behind, so we drop anything
/// past it — a deliberately generous ceiling.
const MAX_PLAUSIBLE_DELAY: i64 = 8 * 3600;

/// A tighter ceiling for *schedule-comparison* delays
/// ([`DelaySource::ScheduledComparison`]), which are our own inference and so carry
/// failure modes a reported `delay` doesn't: a realtime `trip_id` mapping to a
/// *different* scheduled trip (block/id reuse), or a service-date/timezone edge,
/// makes an on-time bus look uniformly hours late — fakes the
/// [`STALE_PREDICTION_SECS`] ghost check can't catch. Real 3h+ delays are almost
/// always reported directly (hitting the looser [`MAX_PLAUSIBLE_DELAY`]), so
/// capping inference here trims fakes while barely touching genuine delays.
const MAX_INFERRED_DELAY: i64 = 3 * 3600;

/// How far from *now* a vehicle's nearest predicted stop may be before its trip is
/// treated as a stale "ghost" (a completed run the feed never expired) rather than
/// live. An in-service vehicle is always minutes from some stop, so a *closest*
/// stop hours away means bogus timestamps; a genuinely late bus is still at a stop
/// *now*, so this never suppresses a real delay.
const STALE_PREDICTION_SECS: i64 = 3600;

/// Measure every trip in one realtime feed message.
///
/// Returns the *late* trips (positive delay, up to the ceiling for how the delay was
/// derived), fully labelled and ready to rank — plus a [`TripObservation`] for every
/// trip we could time at all, late or not, which is what [`crate::history`] needs to
/// tell a delay it watched happen from a stale `trip_id`.
///
/// `gtfs` is the agency's static schedule when available. It is optional: the
/// primary delay signals (trip-level and stop-level) come straight from the
/// realtime feed, so a leaderboard can be built before (or without ever) loading
/// the static feed. When present it enriches labels and unlocks the
/// schedule-comparison fallback.
pub fn delayed_trips(feed: &FeedMessage, gtfs: Option<&Gtfs>) -> FeedDelays {
    let now = Utc::now().timestamp();

    // Some feeds emit the same `trip_id` as several entities in one message (OCTA
    // repeats a trip up to 3x), which would otherwise show as duplicate rows on the
    // leaderboard. Keep one measurement per trip id — the largest delay.
    let mut measured: HashMap<String, (Measurement, &TripUpdate)> = HashMap::new();
    for trip_update in feed.entity.iter().filter_map(|e| e.trip_update.as_ref()) {
        let Some(measurement) = measure_delay(trip_update, gtfs, now) else {
            continue;
        };
        let worse = measured
            .get(&measurement.trip_id)
            .is_none_or(|(existing, _)| measurement.delay_seconds > existing.delay_seconds);
        if worse {
            measured.insert(measurement.trip_id.clone(), (measurement, trip_update));
        }
    }

    let mut trips = Vec::new();
    let mut observations = Vec::with_capacity(measured.len());
    for (trip_id, (measurement, trip_update)) in measured {
        if measurement.is_late() {
            trips.push(describe(trip_update, gtfs, now, &trip_id, &measurement));
        }
        observations.push(TripObservation {
            trip_id,
            delay_seconds: measurement.delay_seconds,
            stop_sequence: measurement.stop_sequence,
        });
    }
    FeedDelays {
        trips,
        observations,
    }
}

/// One trip's raw delay, before we decide whether it's worth labelling and ranking.
struct Measurement {
    trip_id: String,
    delay_seconds: i64,
    source: DelaySource,
    stop_sequence: Option<u32>,
}

impl Measurement {
    /// Whether this trip is late enough — and plausibly enough — to rank. Reported
    /// delays get the generous ceiling, our own inferences the tighter one.
    fn is_late(&self) -> bool {
        let ceiling = match self.source {
            DelaySource::ScheduledComparison => MAX_INFERRED_DELAY,
            _ => MAX_PLAUSIBLE_DELAY,
        };
        self.delay_seconds > 0 && self.delay_seconds <= ceiling
    }
}

/// Whether a feed offers only predicted *times* and no explicit `delay` fields,
/// so its lateness can be derived only by comparing against the static schedule.
///
/// Such feeds (many big agencies: MBTA, TTC, Capital Metro…) surface no delays at
/// all until their static GTFS is loaded — and the scheduler only loads static
/// for feeds that reach the leaderboard, which these never can without delays.
/// The scheduler calls this to break that chicken-and-egg: a times-only feed that
/// produced nothing gets its static loaded so the next poll can compare schedules.
pub fn needs_static_schedule(feed: &FeedMessage) -> bool {
    let mut any_time = false;
    for trip_update in feed.entity.iter().filter_map(|e| e.trip_update.as_ref()) {
        if trip_update.delay.is_some() {
            return false;
        }
        for stop in &trip_update.stop_time_update {
            if stop_delay(stop).is_some() {
                return false;
            }
            any_time |= event_time(stop).is_some();
        }
    }
    any_time
}

/// Measure how late a single trip is, and where its vehicle currently sits in the
/// stop sequence. No label lookups: this runs for every vehicle in every feed.
fn measure_delay(trip_update: &TripUpdate, gtfs: Option<&Gtfs>, now: i64) -> Option<Measurement> {
    let trip_id = trip_update.trip.trip_id.clone()?;

    // Delay is read at the stop nearest *now* (see [`current_stop`]), not the next
    // future stop whose downstream prediction some feeds corrupt.
    let delay_stop = current_stop(trip_update, now);

    // A vehicle parked at one of the trip's *terminal* stops isn't a delayed bus,
    // whatever the feed says its delay is. At the origin it simply never departed —
    // a cancellation to the rider, not an hours-long ride — and at the destination
    // it's a finished run whose timestamps are going stale. Both sit still while
    // their reported delay climbs at a full second per second, which is the fastest
    // any delay can grow ([`crate::history`]), so left in they *win the leaderboard*:
    // Cleveland at stop 1 of 98 and NJ Transit at stop 51 of 52 both did. Genuine
    // lateness shows once a trip is underway at an interior stop, so refusing the
    // endpoints costs no real delay.
    if let (Some(stop), Some(gtfs)) = (delay_stop, gtfs)
        && at_terminal_stop(gtfs, &trip_id, stop)
    {
        return None;
    }

    let stop_sequence = delay_stop.and_then(|stop| stop.stop_sequence);
    let (delay_seconds, source) = trip_delay(trip_update, delay_stop, gtfs, &trip_id, now)?;

    Some(Measurement {
        trip_id,
        delay_seconds,
        source,
        stop_sequence,
    })
}

/// Dress a measured late trip up for the leaderboard: route, headsign, next stop,
/// vehicle. Only called for the handful of trips that are actually rankable, since
/// each of these resolves against the static schedule.
fn describe(
    trip_update: &TripUpdate,
    gtfs: Option<&Gtfs>,
    now: i64,
    trip_id: &str,
    measurement: &Measurement,
) -> DelayedTrip {
    let static_trip = gtfs.and_then(|g| g.trip(trip_id));

    let route = route_label(trip_update, static_trip.as_ref(), gtfs);
    let headsign = describe_headsign(trip_update, static_trip.as_ref(), gtfs, trip_id);
    let next_stop =
        upcoming_stop(trip_update, now).and_then(|stop| next_stop_name(stop, gtfs, trip_id));
    // Some feeds cram the headsign into the vehicle label; don't show it twice.
    let vehicle = vehicle_label(trip_update).filter(|v| Some(v) != headsign.as_ref());

    DelayedTrip {
        trip_id: trip_id.to_string(),
        route,
        delay_seconds: measurement.delay_seconds,
        source: measurement.source,
        headsign,
        next_stop,
        vehicle,
    }
}

/// Whether `stop` is the first or last stop of the trip's static schedule — i.e.
/// the vehicle is sitting at an endpoint rather than running the route. Matches on
/// either the stop sequence or the stop id, since feeds populate one or the other.
/// `false` when the schedule doesn't know the trip (we can't tell, so we don't
/// suppress).
fn at_terminal_stop(gtfs: &Gtfs, trip_id: &str, stop: &StopTimeUpdate) -> bool {
    let Some(terminals) = gtfs.terminal_stops(trip_id) else {
        return false;
    };
    [terminals.first, terminals.last].iter().any(|(seq, id)| {
        (seq.is_some() && stop.stop_sequence == *seq) || (id.is_some() && stop.stop_id == *id)
    })
}

/// Compute a trip's delay, using the most reliable signal available.
///
/// Priority:
/// 1. `TripUpdate.delay` — a whole-trip delay reported directly.
/// 2. `StopTimeEvent.delay` on the next upcoming stop.
/// 3. Realtime predicted arrival at the upcoming stop vs the static schedule.
fn trip_delay(
    trip_update: &TripUpdate,
    stop: Option<&StopTimeUpdate>,
    gtfs: Option<&Gtfs>,
    trip_id: &str,
    now: i64,
) -> Option<(i64, DelaySource)> {
    // 1. Whole-trip delay, reported directly.
    if let Some(delay) = trip_update.delay {
        return Some((delay as i64, DelaySource::TripLevel));
    }

    let stop = stop?;

    // 2. Delay at the next stop the vehicle is heading toward.
    if let Some(delay) = stop_delay(stop) {
        return Some((delay as i64, DelaySource::StopLevel));
    }

    // 3. Predicted arrival vs the static schedule at that stop — only possible
    //    once the static feed (and, on demand, its timetable) is loaded.
    let delay = scheduled_comparison(trip_update, stop, gtfs?, trip_id, now)?;
    Some((delay, DelaySource::ScheduledComparison))
}

/// Delay derived by comparing the realtime predicted arrival at `stop` with the
/// arrival the static schedule promises for that stop.
///
/// This is the only caller that reaches the lazily-loaded timetable
/// ([`Gtfs::scheduled_arrival_secs`]), so a feed reports schedule-comparison
/// delays only after its `stop_times.txt` has been parsed.
fn scheduled_comparison(
    trip_update: &TripUpdate,
    stop: &StopTimeUpdate,
    gtfs: &Gtfs,
    trip_id: &str,
    now: i64,
) -> Option<i64> {
    let tz = gtfs.timezone()?;
    let predicted = event_time(stop)?;
    let scheduled_secs =
        gtfs.scheduled_arrival_secs(trip_id, stop.stop_sequence, stop.stop_id.as_deref())?;
    let service_date = service_date(trip_update, tz, now)?;
    let scheduled = gtfs::local_time_to_unix(tz, service_date, scheduled_secs)?;
    Some(predicted - scheduled)
}

/// The service date for this trip instance: the feed's `start_date` when given,
/// otherwise the local date of `now` in the agency's timezone.
fn service_date(trip_update: &TripUpdate, tz: Tz, now: i64) -> Option<NaiveDate> {
    if let Some(start_date) = &trip_update.trip.start_date
        && let Ok(date) = NaiveDate::parse_from_str(start_date, "%Y%m%d")
    {
        return Some(date);
    }
    Utc.timestamp_opt(now, 0)
        .single()
        .map(|dt| dt.with_timezone(&tz).date_naive())
}

/// The `Scheduled` stop-time updates on a trip — the ones we trust for position.
fn scheduled_stops(trip_update: &TripUpdate) -> impl Iterator<Item = &StopTimeUpdate> {
    trip_update
        .stop_time_update
        .iter()
        .filter(|stu| stu.schedule_relationship() == ScheduleRelationship::Scheduled)
}

/// The earliest scheduled stop by sequence — the fallback when no stop carries a
/// usable predicted time.
fn earliest_stop(trip_update: &TripUpdate) -> Option<&StopTimeUpdate> {
    scheduled_stops(trip_update).min_by_key(|stu| stu.stop_sequence.unwrap_or(u32::MAX))
}

/// The stop marking where the vehicle physically is *right now*: the scheduled
/// stop whose predicted time is closest to `now`, whose delay is the current
/// lateness. Deliberately *not* the next future stop — some feeds emit corrupt
/// downstream predictions (a stop flung hours out with a matching delay) while the
/// just-reached stop still reads correctly. If even the closest stop is more than
/// [`STALE_PREDICTION_SECS`] from now, the trip is a ghost and gets no current stop
/// so its bogus timestamps never reach the delay math.
fn current_stop(trip_update: &TripUpdate, now: i64) -> Option<&StopTimeUpdate> {
    if let Some((time, stu)) = scheduled_stops(trip_update)
        .filter_map(|stu| Some((event_time(stu)?, stu)))
        .min_by_key(|(time, _)| (time - now).abs())
    {
        return ((time - now).abs() <= STALE_PREDICTION_SECS).then_some(stu);
    }
    earliest_stop(trip_update)
}

/// The next scheduled stop by predicted time (what we *display*), falling back to
/// the earliest stop by sequence when no times are present.
fn upcoming_stop(trip_update: &TripUpdate, now: i64) -> Option<&StopTimeUpdate> {
    scheduled_stops(trip_update)
        .filter_map(|stu| Some((event_time(stu)?, stu)))
        .filter(|(time, _)| *time >= now)
        .min_by_key(|(time, _)| *time)
        .map(|(_, stu)| stu)
        .or_else(|| earliest_stop(trip_update))
}

/// Predicted arrival time for a stop, falling back to departure.
fn event_time(stu: &StopTimeUpdate) -> Option<i64> {
    stu.arrival
        .as_ref()
        .and_then(|e| e.time)
        .or_else(|| stu.departure.as_ref().and_then(|e| e.time))
}

/// Reported delay at a stop, preferring arrival over departure.
fn stop_delay(stu: &StopTimeUpdate) -> Option<i32> {
    stu.arrival
        .as_ref()
        .and_then(|e| e.delay)
        .or_else(|| stu.departure.as_ref().and_then(|e| e.delay))
}

/// Best human-readable label for a trip's route.
///
/// The realtime feed's `route_id` is preferred, falling back to the static
/// trip's `route_id`; the resolved route's short name (then long name) is used,
/// with the raw id as a last resort.
fn route_label(
    trip_update: &TripUpdate,
    static_trip: Option<&Trip>,
    gtfs: Option<&Gtfs>,
) -> String {
    let Some(route_id) = trip_update
        .trip
        .route_id
        .clone()
        .or_else(|| static_trip.map(|t| t.route_id.clone()))
    else {
        return "Unknown route".to_string();
    };

    gtfs.and_then(|g| g.route_name(&route_id))
        .unwrap_or(route_id)
}

/// Where the bus is signed for: the static trip's headsign (or destination),
/// falling back to its direction (inbound/outbound) when neither is available.
fn describe_headsign(
    trip_update: &TripUpdate,
    static_trip: Option<&Trip>,
    gtfs: Option<&Gtfs>,
    trip_id: &str,
) -> Option<String> {
    if let (Some(gtfs), Some(trip)) = (gtfs, static_trip)
        && let Some(headsign) = gtfs::trip_headsign(gtfs, trip_id, trip)
    {
        return Some(headsign);
    }
    direction_label(trip_update, static_trip).map(str::to_string)
}

/// Human name for the trip's direction, from the realtime feed or static trip.
fn direction_label(trip_update: &TripUpdate, static_trip: Option<&Trip>) -> Option<&'static str> {
    use crate::gtfs::Direction;

    let label = |outbound: bool| if outbound { "Outbound" } else { "Inbound" };

    if let Some(direction_id) = trip_update.trip.direction_id {
        return Some(label(direction_id == 0));
    }
    static_trip
        .and_then(|t| t.direction_id)
        .map(|d| label(matches!(d, Direction::Outbound)))
}

/// Rider-facing name of the next stop, resolving the realtime `stop_id` (or the
/// trip's stop for that sequence) against the static schedule.
///
/// Returns `None` when the name can't be resolved — we deliberately don't fall
/// back to the raw `stop_id`, since a bare code like `"225"` reads as garbage on
/// the leaderboard rather than a stop.
fn next_stop_name(stop: &StopTimeUpdate, gtfs: Option<&Gtfs>, trip_id: &str) -> Option<String> {
    if let Some(stop_id) = stop.stop_id.as_deref() {
        return gtfs.and_then(|g| g.stop_name(stop_id));
    }

    let sequence = stop.stop_sequence?;
    gtfs?.stop_name_at_sequence(trip_id, sequence)
}

/// Rider-facing vehicle identity from the trip's [`VehicleDescriptor`]: the
/// visible label (typically the bus number), then the license plate.
///
/// [`VehicleDescriptor`]: gtfs_rt::VehicleDescriptor
fn vehicle_label(trip_update: &TripUpdate) -> Option<String> {
    let vehicle = trip_update.vehicle.as_ref()?;
    non_empty(vehicle.label.clone()).or_else(|| non_empty(vehicle.license_plate.clone()))
}

/// Collapse empty strings in an `Option<String>` to `None`
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|s| !s.is_empty())
}
