// The three things the status page derives rather than receives — a source's
// colour, its status word, and how long ago it was polled.

import type { SourceStatus } from "../lib/types";

export type StateClass =
	| "loading"
	| "failed"
	| "auth"
	| "nort"
	| "novp"
	| "ok"
	| "warn"
	| "idle";

/**
 * How long ago this source was last polled.
 *
 * The server sends an absolute timestamp, not an age: an age would change every
 * tick for every source, so nothing would ever be "unchanged" and there'd be no
 * delta left to send.
 */
export const age = (s: SourceStatus, generatedAt: number): number | null =>
	s.last_poll == null ? null : Math.max(generatedAt - s.last_poll, 0);

/**
 * LED colour class for a source's current state — kept in sync with the status
 * text below, so a dot's colour and its tooltip never disagree.
 */
export function stateClass(s: SourceStatus): StateClass {
	// "loading" takes precedence: it's the active work happening right now,
	// regardless of the source's underlying active/no-realtime state.
	if (s.loading) return "loading";
	if (s.state === "failed") return "failed";
	if (s.state === "requires_auth") return "auth";
	if (s.state === "no_realtime") return "nort";
	if (s.state === "no_vehicle_positions") return "novp";
	if (s.last_success === true) return "ok";
	if (s.last_success === false) return "warn";
	return "idle";
}

const SHORT: Record<StateClass, string> = {
	ok: "active",
	warn: "error",
	auth: "needs auth",
	nort: "no realtime",
	novp: "no vehicle pos",
	failed: "failed",
	loading: "loading",
	idle: "idle",
};

/** Short status word for the table, matching the LED colour. */
export const shortStatus = (s: SourceStatus): string => SHORT[stateClass(s)];

/** Human status line. Every error/failure carries its reason. */
export function statusText(s: SourceStatus): string {
	if (s.loading) return "downloading / importing static GTFS…";
	if (s.state === "failed")
		return `failed — HTTP ${s.failed_status}${s.last_error ? ` (${s.last_error})` : ""}`;
	if (s.state === "requires_auth")
		return "requires authentication (not polled)";
	if (s.state === "no_realtime")
		return "no GTFS-realtime feed in the catalog (not polled)";
	if (s.state === "no_vehicle_positions")
		return "no vehicle-positions feed — can't verify routes (not polled)";
	if (s.last_success === false) return `error — ${s.last_error || "unknown"}`;
	if (s.last_success === true) return "active";
	return "idle (not polled yet)";
}

/** The hover card for a dot. */
export function tipText(s: SourceStatus, generatedAt: number): string {
	const lines = [
		s.display_name,
		`status: ${statusText(s)}`,
		`vehicles now: ${s.vehicles_now}`,
		`late right now: ${s.late_trips}`,
		`delays we can't vouch for: ${s.vetted_out}`,
		"scheduled trips: " +
			(s.total_trips != null ? s.total_trips.toLocaleString() : "unknown"),
		`on leaderboard: ${s.hot ? "yes" : "no"}`,
		"poll rate: " +
			(s.poll_interval_seconds != null
				? `every ${s.poll_interval_seconds}s`
				: "not polled"),
	];
	const since = age(s, generatedAt);
	if (since != null) lines.push(`last poll: ${since}s ago`);
	if (s.peak_vehicles) lines.push(`peak vehicles: ${s.peak_vehicles}`);
	if (s.country) lines.push(`country: ${s.country}`);
	return lines.join("\n");
}
