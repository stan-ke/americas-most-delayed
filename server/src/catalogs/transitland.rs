//! Transitland Atlas catalog provider.
//!
//! Transitland publishes its catalog as **DMFR** (Distributed Mobility Feed
//! Registry) JSON files in a GitHub repo
//! (<https://github.com/transitland/transitland-atlas>). We download the repo zip,
//! read every `feeds/*.dmfr.json`, and turn it into [`AgencyConfig`]s.
//!
//! DMFR links feeds through **operators** (rather than MobilityData's
//! `static_reference` column): an operator's `associated_feeds` — explicit at the
//! top level, or implicit when the operator is nested inside a feed — point at a
//! static feed (`static_current`) and/or a realtime feed (`realtime_trip_updates`).
//! Each operator becomes one or more configs: a **pollable** one when static and
//! realtime pair up (deduped on the realtime URL; a multi-modal operator is
//! [`decompose`]d into one config per mode, e.g. SEPTA *(bus)* + *(rail)*), or a
//! **static-only** `no_realtime` config when it has a static feed but no realtime,
//! so a big agency we lack realtime for still surfaces in `/status`.

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read};

use anyhow::{Context, Result};
use reverse_geocoder::ReverseGeocoder;
use serde::Deserialize;

use crate::agency::{AgencyConfig, GeoPoint, GtfsRtUrls};
use crate::catalogs::catalog::GtfsCatalogProvider;

/// GitHub's zip export of the Transitland Atlas repo's default branch. `reqwest`
/// follows the redirect to codeload transparently.
const ATLAS_ZIP_URL: &str =
    "https://codeload.github.com/transitland/transitland-atlas/zip/refs/heads/main";

/// One DMFR file: a set of feeds plus optional top-level operators.
#[derive(Debug, Deserialize)]
struct Dmfr {
    #[serde(default)]
    feeds: Vec<Feed>,
    #[serde(default)]
    operators: Vec<Operator>,
}

/// One feed entry — a single data source (static GTFS, GTFS-realtime, GBFS, …).
#[derive(Debug, Deserialize)]
struct Feed {
    id: String,
    /// `gtfs`, `gtfs-rt`, `gbfs`, … We only care about the first two.
    #[serde(default)]
    spec: String,
    #[serde(default)]
    urls: Urls,
    authorization: Option<Authorization>,
    #[serde(default)]
    tags: Tags,
    /// Operators nested inside the feed they draw from; each is implicitly
    /// associated with this feed even without an explicit `associated_feeds`.
    #[serde(default)]
    operators: Vec<Operator>,
}

impl Feed {
    /// Whether this feed is worth using — i.e. not flagged stale. DMFR rarely
    /// carries a status, but a handful are marked outdated/archived/unpublished.
    fn is_usable(&self) -> bool {
        !matches!(
            self.tags.status.as_deref(),
            Some("outdated" | "archived" | "unpublished")
        )
    }
}

/// A feed's `tags` — a free-form string map; we read only `status`.
#[derive(Debug, Default, Deserialize)]
struct Tags {
    status: Option<String>,
}

/// The subset of a feed's URLs we use: the static schedule and the realtime
/// trip-updates and vehicle-positions endpoints.
#[derive(Debug, Default, Deserialize)]
struct Urls {
    static_current: Option<String>,
    realtime_trip_updates: Option<String>,
    realtime_vehicle_positions: Option<String>,
}

/// A feed's authorization block; its mere presence (with a real type) means the
/// feed is gated behind credentials we don't have.
#[derive(Debug, Deserialize)]
struct Authorization {
    #[serde(rename = "type", default)]
    auth_type: String,
}

/// An operator: a transit agency that draws from one or more feeds.
#[derive(Debug, Deserialize)]
struct Operator {
    onestop_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    short_name: Option<String>,
    #[serde(default)]
    associated_feeds: Vec<AssociatedFeed>,
}

/// One entry in an operator's `associated_feeds` list.
#[derive(Debug, Deserialize)]
struct AssociatedFeed {
    feed_onestop_id: Option<String>,
}

pub struct TransitlandProvider {
    agencies: Vec<AgencyConfig>,
}

