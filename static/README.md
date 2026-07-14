# America's Most Delayed — the static half

The two pages, and nothing else. They hold no data of their own: everything live
comes from the server in `../server`, over the API in `config.js`.

The split exists to keep the bill down. Static hosting is free (GitHub Pages) and
serves the bytes that never change — HTML, CSS, the JS. The VPS is what costs
money, and what it bills for is **egress**, so it serves only what can't be
precomputed: the live leaderboard, the source health, a route shape.

## Deploying

1. Point `window.AMD_API` in `config.js` at the server.
2. Publish this directory to GitHub Pages (repo settings → Pages → the branch and
   folder this lives in).
3. Run the server on the VPS. It listens on `:8080` and serves `/api/*` only.

**The API must be https.** These pages are served over https, and a browser will
refuse to let an https page call a plain-http API (mixed content) — the board will
simply never populate. Put the server behind a TLS terminator (Caddy will do it
with a one-line config and get the certificate itself).

The server allows any origin (`CorsLayer::allow_origin(Any)`), so nothing needs to
change server-side when the Pages URL does. Nothing it serves is private.

## Working on the pages locally

`config.js` points itself at `http://localhost:8080` when the page is served from
localhost (or opened from `file://`), so `cargo run` in `../server` plus any static
file server is the whole loop:

```sh
cd ../server && cargo run          # the API
python3 -m http.server 3000        # here; then open http://localhost:3000
```

There's no build step. The pages are plain HTML + vanilla JS, and the only external
dependency is Leaflet from a CDN (on the leaderboard page, for the map).

## How the pages talk to the server

Both pages hold a local copy of the state and merge **deltas** into it, rather than
re-fetching everything on a timer — see the protocol notes in `../server/src/wire.rs`.
It matters most on the status page: re-fetching its report every 2s was 176 KB a
tick, about **7.4 GB/day for a single open tab**. The delta is a few hundred bytes.

- `GET /api/status` — the full report. Fetched **once**, on load. Gzipped in flight.
- `WS /api/status/live` — what changed, every 2s.
- `WS /api/subscribe` — the leaderboard: a full board on connect, deltas every 15s.
- `GET /api/shape/{slug}/{trip_id}` — a trip's route path, as an encoded polyline
  (~9× smaller than coordinate pairs), cached by the browser for a day.
