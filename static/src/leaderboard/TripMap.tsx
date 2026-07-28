import L from "leaflet";
import { Show, createEffect, createSignal, onCleanup, onMount } from "solid-js";
import { AMD_API } from "../lib/api";
import { endedAgo, fmtDelay, peak, tripKey } from "../lib/format";
import { decodePolyline } from "../lib/polyline";
import type { LeaderboardEntry, ShapeResponse } from "../lib/types";

const busIcon = L.divIcon({
	html: "🚌",
	className: "bus-icon",
	iconSize: [28, 28],
	iconAnchor: [14, 14],
	popupAnchor: [0, -14],
});

/**
 * The selected vehicle on a map, with its scheduled route drawn behind it.
 *
 * Leaflet owns its own DOM, so this is the one imperative corner of the page: the
 * map is created once on mount and an effect pushes the current trip into it.
 */
export function TripMap(props: {
	entry: LeaderboardEntry | null;
	/** The server's clock, so a finished trip's position reads as "last seen at". */
	now: number;
	/** Reading the map means wanting to stay put; the page pauses while hovered. */
	onHover: (over: boolean) => void;
}) {
	let container!: HTMLDivElement;
	let map!: L.Map;
	let marker: L.Marker | null = null;
	let routeLine: L.Polyline | null = null;
	/** The trip whose route we've drawn, so the shape is refetched only on a real
      change of selection — not on every 15s snapshot. */
	let shapeKey: string | null = null;

	const [routeDrawn, setRouteDrawn] = createSignal(false);

	/**
	 * Positions are only fetched for feeds hot enough to be near the top, so a
	 * lower-ranked trip legitimately has nowhere to put on a map.
	 */
	const placed = () => {
		const e = props.entry;
		return !!e && e.latitude != null && e.longitude != null;
	};

	const caption = () => {
		const e = props.entry;
		if (!e || e.latitude == null || e.longitude == null) return " ";
		// For a finished trip the position is frozen at its last sighting, so say so
		// rather than implying the vehicle is sitting there now.
		const where =
			endedAgo(e, props.now) === null ? "Vehicle at" : "Last seen at";
		return (
			`${where} ${e.latitude.toFixed(4)}, ${e.longitude.toFixed(4)}` +
			(routeDrawn() ? " · red line is this trip's scheduled path" : "")
		);
	};

	onMount(() => {
		map = L.map(container).setView([39.5, -98.35], 4); // continental US
		L.tileLayer("https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png", {
			maxZoom: 21,
			attribution: "&copy; OpenStreetMap contributors",
		}).addTo(map);

		const onResize = () => map.invalidateSize(false);
		addEventListener("resize", onResize);
		onCleanup(() => {
			removeEventListener("resize", onResize);
			map.remove();
		});
	});

	/** Drop the marker + route line, so a stale overlay from the previous trip isn't
      left sitting under the next one. */
	const clearOverlays = () => {
		if (routeLine) {
			map.removeLayer(routeLine);
			routeLine = null;
		}
		if (marker) {
			map.removeLayer(marker);
			marker = null;
		}
		shapeKey = null;
		setRouteDrawn(false);
	};

	/**
	 * Fetch and draw the route path for the selected trip, on demand. The backend
	 * reads just this trip's shape from the static feed and drops it, and the
	 * response is cached by the browser for a day, so rotating back around to a trip
	 * costs nothing after the first look.
	 */
	const drawRoute = async (entry: LeaderboardEntry) => {
		const k = tripKey(entry);
		if (k === shapeKey) return; // same trip — keep the line we already have
		shapeKey = k;
		if (routeLine) {
			map.removeLayer(routeLine);
			routeLine = null;
		}
		setRouteDrawn(false);
		if (!entry.slug || !entry.trip_id) return;
		try {
			const path =
				encodeURIComponent(entry.slug) +
				"/" +
				encodeURIComponent(entry.trip_id);
			const res = await fetch(`${AMD_API}/api/shape/${path}`);
			const { polyline } = (await res.json()) as ShapeResponse;
			const points = decodePolyline(polyline || "");
			// Guard against the rotation having moved on while we awaited.
			if (shapeKey !== k || points.length < 2) return;
			// The line goes in Leaflet's overlay pane and the bus marker in the marker
			// pane above it, so the vehicle stays on top of its own route for free.
			routeLine = L.polyline(points, {
				color: "#d33",
				weight: 4,
				opacity: 0.7,
			}).addTo(map);
			map.fitBounds(routeLine.getBounds(), { padding: [30, 30] });
			setRouteDrawn(true);
		} catch {
			/* leave the map with just the marker */
		}
	};

	/** Agency, route and headsign come from third-party catalogs and feeds, so the
      popup is built as DOM rather than trusted into `innerHTML`. */
	const popupFor = (e: LeaderboardEntry) => {
		const el = document.createElement("div");
		const title = document.createElement("b");
		title.textContent = `#${e.rank} ${e.agency}`;
		el.append(
			title,
			document.createElement("br"),
			`route ${e.route}${e.headsign ? ` → ${e.headsign}` : ""}`,
			document.createElement("br"),
			`${fmtDelay(peak(e))} late`,
		);
		return el;
	};

	createEffect(() => {
		const e = props.entry;
		if (!e || e.latitude == null || e.longitude == null) {
			clearOverlays();
			return;
		}
		// After the DOM has settled — the map div may have just been un-hidden, and
		// Leaflet has to be told its size changed.
		requestAnimationFrame(() => map.invalidateSize(false));

		void drawRoute(e);

		const ll: L.LatLngExpression = [e.latitude, e.longitude];
		if (!marker) marker = L.marker(ll, { icon: busIcon }).addTo(map);
		else marker.setLatLng(ll);
		marker.bindPopup(popupFor(e));
		// Only recenter on the vehicle when we haven't fitted the map to a route.
		if (!routeLine) map.setView(ll, 12);
	});

	return (
		<>
			<div
				id="map"
				ref={container}
				classList={{ hidden: !placed() }}
				onPointerEnter={() => props.onHover(true)}
				onPointerLeave={() => props.onHover(false)}
			/>
			<div id="nomap" classList={{ on: !placed() }}>
				<Show when={props.entry}>
					No live location for this vehicle — its agency's position feed hasn't
					placed it.
				</Show>
			</div>
			<p id="mapcap">{caption()}</p>
		</>
	);
}
