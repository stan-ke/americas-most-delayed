//! The wire format between the two halves of the deployment: the **static** half
//! (the pages, on GitHub Pages, free) and the **dynamic** half (this server, on a
//! VPS that bills egress). Everything here exists to move fewer bytes across that
//! boundary.
//!
//! Both live streams — the leaderboard and the source-status page — push the same
//! shape of message, because both are the same thing: *a keyed set of rows that
//! mostly doesn't change from one tick to the next*. Re-sending the whole set every
//! tick is what the naive API did, and it was the entire hosting bill: the status
//! page alone was 176 KB every 2 seconds, **~7.4 GB/day for a single open tab**.
//!
//! So a tick sends only what changed, at two levels:
//!
//! - **row level** — a source whose every field is unchanged isn't sent at all;
//! - **field level** — a row that *did* change carries only the fields that differ,
//!   plus the identity fields that name it.
//!
//! The client keeps the last full state and merges each delta into it
//! (`{...old, ...new}`). That is only sound if it can prove it hasn't missed a
//! tick, so every message carries a `seq`, and a delta also carries the `base` seq
//! it was computed against. A message with **no `base` is a full**: it stands alone
//! and replaces everything. The client's rule is three lines:
//!
//! - `seq <= mine` → already have it; ignore. (A full fetched over HTTP can be
//!   *newer* than the first delta that arrives on the socket — this is what makes
//!   that race benign rather than something to lock against.)
//! - `base > mine` → a tick was missed; resync from a full.
//! - otherwise → merge, and `mine = seq`.
//!
//! One subtlety worth keeping: **a field that changes *to* null must travel as an
//! explicit `null`**, or the merge would keep the stale value — a `last_error` that
//! cleared on the next successful poll, a vehicle that lost its GPS fix. A field
//! that is merely null in a row the client has *never seen* is omitted instead:
//! absent and null render identically, and there's no stale value to overwrite.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

/// A sequenced stream of keyed rows that sends only what changed.
///
/// Holds the rows as they were at the current `seq`, which is both what deltas are
/// computed against and what a newly-connected client is served as its full.
pub struct DeltaStream {
    seq: u64,
    /// Message-level fields at the current seq (`generated_at`, `summary`, …).
    head: Map<String, Value>,
    /// The current rows in order, each paired with the identity that names it
    /// across ticks.
    rows: Vec<(String, Value)>,
    /// The JSON key the rows travel under — `entries`, `sources`.
    rows_key: &'static str,
    /// Fields that identify a row. Always echoed in a delta row, so the client can
    /// key the merge.
    id: &'static [&'static str],
    /// Whether the row array *is* the ordering (the leaderboard) rather than just a
    /// set (the status list). When it is, every row must appear in every delta —
    /// an unchanged one shrinks to its identity — because the array's order is
    /// itself information. When it isn't, unchanged rows are dropped entirely and
    /// departures are named in `removed`.
    ordered: bool,
}

