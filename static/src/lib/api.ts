// Where the dynamic half lives.
//
// These pages are static and free to host (GitHub Pages); everything live comes
// from the server on a VPS, which bills egress. That split is the whole point of
// the deployment, and this file is the seam: the one thing that has to change when
// the API moves.
//
// It's a build-time variable now rather than a `config.js` the deploy rewrites in
// place — set `VITE_AMD_API` when running `vite build` (the Pages workflow does).
// Must be https: these pages are served over https, and a browser refuses to let
// an https page call a plain-http API (mixed content).

const configured = (import.meta.env.VITE_AMD_API ?? "").trim();

/** Hostnames that mean "a local checkout", including `file://` (empty host). */
const LOCAL_HOSTS = ["localhost", "127.0.0.1", "[::1]", ""];

/**
 * The API origin. An explicit build-time value always wins; otherwise a page
 * served from localhost talks to a local `cargo run`, and anything else falls
 * back to the deployed API — so an un-configured build still works in both places.
 */
export const AMD_API =
	configured ||
	(LOCAL_HOSTS.includes(location.hostname)
		? "http://localhost:8080"
		: "https://amd-api.stan.ke");

/** The websocket origin is the same host, one scheme over. */
export const AMD_WS = AMD_API.replace(/^http/, "ws");
