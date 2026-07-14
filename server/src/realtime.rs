//! Fetching and decoding GTFS-realtime feeds.

use anyhow::{Context, Result};
use gtfs_rt::FeedMessage;
use prost::Message;
use reqwest::Client;

/// Fetch and decode a GTFS-realtime feed (trip updates or vehicle positions —
/// both are `FeedMessage`s, distinguished by which entity fields are populated).
pub async fn fetch_feed(client: &Client, url: &str) -> Result<FeedMessage> {
    let bytes = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await
        .context("reading realtime feed body")?;

    FeedMessage::decode(bytes).context("decoding GTFS-realtime protobuf")
}

/// Fetch a GTFS-realtime feed's raw protobuf bytes *without* decoding — used by
/// the debug capture, which archives the exact bytes seen on the wire so a report
/// can be re-decoded offline long after the live feed has moved on.
pub async fn fetch_bytes(client: &Client, url: &str) -> Result<Vec<u8>> {
    Ok(client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await
        .context("reading realtime feed body")?
        .to_vec())
}
