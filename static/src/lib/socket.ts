import { onCleanup } from "solid-js";

/**
 * A websocket that reconnects, scoped to the calling component's lifetime.
 *
 * Both live streams are deltas over a retained state, so a dropped connection is
 * not a cosmetic problem: whatever comes back has to be a **full**. That's why
 * `onClose` fires on every drop — the caller resets its `seq` there, and the
 * server sends a full to a client that connects fresh.
 */
export function createSocket(opts: {
	url: string;
	onMessage: (message: unknown, socket: WebSocket) => void;
	onOpen?: (socket: WebSocket) => void;
	onClose?: () => void;
	retryMs?: number;
}): void {
	const retryMs = opts.retryMs ?? 2000;
	let socket: WebSocket | null = null;
	let retry: ReturnType<typeof setTimeout> | undefined;
	let disposed = false;

	const connect = () => {
		if (disposed) return;
		socket = new WebSocket(opts.url);
		socket.onopen = () => opts.onOpen?.(socket!);
		socket.onmessage = (ev) => opts.onMessage(JSON.parse(ev.data), socket!);
		socket.onclose = () => {
			opts.onClose?.();
			if (!disposed) retry = setTimeout(connect, retryMs);
		};
	};
	connect();

	onCleanup(() => {
		disposed = true;
		clearTimeout(retry);
		// Drop the handler first: a close we asked for shouldn't schedule a retry.
		if (socket) socket.onclose = null;
		socket?.close();
	});
}