impl DeltaStream {
    pub fn new(rows_key: &'static str, id: &'static [&'static str], ordered: bool) -> Self {
        DeltaStream {
            seq: 0,
            head: Map::new(),
            rows: Vec::new(),
            rows_key,
            id,
            ordered,
        }
    }

    /// Take a new tick, returning the delta message to broadcast.
    ///
    /// `rows` are the complete current rows, in order, already serialized.
    pub fn advance(&mut self, head: Map<String, Value>, rows: Vec<Value>) -> String {
        let rows: Vec<(String, Value)> = rows.into_iter().map(|r| (self.key_of(&r), r)).collect();

        let message = {
            let previous: HashMap<&str, &Value> =
                self.rows.iter().map(|(k, v)| (k.as_str(), v)).collect();

            let mut changed = Vec::new();
            for (key, row) in &rows {
                match self.delta_row(previous.get(key.as_str()).copied(), row) {
                    Some(delta) => changed.push(delta),
                    // Unchanged, but its position still matters: send the identity
                    // alone so the client can place it.
                    None if self.ordered => changed.push(self.identity_of(row)),
                    None => {}
                }
            }

            let mut message = head.clone();
            message.insert("seq".into(), json!(self.seq + 1));
            message.insert("base".into(), json!(self.seq));
            message.insert(self.rows_key.into(), Value::Array(changed));

            if !self.ordered {
                let current: HashSet<&str> = rows.iter().map(|(k, _)| k.as_str()).collect();
                let removed: Vec<&str> = previous
                    .keys()
                    .copied()
                    .filter(|key| !current.contains(key))
                    .collect();
                if !removed.is_empty() {
                    message.insert("removed".into(), json!(removed));
                }
            }

            Value::Object(message).to_string()
        };

        self.seq += 1;
        self.head = head;
        self.rows = rows;
        message
    }

    /// The whole current state as one self-contained message — what a client needs
    /// before any delta can mean anything. No `base`, so it replaces rather than
    /// merges.
    pub fn full(&self) -> String {
        let rows = self
            .rows
            .iter()
            .map(|(_, row)| strip_nulls(row.clone()))
            .collect();

        let mut message = self.head.clone();
        message.insert("seq".into(), json!(self.seq));
        message.insert(self.rows_key.into(), Value::Array(rows));
        Value::Object(message).to_string()
    }

    /// The fields of `current` the client doesn't already have. `None` when the row
    /// is unchanged.
    fn delta_row(&self, previous: Option<&Value>, current: &Value) -> Option<Value> {
        let current = current.as_object()?;

        // Never seen: send it whole. Nulls are dropped — there's no stale value
        // underneath to overwrite, and absent renders the same as null.
        let Some(previous) = previous.and_then(Value::as_object) else {
            let mut row = current.clone();
            row.retain(|_, value| !value.is_null());
            return Some(Value::Object(row));
        };

        let mut row: Map<String, Value> = current
            .iter()
            .filter(|(key, value)| previous.get(key.as_str()) != Some(value))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if row.is_empty() {
            return None;
        }

        for key in self.id {
            if let Some(value) = current.get(*key) {
                row.insert((*key).to_string(), value.clone());
            }
        }
        Some(Value::Object(row))
    }

    /// Just the identity fields of a row — an unchanged row's whole contribution to
    /// an ordered delta.
    fn identity_of(&self, row: &Value) -> Value {
        let mut identity = Map::new();
        for key in self.id {
            if let Some(value) = row.get(*key) {
                identity.insert((*key).to_string(), value.clone());
            }
        }
        Value::Object(identity)
    }

    /// The key that names this row across ticks: its identity fields, joined.
    fn key_of(&self, row: &Value) -> String {
        let mut key = String::new();
        for field in self.id {
            if let Some(value) = row.get(*field) {
                key.push('\u{1f}');
                match value {
                    Value::String(s) => key.push_str(s),
                    other => key.push_str(&other.to_string()),
                }
            }
        }
        key
    }
}

/// Drop null fields — worth ~35% of a full status report, which is all nulls for
/// the sources that never failed.
fn strip_nulls(row: Value) -> Value {
    match row {
        Value::Object(mut fields) => {
            fields.retain(|_, value| !value.is_null());
            Value::Object(fields)
        }
        other => other,
    }
}

/// Encode a route path as a [Google encoded polyline] at precision 5 (~1 m).
///
/// Shapes are the largest single thing we serve — a rail route runs to thousands of
/// points, and as a JSON array of `[lat, lon]` pairs that's 15–300 KB. Because the
/// format stores each point as a *delta* from the one before, and consecutive shape
/// points are metres apart, it costs ~2 characters per coordinate instead of ~11:
/// a measured 15,504 → 1,751 bytes (8.9×) on a 652-point Calgary route, and still
/// 3× smaller after gzip has had its turn at both.
///
/// [Google encoded polyline]: https://developers.google.com/maps/documentation/utilities/polylinealgorithm
pub fn encode_polyline(points: &[(f64, f64)]) -> String {
    let mut out = String::with_capacity(points.len() * 6);
    let (mut last_lat, mut last_lon) = (0i64, 0i64);

    for &(lat, lon) in points {
        let lat = (lat * 1e5).round() as i64;
        let lon = (lon * 1e5).round() as i64;
        push_varint(lat - last_lat, &mut out);
        push_varint(lon - last_lon, &mut out);
        (last_lat, last_lon) = (lat, lon);
    }
    out
}

/// One signed offset: zigzag, then base-64 in 5-bit chunks offset into printable
/// ASCII (the polyline format's own variable-length integer).
fn push_varint(value: i64, out: &mut String) {
    let mut value = if value < 0 { !(value << 1) } else { value << 1 };
    while value >= 0x20 {
        out.push((((0x20 | (value & 0x1f)) + 63) as u8) as char);
        value >>= 5;
    }
    out.push(((value + 63) as u8) as char);
}
