# America's Most Delayed — the static half

The two pages, and nothing else. They hold no data of their own: everything live
comes from the server in `../server`, over the API origin baked in at build time.

The split exists to keep the bill down. Static hosting is free (GitHub Pages) and
serves the bytes that never change — HTML, CSS, the JS. The VPS is what costs
money, and what it bills for is **egress**, so it serves only what can't be
precomputed: the live leaderboard, the source health, a route shape.

## Stack

**SolidJS + Vite + TypeScript.** The pages are two Vite entries (`index.html`,
`status.html`) sharing everything under `src/lib/`:

```
src/lib/          the wire types, the API origin, the formatters, the socket
src/leaderboard/  the rotating one-trip-at-a-time board
src/status/       the LED grid + sortable source table
```

Solid is a fit for what these pages actually do: both hold a local copy of server
state and merge **deltas** into it (see below), so the reactive primitives map
straight onto the protocol — a tick updates the handful of rows that changed and
nothing else re-renders. The status page keeps its ~500 sources in a Solid
**store** for exactly that reason: a two-second tick touches a few rows, and the
other 495 dots and table cells are left alone.

Leaflet is an npm dependency and bundled, rather than pulled from a CDN — bytes
served from Pages are free, so there's no reason to make a visitor round-trip to
someone else. The OpenStreetMap **tiles** are still fetched from OSM at runtime
(on the leaderboard page only); that's the one external resource left.

## Working on the pages locally

```sh
cd ../server && cargo run    # the API on :8080
npm install                  # here, once
npm run dev                  # http://localhost:3000, with hot reload
```

`npm run dev` needs no configuration: a page served from localhost talks to
`http://localhost:8080` unless `VITE_AMD_API` says otherwise.

- `npm run build` — production build into `dist/`
- `npm run preview` — serve the built `dist/` (what CI ships)
- `npm run typecheck` — `tsc --noEmit`; CI runs this before it builds

## Deploying

`.github/workflows/static.yml` does all of it on a push to `main`: `npm ci`,
typecheck, `npm run build` with `VITE_AMD_API` set to the API origin, then upload
`static/dist/` to Pages.

To point the pages at a different server, change `VITE_AMD_API` in that workflow.
It's a build-time variable (`src/lib/api.ts`), not a file rewritten after the fact.
An un-configured build still works: it falls back to `http://localhost:8080` from
localhost, and to the deployed API anywhere else.

**The API must be https.** These pages are served over https, and a browser will
refuse to let an https page call a plain-http API (mixed content) — the board will
simply never populate. Put the server behind a TLS terminator (Caddy will do it
with a one-line config and get the certificate itself).

The server allows any origin (`CorsLayer::allow_origin(Any)`), so nothing needs to
change server-side when the Pages URL does. Nothing it serves is private. Asset
URLs are relative (`base: "./"` in `vite.config.ts`), so the same build works from
a custom domain at the root or a repo-scoped `user.github.io/repo/` URL.

There's also a `Dockerfile` here (build → nginx) used by the repo's
`docker-compose.yml` for the local stack. Production doesn't use it.

## The leaderboard page

`index.html` shows **one trip at a time, full screen**, and rotates to the next every
12 seconds. Everything about an entry is on the card — the delay set large, who it
belongs to, where it is, and the receipts for why the number is believable — so it
reads from across a room as well as up close.

`?rotate=<seconds>` sets the dwell for a load, which is what a wall display wants:

```
https://…/index.html?rotate=25
```

Rotation pauses while the pointer is over the map, on `space`, and whenever the tab
is hidden (so a backgrounded screen doesn't race through the board and come back
somewhere arbitrary). `←`/`→` step; the numbered chips along the bottom jump.

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

The merge rules live in `src/leaderboard/board.ts` and `src/status/report.ts`, and
the payload shapes in `src/lib/types.ts` — keep those in step with the Rust structs
in `../server/src/scheduler.rs`.
