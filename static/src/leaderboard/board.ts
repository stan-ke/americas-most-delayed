import { createSignal, type Accessor } from "solid-js";
import { AMD_WS } from "../lib/api";
import { tripKey } from "../lib/format";
import { createSocket } from "../lib/socket";
import type { LeaderboardEntry, LeaderboardMessage } from "../lib/types";

/** Whether we have a live stream, and whether we ever did. */
export type Link = "connecting" | "open" | "down";

export interface Board {
	entries: Accessor<LeaderboardEntry[]>;
	/** The server's clock at the last message — what finished trips are aged against. */
	generatedAt: Accessor<number>;
	debugEnabled: Accessor<boolean>;
	link: Accessor<Link>;
}

/**
 * The board, kept up to date from the delta stream.
 *
 * The server doesn't re-send the board every 15s; it sends what changed, and
 * within a changed entry only the fields that changed. We hold the last full board
 * and merge each message into it. See `../server/src/wire.rs` for the protocol.
 */
export function createBoard(): Board {
	const [entries, setEntries] = createSignal<LeaderboardEntry[]>([]);
	const [generatedAt, setGeneratedAt] = createSignal(
		Math.floor(Date.now() / 1000),
	);
	const [debugEnabled, setDebugEnabled] = createSignal(false);
	const [link, setLink] = createSignal<Link>("connecting");

	/** Trip identity -> the entry as we last knew it. */
	let known = new Map<string, LeaderboardEntry>();
	/** The last message we've applied. */
	let seq = -1;

	const apply = (message: LeaderboardMessage, socket: WebSocket) => {
		// Already have it — a delta can arrive that we're not behind on.
		if (message.seq <= seq) return;
		// A tick went missing, so a merge would silently keep stale fields. Drop the
		// socket and reconnect into a fresh full.
		if (message.base !== undefined && message.base > seq) {
			socket.close();
			return;
		}
		// No `base` means a full: it replaces everything rather than merging.
		if (message.base === undefined) known = new Map();

		const board = message.entries.map((row) => {
			const merged = { ...known.get(tripKey(row)), ...row } as LeaderboardEntry;
			known.set(tripKey(row), merged);
			return merged;
		});
		// The message carries the whole board in rank order, so anything not in it has
		// fallen off and can be forgotten.
		known = new Map(board.map((e) => [tripKey(e), e]));
		seq = message.seq;

		setEntries(board);
		setGeneratedAt(message.generated_at);
		setDebugEnabled(!!message.debug_enabled);
		setLink("open");
	};

	createSocket({
		url: `${AMD_WS}/api/subscribe`,
		onMessage: (message, socket) =>
			apply(message as LeaderboardMessage, socket),
		onClose: () => {
			setLink("down");
			seq = -1; // whatever comes back will be a full
		},
	});

	return { entries, generatedAt, debugEnabled, link };
}
