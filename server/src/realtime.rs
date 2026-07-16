//! Fetching and decoding GTFS-realtime feeds.

use anyhow::{Context, Result};
use gtfs_rt::FeedMessage;
use prost::{Message, bytes::Bytes};
use reqwest::Client;

<<<<<<< Updated upstream
use crate::auth::FeedAuth;

/// GET a URL and return its body bytes, erroring on a non-success status. Any
/// per-host credential (see [`crate::auth`]) is injected before the request is
/// sent, so a gated feed authenticates transparently.
async fn get_bytes(client: &Client, auth: &FeedAuth, url: &str) -> Result<Bytes> {
    auth.apply(client, url)
=======
/// GET a URL and return its body bytes, erroring on a non-success status.
async fn get_bytes(client: &Client, url: &str) -> Result<Bytes> {
    client
        .get(url)
>>>>>>> Stashed changes
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await
        .context("reading realtime feed body")
}

/// Fetch and decode a GTFS-realtime feed (trip updates or vehicle positions —
/// both are `FeedMessage`s, distinguished by which entity fields are populated).
<<<<<<< Updated upstream
pub async fn fetch_feed(client: &Client, auth: &FeedAuth, url: &str) -> Result<FeedMessage> {
    FeedMessage::decode(get_bytes(client, auth, url).await?)
        .context("decoding GTFS-realtime protobuf")
=======
pub async fn fetch_feed(client: &Client, url: &str) -> Result<FeedMessage> {
    FeedMessage::decode(get_bytes(client, url).await?).context("decoding GTFS-realtime protobuf")
>>>>>>> Stashed changes
}

/// Fetch a GTFS-realtime feed's raw protobuf bytes *without* decoding — used by
/// the debug capture, which archives the exact bytes seen on the wire so a report
/// can be re-decoded offline long after the live feed has moved on.
<<<<<<< Updated upstream
pub async fn fetch_bytes(client: &Client, auth: &FeedAuth, url: &str) -> Result<Vec<u8>> {
    Ok(get_bytes(client, auth, url).await?.to_vec())
=======
pub async fn fetch_bytes(client: &Client, url: &str) -> Result<Vec<u8>> {
    Ok(get_bytes(client, url).await?.to_vec())
>>>>>>> Stashed changes
}
