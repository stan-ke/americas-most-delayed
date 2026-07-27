//! The hidden score that ranks the leaderboard.
//!
//! The board used to be a straight sort on `delay_seconds`. That answers "which
//! vehicle's clock is furthest behind", which is *nearly* the question — but not
//! quite, in three ways this module fixes.
//!
//! 1. **Delay is not the same as failure.** Thirty minutes on a 20-minute crosstown
//!    shuttle is a trip that has more than doubled; thirty minutes on a 3-hour
//!    commuter rail run is a bad afternoon. The rider experience scales with the
//!    delay *relative to the trip*, so the score reads both.
//! 2. **A late bus with nobody left to strand isn't the story.** A trip two stops
//!    from its destination has already done its damage; one with thirty stops still
//!    ahead is actively ruining the evening of everyone waiting at them.
//! 3. **A trip stops existing the moment it ends — and that's exactly when it's
//!    most interesting.** The old board could only ever show what was moving *now*,
//!    so the worst trip of the day vanished from the wall of shame the instant it
//!    finally pulled in. Now a finished trip keeps its place and **decays**: see
//!    [`decay`].
//!
//! The score is deliberately **hidden** — it isn't displayed and (outside debug
//! mode) isn't even sent to the browser. It's an ordering, not a statistic, and a
//! number nobody can independently check is worse than useless on a page whose whole
//! claim is that its delays are real. What the page shows is the delay, the trip's
//! receipts, and — for a finished trip — how long ago it ended.
//!
//! ## Shape of the formula
//!
//! Every factor is a **multiplier on minutes late**, floored at or near 1.0, so the
//! ranking stays recognisably a delay ranking: nothing here can promote a 4-minute
//! trip over a 90-minute one. They only reorder trips that are already in the same
//! league.
//!
//! ```text
//! score = minutes_late
//!       × severity   (1.0 … 1 + SEVERITY_WEIGHT)   delay relative to trip length
//!       × reach      (1.0 … 1 + REACH_WEIGHT)      stops still ahead of it
//!       × confidence (CONFIDENCE_FLOOR … 1.0)      how much of it we watched happen
//!       × location   (MISSING_LOCATION_FACTOR … 1.0) whether we can see the vehicle
//!       × decay      (0.0 … 1.0)                   1.0 while live; falls off once ended
//! ```
//!
//! A factor whose input is unknown (no static schedule loaded, a feed that gives no
//! stop sequence) is **1.0** — neutral, never a penalty. A feed we know less about
//! must not be quietly demoted for it. The one deliberate exception is `location`: a
//! *missing* live position isn't ignorance, it's a trip we couldn't verify against
//! its route (the off-route / terminal checks never ran on it), so a vehicle we can't
//! place takes a small, minor penalty rather than a neutral pass.

use serde::Serialize;

/// The hard horizon: a finished trip is off the board after this, whatever it did.
/// [`decay`] is built to reach exactly zero here rather than merely getting small,
/// so nothing can homestead the wall of shame.
pub const MAX_RETENTION_SECS: i64 = 24 * 3600;

/// Time constant of the exponential half of the decay curve. The falloff is fast
/// early — an ordinary finished trip is out of contention within the hour — and long
/// in the tail, so only a genuinely absurd delay is still ranking hours later.
const DECAY_TAU_SECS: f64 = 4.0 * 3600.0;

/// Ceiling on how much the severity factor can add. At full strength a trip that has
/// doubled its own scheduled running time ranks like one with `1 + SEVERITY_WEIGHT`×
/// its delay.
const SEVERITY_WEIGHT: f64 = 0.8;
/// Where severity saturates: a delay this many times the scheduled trip length is
/// already a total failure, and being worse doesn't make it more so.
const SEVERITY_FULL_RATIO: f64 = 1.5;

/// Ceiling on how much the reach factor can add.
const REACH_WEIGHT: f64 = 0.5;
/// Stops-still-ahead at which reach saturates. Logarithmic below it — the difference
/// between 2 stops left and 10 matters far more than between 30 and 40.
const REACH_FULL_STOPS: f64 = 30.0;

/// The most the confidence factor can discount a trip. A delay that arrived mostly
/// pre-formed still ranks — it passed the provenance gate in [`crate::history`], so
/// it's real — but it yields to one we watched build from nothing.
const CONFIDENCE_FLOOR: f64 = 0.6;
/// How the confidence factor splits between *what* we watched (the share of the
/// delay that accrued under observation) and *how long* we watched. They must sum
/// to 1.0.
const CONFIDENCE_GROWTH_SHARE: f64 = 0.7;
const CONFIDENCE_TENURE_SHARE: f64 = 1.0 - CONFIDENCE_GROWTH_SHARE;
/// Observation time at which the tenure half of confidence is fully earned.
const TENURE_FULL_SECS: f64 = 30.0 * 60.0;

/// A small nudge down for a trip whose vehicle we can't place on the map. Without a
/// live position the scheduler's off-route and terminal checks never ran on it, so
/// it's the least-verified thing that can rank — but it *did* pass the provenance
/// gate, so the penalty is deliberately minor: it reorders peers, never overturns a
/// delay. A finished trip keeps the coordinates it was frozen with at its last
/// sighting, so only a vehicle we could never see (in practice a still-running,
/// unverified one) is actually docked.
const MISSING_LOCATION_FACTOR: f64 = 0.9;

