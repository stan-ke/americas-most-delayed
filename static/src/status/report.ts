import { createMemo, createSignal, type Accessor } from "solid-js";
import { createStore, produce, reconcile } from "solid-js/store";
import { AMD_API, AMD_WS } from "../lib/api";
import { createSocket } from "../lib/socket";
import type { SourceStatus, StatusMessage, StatusSummary } from "../lib/types";

export interface Report {
	sources: Record<string, SourceStatus>;
	slugs: Accessor<string[]>;
	summary: Accessor<StatusSummary | null>;
	/** The server's clock at the last message — what `age` is derived against. */
	generatedAt: Accessor<number>;
	/** Set while the full report is unreachable. */
	disconnected: Accessor<boolean>;
}

/**
 * The source report, kept up to date from the delta stream.
 *
 * This page used to poll the whole report every 2 seconds. The report is ~176 KB,
 * which is ~7.4 GB a day for one open tab — on a VPS that bills egress, it was the
 * entire hosting bill.
 *
 * So we fetch the report *once* (over HTTP, where it arrives gzipped), and after
 * that the server sends only what changed: the handful of sources whose poll
 * landed in the last two seconds, and within each, only the fields that moved. A
 * tick is a few hundred bytes. See `../server/src/wire.rs` for the protocol; the
 * rules are:
 *   seq <= ours   -> we already have it
 *   base > ours   -> we missed a tick; refetch the full report
 *   otherwise     -> merge it in
 *
 * The sources live in a Solid **store** rather than a signal holding a Map: a tick
 * touches a handful of the ~500 rows, and a store lets those rows update in place
 * without re-rendering (or re-sorting the DOM of) everything else.
 */
export function createReport(): Report {
	const [sources, setSources] = createStore<Record<string, SourceStatus>>({});
	const [summary, setSummary] = createSignal<StatusSummary | null>(null);
	const [generatedAt, setGeneratedAt] = createSignal(0);
	const [disconnected, setDisconnected] = createSignal(false);

	let seq = -1;
	let fetching = false;
	/** Deltas that arrived while the full was in flight. */
	let queued: StatusMessage[] = [];

	const apply = (message: StatusMessage) => {
		// Hold deltas until the full they build on has landed, rather than throwing
		// them away and having to refetch.
		if (fetching && message.base !== undefined) {
			queued.push(message);
			return;
		}
		if (message.seq <= seq) return;
		if (message.base !== undefined && message.base > seq) {
			void loadFull();
			return;
		}
		if (message.base === undefined) {
			// A full replaces everything. `reconcile` diffs it against what we hold so
			// unchanged rows don't churn.
			const next: Record<string, SourceStatus> = {};
			for (const row of message.sources) next[row.slug] = row;
			setSources(reconcile(next));
		} else {
			setSources(
				produce((state) => {
					for (const row of message.sources) {
						const existing = state[row.slug];
						if (existing) Object.assign(existing, row);
						else state[row.slug] = row;
					}
					for (const slug of message.removed ?? []) delete state[slug];
				}),
			);
		}

		seq = message.seq;
		setSummary(message.summary);
		setGeneratedAt(message.generated_at);
		setDisconnected(false);
	};

	const loadFull = async () => {
		if (fetching) return;
		fetching = true;
		try {
			const report = (await fetch(`${AMD_API}/api/status`).then((r) =>
				r.json(),
			)) as StatusMessage;
			fetching = false;
			apply(report);
			// Replay anything that arrived while we were fetching, oldest first.
			const held = queued.sort((a, b) => a.seq - b.seq);
			queued = [];
			for (const message of held) apply(message);
		} catch {
			fetching = false;
			setDisconnected(true);
		}
	};

	createSocket({
		url: `${AMD_WS}/api/status/live`,
		onOpen: () => void loadFull(),
		onMessage: (message) => apply(message as StatusMessage),
		onClose: () => {
			seq = -1; // whatever we resync to next will be a full
		},
	});

	const slugs = createMemo(() => Object.keys(sources));

	return { sources, slugs, summary, generatedAt, disconnected };
}
