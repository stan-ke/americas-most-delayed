// The shapes the server puts on the wire. These mirror the serializable types in
// `../server/src/scheduler.rs` (and `score.rs`); keep them in step.
//
// Everything here arrives either as a **full** (no `base`) or as a **delta** that
// carries only the fields that changed since `base` — so on a delta every field of
// a row is optional, which is exactly why the merged types below are the ones the
// components see. See `../server/src/wire.rs` for the protocol.

/** How a delay was derived. */
export type DelaySource = "trip-level" | "stop-level" | "vs-schedule";

/**
 * What kind of vehicle runs a trip's route, from the static schedule's
 * `route_type` (see `../server/src/gtfs.rs`'s `RouteKind`). `null` when the
 * agency's schedule isn't loaded or doesn't classify the route.
 */
export type VehicleType =
	| "tram"
	| "subway"
	| "rail"
	| "bus"
	| "ferry"
	| "cable-tram"
	| "aerial-lift"
	| "funicular"
	| "trolleybus"
	| "monorail";

/** One line of the score breakdown: what the factor is, and why. */
export interface ScoreStep {
	name: string;
	value: number;
	detail: string;
}

/** The arithmetic behind a row's rank. Debug mode only. */
export interface ScoreBreakdown {
	base: number;
	severity: number;
	reach: number;
	confidence: number;
	location: number;
	decay: number;
	score: number;
	steps: ScoreStep[];
}

/** One trip on the wall of shame. */
export interface LeaderboardEntry {
	rank: number;
	agency: string;
	slug: string;
	trip_id: string;
	route: string;
	/** Which vehicle the map draws for this trip; `null` when the mode is unknown. */
	vehicle_type: VehicleType | null;
	headsign: string | null;
	next_stop: string | null;
	vehicle: string | null;
	delay_seconds: number;
	/** The worst this trip got while we could vouch for it — what it's ranked on. */
	peak_delay_seconds: number;
	/** Unix timestamp the trip stopped being reported; null while it's running. */
	ended_at: number | null;
	source: DelaySource;
	/** The receipts: how long we watched, and how late it was when first seen. */
	tracked_seconds: number;
	birth_delay_seconds: number;
	latitude: number | null;
	longitude: number | null;
	score_breakdown?: ScoreBreakdown;
}

/** A leaderboard message: a full board, or the rows/fields that changed. */
export interface LeaderboardMessage {
	seq: number;
	/** The seq this was diffed against. Absent means it's a full. */
	base?: number;
	generated_at: number;
	entries: LeaderboardEntry[];
	debug_enabled?: boolean;
}

/** Health of one polled feed. */
export interface SourceStatus {
	slug: string;
	display_name: string;
	country: string | null;
	state:
		| "active"
		| "requires_auth"
		| "no_realtime"
		| "no_vehicle_positions"
		| "failed";
	failed_status: number | null;
	hot: boolean;
	loading: boolean;
	poll_interval_seconds: number | null;
	/** Unix timestamp of the last poll — *not* an age. See `wire.rs`. */
	last_poll: number | null;
	last_success: boolean | null;
	last_error: string | null;
	vehicles_now: number;
	late_trips: number;
	vetted_out: number;
	peak_vehicles: number;
	total_trips: number | null;
}

/** Aggregate counts across every source. */
export interface StatusSummary {
	total_sources: number;
	active: number;
	requires_auth: number;
	no_realtime: number;
	no_vehicle_positions: number;
	failed: number;
	hot: number;
	loading: number;
	vehicles_now: number;
	static_loaded: number;
	sqlite_bytes: number;
	sqlite_peak_bytes: number;
	process_rss_bytes: number | null;
}

/** A status message: the full report, or the sources/fields that changed. */
export interface StatusMessage {
	seq: number;
	base?: number;
	generated_at: number;
	summary: StatusSummary;
	sources: SourceStatus[];
	/** Slugs that have dropped out of the report entirely. */
	removed?: string[];
}

/** The `/api/shape/{slug}/{trip_id}` response. */
export interface ShapeResponse {
	polyline?: string | null;
}

/** The `/api/debug/capture` response. */
export interface CaptureResponse {
	ok: boolean;
	path?: string;
	error?: string;
}
