/**
 * Decode a Google encoded polyline into `[lat, lon]` pairs.
 *
 * The server sends a route this way rather than as an array of coordinate pairs:
 * a rail shape is thousands of points, and delta-encoding consecutive ones costs
 * ~2 characters apiece instead of ~11 (~9x smaller raw, 3x after gzip).
 */
export function decodePolyline(encoded: string): [number, number][] {
  const points: [number, number][] = [];
  let i = 0;
  let lat = 0;
  let lon = 0;
  while (i < encoded.length) {
    for (const axis of [0, 1]) {
      let result = 0;
      let shift = 0;
      let byte: number;
      do {
        byte = encoded.charCodeAt(i++) - 63;
        result |= (byte & 0x1f) << shift;
        shift += 5;
      } while (byte >= 0x20);
      const delta = result & 1 ? ~(result >> 1) : result >> 1;
      if (axis === 0) lat += delta;
      else lon += delta;
    }
    points.push([lat / 1e5, lon / 1e5]);
  }
  return points;
}
