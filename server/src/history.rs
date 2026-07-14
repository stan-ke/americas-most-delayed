//! Delay provenance: only credit lateness we actually watched accumulate.
//!
//! Every fake that has topped the leaderboard so far has the same shape: a feed
//! hands us a `trip_id` that no longer describes the run the vehicle is driving.
//! MARTA's AVL finishes a bus's 10:22 run, sends it back out on its 12:22 run, and
//! keeps labelling it `11012496`; we compare a 12:40 bus against a 10:22 timetable
//! and get a fake two hours. LADOT does the same by leaving a bus on a stale block
//! assignment. In a *single* frame these are indistinguishable from a genuinely
//! late bus — the predictions are self-consistent, the vehicle is on its route, the
//! stop is interior. No amount of cross-checking one snapshot against the schedule
//! can separate them, and re-matching the vehicle to its best-fitting scheduled trip
//! is worse than useless: that computes delay *modulo the headway*, silently zeroing
//! out exactly the large delays this project exists to find.
//!
//! Across *time* they're trivially separable, because delay obeys a physical bound:
//!
//! > **A trip's delay can grow no faster than the clock.**
//!
//! A bus sitting motionless in gridlock accumulates exactly one second of lateness
//! per second; nothing can make it accumulate more. So between two observations
//! `dt` apart, a real trip's delay rises by at most `dt`. MARTA's goes from roughly
//! on-time to +121 minutes between two polls at most five minutes apart — not
//! implausible but *impossible*, and impossible without knowing anything about
//! headways or route structure. Meanwhile a genuinely 15-minutes-late bus on a
//! 15-minute route — the case re-matching destroys — sails through, because we
//! watched it pass 3, 6, 9, 12, 15.
//!
//! That gives three rules, applied per feed per poll ([`TripHistory::vet`]):
//!
//! 1. **Birth.** A trip first seen already more than [`CREDIBLE_BIRTH_DELAY`] late
//!    is never credited. We have no evidence its delay is real rather than a stale
//!    label — and a run that never departed is a *cancellation* to the rider, not a
//!    two-hour ride, so it isn't ours to shame anyway.
//! 2. **Growth.** Delay may exceed neither `last_delay + elapsed` nor
//!    `birth_delay + age` (plus [`JUMP_SLACK`], since a prediction is an estimate
//!    and may be revised). The first catches a label flipping mid-run; the second
//!    stops small per-poll slack allowances from compounding into a large fake.
//! 3. **Direction.** A trip's current stop never moves meaningfully backwards
//!    through its own stop sequence. A bus does not un-drive its route; MARTA's
//!    jumps from stop 68 to stop 2.
//!
//! A violation is **sticky** — once a `trip_id` stops describing a real run, the
//! delays it reports afterwards are steady and self-consistent, so re-testing each
//! poll would let the fake back in. A trip absent from its feed for
//! [`ABSENCE_RESET`] is forgotten instead, and starts a fresh life (subject to rule
//! 1) when it returns; that is also what bounds this module's memory.
//!
//! The cost, accepted deliberately: after a restart nothing has a history, so the
//! board fills over the first several minutes instead of instantly, and a genuinely
//! late-*starting* run never scores.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::delay::{DelayedTrip, TripObservation};

/// A trip first observed later than this is never credited: we didn't watch the
/// delay happen, so we can't tell a real one from a stale `trip_id`. Generous
/// enough that ordinary feed jitter at a trip's start still counts as "born on
/// time".
const CREDIBLE_BIRTH_DELAY: i64 = 10 * 60;

/// Headroom on the physical growth bound. A stop's predicted time is an estimate,
/// and a feed may revise it in one step (a closed road, a driver break) rather than
/// letting it drift up second by second. Absorbs that without admitting the
/// hour-scale jumps a stale label produces.
const JUMP_SLACK: i64 = 15 * 60;

/// How long a trip may be absent from its feed before we forget it. On its return
/// it is a *new* run and must be born credible again — which is precisely how a bus
/// that finished its 07:30 trip and reappears, still mislabelled, in the middle of
/// its 09:44 one gets refused. Comfortably longer than the slowest poll interval,
/// so an ordinary gap between polls never resets a live trip.
const ABSENCE_RESET: i64 = 20 * 60;

/// How far a trip's current stop may sit below the furthest it has reached before
/// we call it a backwards jump. A stop or two of wobble is normal (the "current"
/// stop is the one nearest *now*, and predictions shift); a plunge from stop 68 to
/// stop 2 is a different run wearing the same id.
const SEQ_TOLERANCE: u32 = 3;