/// Everything the score is computed from, gathered in one place so the debug
/// breakdown can show the inputs next to the arithmetic they produced.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ScoreInputs {
    /// The delay being ranked, in seconds. This is the trip's **peak** vetted delay:
    /// the worst it ever got while we could vouch for it. For a live trip that's
    /// almost always its current delay (the growth bound in [`crate::history`] means
    /// delay barely ever falls); for a finished one it's what the trip is remembered
    /// for rather than whatever its last frame happened to say.
    pub delay_seconds: i64,
    /// How long the static schedule says this trip takes end to end. `None` when the
    /// agency's schedule isn't loaded or doesn't know the trip.
    pub scheduled_duration_seconds: Option<i64>,
    /// Stops still ahead of the vehicle when we last placed it in its sequence.
    pub stops_remaining: Option<i64>,
    /// How long we watched this trip, and how late it already was when we met it —
    /// the provenance receipts (see [`crate::history`]).
    pub tracked_seconds: u64,
    pub birth_delay_seconds: i64,
    /// Whether the vehicle-positions feed places this trip on the map. A live vehicle
    /// we can't see takes a small penalty ([`MISSING_LOCATION_FACTOR`]); a finished
    /// trip carries its frozen last-known position, so one we ever placed keeps 1.0.
    pub has_live_location: bool,
    /// How long ago the trip ended. `None` while it's still being reported.
    pub seconds_since_end: Option<i64>,
}

/// The score with its arithmetic laid open — one factor per line, in the order they
/// multiply. Served only when `AMD_DEBUG` is on, behind the leaderboard's 🧮 button,
/// so the ranking can be audited against the inputs that produced it.
#[derive(Debug, Clone, Serialize)]
pub struct ScoreBreakdown {
    pub inputs: ScoreInputs,
    /// Minutes late — the base every factor multiplies.
    pub base: f64,
    pub severity: f64,
    pub reach: f64,
    pub confidence: f64,
    pub location: f64,
    pub decay: f64,
    pub score: f64,
    /// One human-readable line per factor, ready to render as a table.
    pub steps: Vec<ScoreStep>,
}

/// One line of the breakdown: what the factor is, what it came out to, and why.
#[derive(Debug, Clone, Serialize)]
pub struct ScoreStep {
    pub name: &'static str,
    pub value: f64,
    pub detail: String,
}

impl ScoreInputs {
    /// The score itself — the only thing the ranking actually needs.
    pub fn score(&self) -> f64 {
        self.base()
            * self.severity()
            * self.reach()
            * self.confidence()
            * self.location()
            * self.decay()
    }

    /// Minutes late. Everything else is a multiplier on this, which is what keeps
    /// the board "mostly a delay ranking" no matter how the factors are tuned.
    fn base(&self) -> f64 {
        self.delay_seconds.max(0) as f64 / 60.0
    }

    /// How badly the delay breaks *this* trip, as opposed to how big it is in the
    /// abstract: delay over scheduled running time, saturating at
    /// [`SEVERITY_FULL_RATIO`]. Neutral when we don't know the trip's length.
    fn severity(&self) -> f64 {
        let Some(duration) = self.scheduled_duration_seconds.filter(|d| *d > 0) else {
            return 1.0;
        };
        let ratio = (self.delay_seconds.max(0) as f64 / duration as f64) / SEVERITY_FULL_RATIO;
        1.0 + SEVERITY_WEIGHT * ratio.clamp(0.0, 1.0)
    }

    /// How much damage the trip still has left to do: riders waiting at the stops
    /// ahead of it. Logarithmic, because the step from 2 stops to 10 is a different
    /// kind of event than the step from 30 to 40. Neutral when the feed gives us no
    /// position in the sequence.
    fn reach(&self) -> f64 {
        let Some(remaining) = self.stops_remaining.filter(|n| *n > 0) else {
            return 1.0;
        };
        let share = (remaining as f64).ln_1p() / REACH_FULL_STOPS.ln_1p();
        1.0 + REACH_WEIGHT * share.clamp(0.0, 1.0)
    }

    /// How much of this delay we can personally account for. Two halves: the share
    /// of the lateness that accrued *after* we started watching, and how long we've
    /// been watching at all. Both are already gates in [`crate::history`] — nothing
    /// reaches here without passing them — so this is a tiebreak among credible
    /// trips, floored at [`CONFIDENCE_FLOOR`] rather than able to zero anything out.
    fn confidence(&self) -> f64 {
        let delay = self.delay_seconds.max(0) as f64;
        let grown = (self.delay_seconds - self.birth_delay_seconds).max(0) as f64;
        let growth_share = if delay > 0.0 { grown / delay } else { 0.0 };
        let tenure_share = (self.tracked_seconds as f64 / TENURE_FULL_SECS).clamp(0.0, 1.0);

        let earned = CONFIDENCE_GROWTH_SHARE * growth_share.clamp(0.0, 1.0)
            + CONFIDENCE_TENURE_SHARE * tenure_share;
        CONFIDENCE_FLOOR + (1.0 - CONFIDENCE_FLOOR) * earned
    }