impl TransitlandProvider {
    /// Download the Atlas zip and parse it into agency configs.
    pub async fn new() -> Result<Self> {
        let bytes = reqwest::get(ATLAS_ZIP_URL)
            .await
            .context("fetching Transitland Atlas zip")?
            .error_for_status()
            .context("Transitland Atlas zip request failed")?
            .bytes()
            .await
            .context("reading Transitland Atlas zip body")?;

        // Unzipping and JSON parsing are CPU-bound; keep them off the runtime.
        let agencies = tokio::task::spawn_blocking(move || parse_atlas(&bytes)).await??;

        let pollable = agencies
            .iter()
            .filter(|a| !a.realtime_urls.trip_updates_url.is_empty())
            .count();
        println!(
            "Transitland Atlas: {} agencies ({pollable} pollable, {} static-only)",
            agencies.len(),
            agencies.len() - pollable,
        );
        Ok(TransitlandProvider { agencies })
    }
}

impl GtfsCatalogProvider for TransitlandProvider {
    fn get_agencies(&self) -> Vec<AgencyConfig> {
        self.agencies.clone()
    }
}

/// Parse the Atlas zip into one [`AgencyConfig`] per operator: pollable ones that
/// pair a static and a realtime feed, then static-only ones that have a static
/// feed but no realtime.
fn parse_atlas(zip_bytes: &[u8]) -> Result<Vec<AgencyConfig>> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(zip_bytes)).context("opening Transitland Atlas zip")?;

    // Index every feed by its Onestop ID, and collect every operator alongside
    // the feed it was nested in (`None` for top-level operators).
    let mut feeds: HashMap<String, Feed> = HashMap::new();
    let mut operators: Vec<(Operator, Option<String>)> = Vec::new();

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_string();
        // The repo zips to `transitland-atlas-<branch>/feeds/<domain>.dmfr.json`.
        if !(name.contains("/feeds/") && name.ends_with(".dmfr.json")) {
            continue;
        }

        let mut text = String::new();
        if let Err(err) = entry.read_to_string(&mut text) {
            eprintln!("[transitland] skipping {name}: {err}");
            continue;
        }
        let dmfr: Dmfr = match serde_json::from_str(&text) {
            Ok(dmfr) => dmfr,
            Err(err) => {
                eprintln!("[transitland] skipping {name}: {err}");
                continue;
            }
        };

        for op in dmfr.operators {
            operators.push((op, None));
        }
        for mut feed in dmfr.feeds {
            for op in std::mem::take(&mut feed.operators) {
                operators.push((op, Some(feed.id.clone())));
            }
            feeds.insert(feed.id.clone(), feed);
        }
    }

    // One geocoder (loads an embedded place database and builds a k-d tree) shared
    // across every operator's country lookup.
    let geocoder = ReverseGeocoder::new();

    let mut agencies = Vec::new();
    let mut seen_realtime: HashSet<String> = HashSet::new();
    // Static feed URLs already turned into a config, so we don't emit a
    // static-only entry for a feed a pollable operator already covers, nor two
    // static-only entries for the same (e.g. regional) static feed.
    let mut used_static: HashSet<String> = HashSet::new();

    // Pass 1 — pollable operators. An operator is split into one config per
    // **mode group** of its realtime feeds (subway / bus / rail…), so a
    // multi-modal agency like SEPTA (bus + rail) or MTA (subway + bus) surfaces as
    // separate entries rather than collapsing to whichever feed happened to be
    // listed first. Dedup on the realtime URLs so a feed shared by many operators
    // is polled once.
    for (op, nested_in) in &operators {
        let groups = decompose(op, nested_in.as_deref(), &feeds);
        let multi = groups.len() > 1;
        for group in &groups {
            // Skip a group whose realtime feeds another operator already claimed.
            let mut any_new = false;
            for url in &group.trip_updates {
                any_new |= seen_realtime.insert(url.clone());
            }
            if !any_new {
                continue;
            }
            used_static.insert(group.static_url.clone());
            // Only disambiguate the name/slug when the operator actually split.
            let label = multi.then_some(group.label.as_str());
            agencies.push(build_config(
                op,
                group.static_url.clone(),
                label,
                Some(group),
                &geocoder,
            ));
        }
    }

    // Pass 2 — static-only operators (a static feed, *no* realtime at all),
    // surfaced as `no_realtime`. Skip any whose static feed a pollable config
    // already claimed (pass 1), or that an earlier static-only config already used.
    for (op, nested_in) in &operators {
        if has_realtime(op, nested_in.as_deref(), &feeds) {
            continue;
        }
        let Some(static_url) = first_static(op, nested_in.as_deref(), &feeds) else {
            continue;
        };
        if !used_static.insert(static_url.clone()) {
            continue;
        }
        agencies.push(build_config(op, static_url, None, None, &geocoder));
    }

    Ok(agencies)
}