/// What we remember about one live trip, from the first poll that saw it until it
/// vanishes for [`ABSENCE_RESET`].
struct TripTrack {
    /// When we first saw this trip, and how late it already was — the baseline
    /// every later delay is measured against.
    born_at: i64,
    birth_delay: i64,
    last_seen: i64,
    last_delay: i64,
    /// Furthest stop sequence this trip has reached, for the backwards-jump check.
    max_sequence: Option<u32>,
    /// Whether this trip's delay is still one we can vouch for. Sticky: once
    /// falsified, never restored (only forgetting the trip clears it).
    credible: bool,
}

impl TripTrack {
    fn born(observation: &TripObservation, now: i64) -> Self {
        TripTrack {
            born_at: now,
            birth_delay: observation.delay_seconds,
            last_seen: now,
            last_delay: observation.delay_seconds,
            max_sequence: observation.stop_sequence,
            // Rule 1: a trip we met already badly late is not ours to vouch for.
            credible: observation.delay_seconds <= CREDIBLE_BIRTH_DELAY,
        }
    }

    /// Fold in one fresh observation, falsifying the trip if it breaks a rule.
    fn observe(&mut self, observation: &TripObservation, now: i64) {
        let delay = observation.delay_seconds;

        // Rule 2: delay grows no faster than the clock — measured both since the
        // previous poll (catches a label flipping mid-run) and since birth (stops
        // per-poll slack from compounding into a large fake).
        let within_step = delay <= self.last_delay + (now - self.last_seen) + JUMP_SLACK;
        let within_life = delay <= self.birth_delay + (now - self.born_at) + JUMP_SLACK;

        // Rule 3: a bus does not un-drive its route.
        let moving_forward = match (observation.stop_sequence, self.max_sequence) {
            (Some(sequence), Some(furthest)) => sequence + SEQ_TOLERANCE >= furthest,
            _ => true,
        };

        if !(within_step && within_life && moving_forward) {
            self.credible = false;
        }

        self.last_seen = now;
        self.last_delay = delay;
        self.max_sequence = self.max_sequence.max(observation.stop_sequence);
    }
}

/// What we can say about a credited trip's delay — the evidence behind it, shown
/// on the leaderboard so a delay reads as watched rather than asserted.
#[derive(Debug, Clone, Copy)]
pub struct Provenance {
    /// How long we've been watching this trip.
    pub tracked_seconds: u64,
    /// How late it was when we first saw it.
    pub birth_delay_seconds: i64,
}

/// Per-source memory of every trip seen in the last [`ABSENCE_RESET`], and the
/// gate that decides which of a poll's late trips may reach the leaderboard.
#[derive(Default)]
pub struct TripHistory {
    /// Agency index -> that feed's live trips. Bounded by what each feed has
    /// published recently, and pruned on every poll.
    sources: Mutex<HashMap<usize, HashMap<String, TripTrack>>>,
}

impl TripHistory {
    pub fn new() -> Self {
        TripHistory::default()
    }

    /// Record one poll's observations for a source, then drop from `trips` every
    /// trip whose delay we can't vouch for. Returns how many were dropped.
    ///
    /// `observations` must cover **every** trip the feed published a delay for, late
    /// or not — an on-time sighting is exactly the evidence that lets the same trip
    /// be believed when it later turns up an hour down.
    pub fn vet(
        &self,
        idx: usize,
        observations: &[TripObservation],
        trips: &mut Vec<DelayedTrip>,
        now: i64,
    ) -> usize {
        let mut sources = self.sources.lock().unwrap();
        let tracks = sources.entry(idx).or_default();

        // Forget trips this feed hasn't mentioned in a while: they've finished (or
        // the feed dropped them), and if the id comes back it's a new run.
        tracks.retain(|_, track| now - track.last_seen <= ABSENCE_RESET);

        for observation in observations {
            match tracks.get_mut(&observation.trip_id) {
                Some(track) => track.observe(observation, now),
                None => {
                    tracks.insert(
                        observation.trip_id.clone(),
                        TripTrack::born(observation, now),
                    );
                }
            }
        }

        let before = trips.len();
        trips.retain(|trip| {
            tracks
                .get(&trip.trip_id)
                .is_some_and(|track| track.credible)
        });
        before - trips.len()
    }

    /// The evidence behind one credited trip's delay, if we're still tracking it.
    pub fn provenance(&self, idx: usize, trip_id: &str) -> Option<Provenance> {
        let sources = self.sources.lock().unwrap();
        let track = sources.get(&idx)?.get(trip_id)?;
        Some(Provenance {
            tracked_seconds: (track.last_seen - track.born_at).max(0) as u64,
            birth_delay_seconds: track.birth_delay,
        })
    }

    /// Drop everything remembered about a source — called when a feed is retired,
    /// so a dead agency doesn't hold its trips in memory forever.
    pub fn forget_source(&self, idx: usize) {
        self.sources.lock().unwrap().remove(&idx);
    }
}