    /// A minor penalty for a vehicle we can't place. Neutral (1.0) as soon as we have
    /// coordinates — which a finished trip keeps frozen from its last sighting — so
    /// only a trip we could *never* see (in practice a still-running one whose feed we
    /// haven't verified) is docked. Absence of a position is information, not
    /// ignorance, which is why this is the one factor that penalises a missing input.
    fn location(&self) -> f64 {
        if self.has_live_location {
            1.0
        } else {
            MISSING_LOCATION_FACTOR
        }
    }

    /// How much of the trip's score survives now that it has ended — `1.0` while it
    /// is still being reported. See [`decay`].
    fn decay(&self) -> f64 {
        self.seconds_since_end.map_or(1.0, decay)
    }

    /// The score with every intermediate step exposed, for the debug column.
    pub fn breakdown(&self) -> ScoreBreakdown {
        let (base, severity, reach, confidence, location, decay) = (
            self.base(),
            self.severity(),
            self.reach(),
            self.confidence(),
            self.location(),
            self.decay(),
        );

        let steps = vec![
            ScoreStep {
                name: "base (minutes late)",
                value: base,
                detail: format!("peak vetted delay {}s ÷ 60", self.delay_seconds),
            },
            ScoreStep {
                name: "× severity",
                value: severity,
                detail: match self.scheduled_duration_seconds.filter(|d| *d > 0) {
                    Some(duration) => format!(
                        "delay is {:.0}% of the trip's scheduled {duration}s; saturates at {:.0}%",
                        100.0 * self.delay_seconds.max(0) as f64 / duration as f64,
                        100.0 * SEVERITY_FULL_RATIO,
                    ),
                    None => "no scheduled trip length known — neutral".to_string(),
                },
            },
            ScoreStep {
                name: "× reach",
                value: reach,
                detail: match self.stops_remaining.filter(|n| *n > 0) {
                    Some(remaining) => format!(
                        "{remaining} stop(s) still ahead; log-scaled, saturates at {REACH_FULL_STOPS:.0}"
                    ),
                    None => "no stops-remaining known (or none left) — neutral".to_string(),
                },
            },
            ScoreStep {
                name: "× confidence",
                value: confidence,
                detail: format!(
                    "{}s of the delay accrued under observation, watched {}s (full credit at {:.0}s); floor {CONFIDENCE_FLOOR}",
                    (self.delay_seconds - self.birth_delay_seconds).max(0),
                    self.tracked_seconds,
                    TENURE_FULL_SECS,
                ),
            },
            ScoreStep {
                name: "× location",
                value: location,
                detail: if self.has_live_location {
                    "vehicle placed on the map — neutral".to_string()
                } else {
                    format!(
                        "no live vehicle position — unverified against its route; penalty {MISSING_LOCATION_FACTOR}"
                    )
                },
            },
            ScoreStep {
                name: "× decay",
                value: decay,
                detail: match self.seconds_since_end {
                    Some(age) => format!(
                        "finished {age}s ago; exp(-t/{:.0}h) × (1 - t/{:.0}h), zero at {:.0}h",
                        DECAY_TAU_SECS / 3600.0,
                        MAX_RETENTION_SECS as f64 / 3600.0,
                        MAX_RETENTION_SECS as f64 / 3600.0,
                    ),
                    None => "still running — no decay".to_string(),
                },
            },
        ];

        ScoreBreakdown {
            inputs: *self,
            base,
            severity,
            reach,
            confidence,
            location,
            decay,
            score: base * severity * reach * confidence * location * decay,
            steps,
        }
    }
}

/// What fraction of its score a finished trip keeps, `age` seconds after it ended.
///
/// Two curves multiplied, because neither alone does the job:
///
/// - an **exponential** `exp(-age/TAU)` gives the shape we want — a steep early
///   drop, so the board is dominated by what's happening *now*, over a long tail
///   that lets a genuinely outrageous trip stay visible for hours;
/// - a **linear taper** `1 - age/MAX_RETENTION` forces it to actually reach zero.
///   An exponential never does, and "asymptotically small" is not the same promise
///   as "gone": without the taper a sufficiently insane delay could sit on the wall
///   forever, and the board would slowly become a museum instead of a leaderboard.
///
/// Concretely, at the shipped constants a finished trip retains ~75% after an hour,
/// ~41% after three, ~17% after six, ~2.5% after twelve, and exactly nothing after
/// twenty-four. So surviving to the end of that window means out-scoring the live
/// board by roughly forty times — which is the point: the trips still there the next
/// morning are the ones that were genuinely unbelievable.
pub fn decay(age_seconds: i64) -> f64 {
    if age_seconds <= 0 {
        return 1.0;
    }
    if age_seconds >= MAX_RETENTION_SECS {
        return 0.0;
    }
    let age = age_seconds as f64;
    (-age / DECAY_TAU_SECS).exp() * (1.0 - age / MAX_RETENTION_SECS as f64)
}