/// Transit modes we recognise in a feed id, most specific first, used to group an
/// operator's realtime feeds and to pair each group with a matching static feed.
const MODE_KEYWORDS: &[&str] = &[
    "lightrail",
    "streetcar",
    "subway",
    "trolley",
    "ferry",
    "metro",
    "rail",
    "tram",
    "bus",
];

/// The transit mode a feed id names, if any — the first [`MODE_KEYWORDS`] entry
/// that appears in it (`f-mta~nyc~rt~subway~…` → `subway`, `…~bustime` → `bus`).
fn mode_key(feed_id: &str) -> Option<&'static str> {
    let id = feed_id.to_ascii_lowercase();
    MODE_KEYWORDS.iter().copied().find(|kw| id.contains(*kw))
}

/// The feeds an operator draws from: explicit `associated_feeds` plus the feed it
/// was nested in (if any), resolved against the feed index.
fn associated_feeds<'a>(
    op: &Operator,
    nested_in: Option<&str>,
    feeds: &'a HashMap<String, Feed>,
) -> Vec<&'a Feed> {
    op.associated_feeds
        .iter()
        .filter_map(|a| a.feed_onestop_id.as_deref())
        .chain(nested_in)
        .filter_map(|id| feeds.get(id))
        .collect()
}

/// Whether an operator has any pollable realtime trip-updates feed.
fn has_realtime(op: &Operator, nested_in: Option<&str>, feeds: &HashMap<String, Feed>) -> bool {
    associated_feeds(op, nested_in, feeds)
        .iter()
        .any(|f| f.spec == "gtfs-rt" && f.urls.realtime_trip_updates.is_some())
}

/// An operator's first usable static feed URL (for static-only entries).
fn first_static(
    op: &Operator,
    nested_in: Option<&str>,
    feeds: &HashMap<String, Feed>,
) -> Option<String> {
    associated_feeds(op, nested_in, feeds)
        .iter()
        .find(|f| f.spec == "gtfs" && f.is_usable() && f.urls.static_current.is_some())
        .and_then(|f| f.urls.static_current.clone())
}

/// One pollable slice of an operator: a static feed paired with the realtime feeds
/// of a single mode, already split so only feeds we can actually poll are listed
/// (or, if all are auth-gated, the auth-gated ones — the config then surfaces as
/// `requires_auth`).
struct Group {
    /// Mode label for disambiguating the name/slug when an operator splits.
    label: String,
    static_url: String,
    trip_updates: Vec<String>,
    vehicle_positions: Vec<String>,
    requires_auth: bool,
}

/// Break an operator into one [`Group`] per realtime mode, pairing each with the
/// static feed whose id best matches that mode (a static is claimed by at most one
/// group; ties and unmatched modes fall back to the first free static). Realtime
/// feeds with no recognisable mode collapse into one default group. Returns empty
/// when the operator has no static or no realtime feed.
fn decompose(op: &Operator, nested_in: Option<&str>, feeds: &HashMap<String, Feed>) -> Vec<Group> {
    let associated = associated_feeds(op, nested_in, feeds);
    let statics: Vec<&Feed> = associated
        .iter()
        .copied()
        .filter(|f| f.spec == "gtfs" && f.is_usable() && f.urls.static_current.is_some())
        .collect();
    let realtimes: Vec<&Feed> = associated
        .iter()
        .copied()
        .filter(|f| f.spec == "gtfs-rt" && f.urls.realtime_trip_updates.is_some())
        .collect();
    if statics.is_empty() || realtimes.is_empty() {
        return Vec::new();
    }

    // Group the realtime feeds by mode, preserving first-seen order.
    let mut mode_groups: Vec<(Option<&'static str>, Vec<&Feed>)> = Vec::new();
    for rt in realtimes {
        let key = mode_key(&rt.id);
        match mode_groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, group)) => group.push(rt),
            None => mode_groups.push((key, vec![rt])),
        }
    }

    let mut claimed: HashSet<usize> = HashSet::new();
    let mut groups = Vec::new();
    for (key, rt_feeds) in &mode_groups {
        let key = *key;
        let static_idx = pick_static(&statics, key, &claimed);
        claimed.insert(static_idx);
        let static_feed = statics[static_idx];

        // Prefer the feeds we can actually poll; only if a mode is *entirely*
        // auth-gated (e.g. MTA buses via BusTime) do we keep the auth feeds so it
        // surfaces as `requires_auth` rather than vanishing.
        let mut open: Vec<&Feed> = Vec::new();
        let mut auth: Vec<&Feed> = Vec::new();
        for &feed in rt_feeds {
            if requires_auth(feed) {
                auth.push(feed);
            } else {
                open.push(feed);
            }
        }
        let requires_auth = open.is_empty();
        let use_feeds = if open.is_empty() { &auth } else { &open };

        groups.push(Group {
            label: key
                .map(str::to_string)
                .unwrap_or_else(|| static_mode_token(&static_feed.id)),
            static_url: static_feed.urls.static_current.clone().unwrap(),
            trip_updates: use_feeds
                .iter()
                .filter_map(|f| f.urls.realtime_trip_updates.clone())
                .collect(),
            vehicle_positions: use_feeds
                .iter()
                .filter_map(|f| f.urls.realtime_vehicle_positions.clone())
                .collect(),
            requires_auth,
        });
    }
    groups
}

