// Formatting a delay, and the small derivations both pages agree on.

import type { DelaySource, LeaderboardEntry } from "./types";

/** Seconds -> "1h 12m 30s". The exact readout. */
export function fmtDelay(s: number): string {
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = Math.floor(s % 60);
  if (h) return `${h}h ${m}m ${sec}s`;
  if (m) return `${m}m ${sec}s`;
  return `${sec}s`;
}

/**
 * The same figure split into number/unit pairs, so the hero can set the units
 * smaller than the digits.
 */
export function delayParts(s: number): [number, string][] {
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = Math.floor(s % 60);
  if (h) return [[h, "h"], [m, "m"], [sec, "s"]];
  if (m) return [[m, "m"], [sec, "s"]];
  return [[sec, "s"]];
}

/**
 * The same span coarsened for prose — "20m", "2h 4m". Seconds are exactly as
 * meaningful as they look in a sentence; they belong in the readout below it.
 */
export function fmtSpan(s: number): string {
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (h) return `${h}h ${m}m`;
  if (s >= 60) return `${m}m`;
  return `${Math.floor(s)}s`;
}

/**
 * The delay a trip is ranked and remembered for: the worst it reached while the
 * server could vouch for it. For a running trip that's essentially its current
 * delay; for a finished one it's what the run is remembered for rather than
 * whatever its last frame happened to say.
 */
export const peak = (e: LeaderboardEntry): number =>
  e.peak_delay_seconds ?? e.delay_seconds;

/**
 * How much lateness the trip picked up while we watched. Every ranked trip was on
 * time (or nearly) when we first saw it and got worse under observation — that's
 * what makes the number credible rather than a stale trip id from the feed.
 */
export const growth = (e: LeaderboardEntry): number =>
  Math.max(peak(e) - e.birth_delay_seconds, 0);

export const fmtBirth = (e: LeaderboardEntry): string =>
  e.birth_delay_seconds > 0 ? `${fmtSpan(e.birth_delay_seconds)} late` : "on time";

/**
 * How long ago a finished trip ended, against the *server's* clock.
 *
 * The server sends the end as a timestamp, not an age — an age would change every
 * tick for every finished row and put the whole board back on the wire — so the
 * age is derived here, and against the message's own `generated_at` rather than
 * `Date.now()` so a skewed browser clock can't render "finished -3m ago".
 */
export const endedAgo = (e: LeaderboardEntry, now: number): number | null =>
  e.ended_at == null ? null : Math.max(now - e.ended_at, 0);

/** What names an entry across ticks: the same identity the server keys deltas by. */
export const tripKey = (e: Pick<LeaderboardEntry, "slug" | "trip_id">): string =>
  JSON.stringify([e.slug, e.trip_id]);

export const SOURCE_LABEL: Record<DelaySource, string> = {
  "trip-level": "agency-reported (trip)",
  "stop-level": "agency-reported (stop)",
  "vs-schedule": "compared against the timetable",
};
