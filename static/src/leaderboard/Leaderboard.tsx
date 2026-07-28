import { Show, createEffect, createSignal, onCleanup } from "solid-js";
import { AMD_API } from "../lib/api";
import { fmtDelay, peak, } from "../lib/format";
import type { CaptureResponse } from "../lib/types";
import { Rail } from "./Rail";
import { ScoreDialog } from "./ScoreDialog";
import { TripCard } from "./TripCard";
import { TripMap } from "./TripMap";
import { createBoard } from "./board";
import { createRotation } from "./rotation";
import { createSelection } from "./selection";

/**
 * How long each trip holds the screen. The board itself only refreshes every 15s,
 * so a dwell near that keeps a trip from visibly changing under you mid-view.
 * Overridable for a wall display: `index.html?rotate=20`.
 */
const ROTATE_MS =
	Math.max(
		2,
		Number(new URLSearchParams(location.search).get("rotate")) || 12,
	) * 1000;

export function Leaderboard() {
	const board = createBoard();
	const selection = createSelection(board.entries);
	const rotation = createRotation({
		dwellMs: ROTATE_MS,
		active: () => board.entries().length > 1,
		onAdvance: () => selection.select(selection.index() + 1),
	});

	const [scoreOpen, setScoreOpen] = createSignal(false);
	const [dbgStatus, setDbgStatus] = createSignal("");

	const [localNow, setLocalNow] = createSignal(Date.now());
	const timer = setInterval(() => setLocalNow(Date.now()), 1000);
	onCleanup(() => clearInterval(timer));

	/** A manual move restarts the dwell — the whole point of stepping is to look. */
	const step = (to: number) => {
		selection.select(to);
		rotation.reset();
	};

	// --- the page's global-ish state, still expressed as body classes ------------
	createEffect(() =>
		document.body.classList.toggle("empty", !selection.current()),
	);
	createEffect(() =>
		document.body.classList.toggle(
			"finished",
			selection.current()?.ended_at != null,
		),
	);
	createEffect(() =>
		document.body.classList.toggle("debug-on", board.debugEnabled()),
	);

	// --- keyboard ---------------------------------------------------------------
	const onKeyDown = (ev: KeyboardEvent) => {
		if ((ev.target as Element | null)?.closest?.("dialog")) return;
		if (ev.key === "ArrowLeft") {
			step(selection.index() - 1);
			ev.preventDefault();
		} else if (ev.key === "ArrowRight") {
			step(selection.index() + 1);
			ev.preventDefault();
		} else if (ev.key === " ") {
			rotation.setPaused((p) => !p);
			ev.preventDefault();
		}
	};
	addEventListener("keydown", onKeyDown);
	onCleanup(() => removeEventListener("keydown", onKeyDown));

	// Reading the map means wanting to stay put; don't rotate out from under it.
	let pausedByMap = false;
	const onMapHover = (over: boolean) => {
		if (over) {
			if (!rotation.paused()) {
				pausedByMap = true;
				rotation.setPaused(true);
			}
		} else if (pausedByMap) {
			pausedByMap = false;
			rotation.setPaused(false);
		}
	};

	// --- debug capture (AMD_DEBUG only) -----------------------------------------
	// Prompt for a note, then ask the server to zip up everything behind this entry
	// (config, live GTFS-RT, static GTFS, computed delay) for offline debugging.
	const captureDebug = async () => {
		const e = selection.current();
		if (!e) return;
		const msg = prompt(
			`What's wrong with #${e.rank} ${e.agency} — route ${e.route}, ` +
				`${fmtDelay(peak(e))} late?\n\nDescribe the problem (saved into the archive):`,
			"",
		);
		if (msg === null) return; // cancelled
		setDbgStatus(`capturing #${e.rank} ${e.agency}…`);
		try {
			const res = await fetch(`${AMD_API}/api/debug/capture`, {
				method: "POST",
				headers: { "Content-Type": "application/json" },
				body: JSON.stringify({
					slug: e.slug,
					trip_id: e.trip_id,
					message: msg,
				}),
			});
			const data = (await res.json()) as CaptureResponse;
			setDbgStatus(data.ok ? `✓ saved ${data.path}` : `✗ ${data.error}`);
		} catch (err) {
			setDbgStatus(`✗ capture failed: ${err}`);
		}
	};

	// --- the header line --------------------------------------------------------
	const meta = () => {
		if (board.link() === "down") return "disconnected — reconnecting…";
		if (board.link() === "connecting") return "connecting…";
		const entries = board.entries();
		const generatedMs = board.generatedAt() * 1000;
		const diffSeconds = Math.max(0, (localNow() - generatedMs) / 1000);

		let when: string;
		if (diffSeconds < 60) {
			const secs = Math.floor(diffSeconds);
			when = `${secs} second${secs === 1 ? "" : "s"} ago`;
		} else if (diffSeconds < 600) {
			// 10 minutes
			const mins = Math.floor(diffSeconds / 60);
			when = `${mins} minute${mins === 1 ? "" : "s"} ago`;
		} else {
			when = new Date(generatedMs).toLocaleTimeString();
		}

		return entries.length
			? `updated ${when}`
			: `no delays reported right now - updated ${when}`;
	};

	return (
		<>
			<header>
				<h1>America's Most Delayed</h1>
				<span class="sep">·</span>
				<a href="status.html">source status →</a>
				<span class="dbg">
					<button
						type="button"
						class="dbg-btn"
						title="Show how this entry's rank was calculated"
						onClick={() => setScoreOpen(true)}
					>
						🧮
					</button>
				</span>
				<span class="dbg">
					<button
						type="button"
						class="dbg-btn"
						title="Capture a debug archive for this entry"
						onClick={() => void captureDebug()}
					>
						🐛
					</button>
				</span>
				<span class="dbg" id="dbgstatus">
					{dbgStatus()}
				</span>
				<span id="meta">{meta()}</span>
			</header>

			{/* Rotation progress: a hairline that fills over one dwell, so the next trip
          is never a surprise. */}
			<div id="progress">
				<div style={{ width: `${rotation.progress() * 100}%` }} />
			</div>

			<main>
				<section class="pane" id="detail">
					<Show when={selection.current()}>
						{(entry) => (
							<TripCard
								entry={entry()}
								total={board.entries().length}
								now={board.generatedAt()}
							/>
						)}
					</Show>
				</section>

				<section class="pane" id="mappane">
					<TripMap
						entry={selection.current()}
						now={board.generatedAt()}
						onHover={onMapHover}
					/>
				</section>
			</main>

			<div id="empty">
				{board.link() === "open"
					? "No delays on the board right now."
					: "waiting for the board…"}
			</div>

			<footer>
				<button
					type="button"
					title="Previous trip (←)"
					disabled={board.entries().length < 2}
					onClick={() => step(selection.index() - 1)}
				>
					◀
				</button>
				<button
					type="button"
					id="pause"
					title={
						rotation.paused()
							? "Resume rotation (space)"
							: "Pause rotation (space)"
					}
					onClick={() => rotation.setPaused((p) => !p)}
				>
					{rotation.paused() ? "resume" : "pause"}
				</button>
				<button
					type="button"
					title="Next trip (→)"
					disabled={board.entries().length < 2}
					onClick={() => step(selection.index() + 1)}
				>
					▶
				</button>
				<Rail
					entries={board.entries()}
					index={selection.index()}
					onSelect={(i) => step(i)}
				/>
				<span class="hint">← → to step · space to pause</span>
			</footer>

			<ScoreDialog
				entry={selection.current()}
				open={scoreOpen()}
				onClose={() => setScoreOpen(false)}
			/>
		</>
	);
}