/// Index of the static feed to pair with a mode group: the unclaimed static with
/// the best mode affinity (first on ties), falling back to the best-affinity
/// static overall when every one is already claimed.
fn pick_static(statics: &[&Feed], key: Option<&str>, claimed: &HashSet<usize>) -> usize {
    let affinity = |i: usize| match key {
        Some(kw) if statics[i].id.to_ascii_lowercase().contains(kw) => 1usize,
        _ => 0,
    };
    let best = |allowed: &dyn Fn(usize) -> bool| -> Option<usize> {
        (0..statics.len())
            .filter(|i| allowed(*i))
            .max_by(|a, b| affinity(*a).cmp(&affinity(*b)).then(b.cmp(a)))
    };
    best(&|i| !claimed.contains(&i))
        .or_else(|| best(&|_| true))
        .unwrap_or(0)
}

/// A short, stable label for a static feed: its mode keyword if any, else its last
/// id segment — so an operator's sibling statics still get distinct labels.
fn static_mode_token(feed_id: &str) -> String {
    mode_key(feed_id).map(str::to_string).unwrap_or_else(|| {
        feed_id
            .rsplit(['~', '-'])
            .next()
            .unwrap_or(feed_id)
            .to_string()
    })
}

/// Build one agency config. `group` is `Some(..)` for a pollable/auth feed, or
/// `None` for a static-only (`no_realtime`) feed; `label`, when set, disambiguates
/// the name and slug of an operator that split into multiple mode configs.
fn build_config(
    op: &Operator,
    static_url: String,
    label: Option<&str>,
    group: Option<&Group>,
    geocoder: &ReverseGeocoder,
) -> AgencyConfig {
    let base = op
        .name
        .clone()
        .or_else(|| op.short_name.clone())
        .unwrap_or_else(|| op.onestop_id.clone());
    let (display_name, slug) = match label {
        Some(label) => (
            format!("{base} ({label})"),
            format!("{}~{label}", op.onestop_id),
        ),
        None => (base, op.onestop_id.clone()),
    };

    let (trip_updates_url, vehicle_positions_url, gtfs_rt_requires_auth) = match group {
        Some(group) => (
            group.trip_updates.clone(),
            group.vehicle_positions.clone(),
            Some(group.requires_auth),
        ),
        None => (Vec::new(), Vec::new(), None),
    };
    let (country_code, location) = locate(&op.onestop_id, geocoder);

    AgencyConfig {
        slug,
        display_name,
        static_url,
        realtime_urls: GtfsRtUrls {
            trip_updates_url,
            vehicle_positions_url,
        },
        gtfs_rt_requires_auth,
        country_code,
        location,
    }
}

/// Whether a feed is gated behind credentials we don't have (any real
/// authorization type present).
fn requires_auth(feed: &Feed) -> bool {
    feed.authorization
        .as_ref()
        .is_some_and(|auth| !auth.auth_type.is_empty() && auth.auth_type != "none")
}

/// An operator's location and country, derived from its Onestop ID's geohash.
/// Decodes the geohash to a point (via the `geohash` crate) — that's the
/// [`GeoPoint`] used for dedup — and reverse-geocodes it to the nearest known
/// place for an ISO 3166-1 country code (via `reverse_geocoder`, an offline
/// worldwide database). Both are `None` for IDs without a decodable geohash —
/// two-part IDs (`o-<name>`), whose name isn't a geohash.
fn locate(onestop_id: &str, geocoder: &ReverseGeocoder) -> (Option<String>, Option<GeoPoint>) {
    let Some(geohash) = onestop_id
        .strip_prefix("o-")
        .and_then(|id| id.split('-').next())
    else {
        return (None, None);
    };
    let Ok((coord, _, _)) = geohash::decode(geohash) else {
        return (None, None);
    };
    let country = geocoder.search((coord.y, coord.x)).record.cc.clone();
    (Some(country), Some(GeoPoint::new(coord.y, coord.x)))
}
