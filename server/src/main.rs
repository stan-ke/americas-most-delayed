//! America's Most Delayed — a wall of shame for late public transit.

mod agency;
mod api;
mod auth;
mod catalogs;
mod delay;
mod gtfs;
mod history;
mod metrics;
mod realtime;
mod scheduler;
mod score;
mod wire;

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;

use crate::agency::{AgencyConfig, GeoPoint};
use crate::auth::FeedAuth;
use crate::catalogs::catalog::GtfsCatalogProvider;
use crate::catalogs::mobilitydata::MobilityDataProvider;
use crate::catalogs::transitland::TransitlandProvider;

// jemalloc as the global allocator, so the process actually gives memory back.
//
// This server allocates in bursts: a cold start downloads and imports hundreds of
// GTFS feeds, and the maintenance task re-imports stale ones every 24h. Under glibc
// malloc those bursts pinned RSS at their peak forever — freed buffers scattered
// across up to 8×ncpus arenas that glibc never `madvise`s back to the OS, so a
// cold start sat at ~500 MB indefinitely while a warm restart (which skips the
// downloads/imports) sat at ~200 MB. jemalloc bounds the arena count and reclaims;
// see the `malloc_conf` below.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

// Turn on jemalloc's background purge thread and give dirty/muzzy pages a short
// decay window, so the memory freed after a warmup or maintenance burst is returned
// to the OS within seconds instead of being held for the life of the process. This
// is what keeps RSS flat across an indefinite run rather than ratcheting up to the
// cold-start high-water mark.
//
// The symbol jemalloc reads is `_rjem_malloc_conf`, not `malloc_conf`: tikv's build
// prefixes every jemalloc symbol with `_rjem_` (its env override is likewise
// `_RJEM_MALLOC_CONF`). `#[used]` keeps this otherwise-unreferenced static from being
// stripped before it can override jemalloc's own weak default; `export_name` is
// `unsafe` in edition 2024.
#[cfg(not(target_env = "msvc"))]
#[used]
#[allow(non_upper_case_globals)]
#[unsafe(export_name = "_rjem_malloc_conf")]
pub static malloc_conf: &[u8] = b"background_thread:true,dirty_decay_ms:5000,muzzy_decay_ms:5000\0";

/// Catalog sources to draw agencies from, **in order of preference** — an earlier
/// source wins any collision (same slug or same realtime feed) against a later
/// one. To use only one catalog, delete the other line; to change which catalog
/// wins duplicates, reorder them. NJ Transit is always tried first (it's in no
/// catalog), so it wins everything.
const CATALOG_SOURCES: &[CatalogSource] =
    &[CatalogSource::Transitland, CatalogSource::MobilityData];

/// One pluggable catalog. Each variant knows how to fetch its provider and return
/// that provider's agency configs.
#[derive(Debug, Clone, Copy)]
enum CatalogSource {
    Transitland,
    MobilityData,
}

impl CatalogSource {
    fn name(self) -> &'static str {
        match self {
            CatalogSource::Transitland => "Transitland Atlas",
            CatalogSource::MobilityData => "MobilityData",
        }
    }

    /// Fetch and parse this catalog into agency configs. Async because every
    /// provider downloads its catalog over the network at startup.
    async fn load(self) -> Result<Vec<AgencyConfig>> {
        match self {
            CatalogSource::Transitland => Ok(TransitlandProvider::new().await?.get_agencies()),
            CatalogSource::MobilityData => Ok(MobilityDataProvider::new().await?.get_agencies()),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Feed credentials (git-ignored `keys.env`) — loaded once and shared by the
    // agency wiring (which feeds it can build) and the scheduler (which injects the
    // credentials into requests).
    let auth = Arc::new(FeedAuth::load());

    let configs = collect_agencies(&auth).await?;
    println!("Monitoring {} agencies", configs.len());

    // Spawn the polling tasks, then serve the API/frontend for as long as the
    // process lives.
    let scheduler = scheduler::start(configs, auth)?;
    api::serve(scheduler).await
}

/// Build the full list of feeds to monitor: NJ Transit (hand-configured, in no
/// catalog) followed by every [`CATALOG_SOURCES`] entry in order of preference.
///
/// A source that fails to load is logged and skipped rather than aborting
/// startup. Agencies are deduped across sources by slug **and** by realtime feed
/// URL, so the same agency listed under two catalogs' differing ids isn't polled
/// twice — the earlier (more-preferred) source wins. Finally the list is scoped
/// to North America.
async fn collect_agencies(auth: &FeedAuth) -> Result<Vec<AgencyConfig>> {
    // NJ Transit and the other hand-configured feeds go first, so they win dedup
    // over the catalogs' `requires_auth` / `no_realtime` versions of the same
    // agency (see `auth::FeedAuth::authed_agencies`, driven by `auth.json`).
    let mut configs = vec![agency::nj_transit()];
    configs.extend(auth.authed_agencies());
    for &source in CATALOG_SOURCES {
        match source.load().await {
            Ok(agencies) => {
                println!("{}: {} agencies", source.name(), agencies.len());
                configs.extend(agencies);
            }
            Err(err) => eprintln!("catalog source {} failed to load: {err:#}", source.name()),
        }
    }

    // Scope to North America first, so a dropped out-of-scope config can't
    // "claim" a slug or realtime URL that an in-scope config from a later source
    // legitimately needs.
    configs.retain(|config| {
        matches!(
            config.country_code.as_deref(),
            Some("US") | Some("CA") | Some("MX")
        )
    });

    // Pass 1 — dedup *exact* feed matches across sources: a config whose slug,
    // static-feed URL, or realtime trip-updates URL an earlier (more-preferred)
    // one already claimed. This is precise (shared URLs are definitely the same
    // feed) and keeps an agency from being polled twice under two catalogs' ids.
    let mut seen_slugs = HashSet::new();
    let mut seen_static = HashSet::new();
    let mut seen_realtime = HashSet::new();
    configs.retain(|config| {
        let urls = &config.realtime_urls.trip_updates_url;
        if seen_slugs.contains(&config.slug)
            || seen_static.contains(&config.static_url)
            || urls.iter().any(|url| seen_realtime.contains(url))
        {
            return false;
        }
        seen_slugs.insert(config.slug.clone());
        seen_static.insert(config.static_url.clone());
        seen_realtime.extend(urls.iter().cloned());
        true
    });

    // Pass 2 — dedup the *same agency* listed under different names/ids/URLs
    // across catalogs (e.g. "Valley Metro" and "Valley Metro (VM)"). Two configs
    // match when [`Identity::same_agency`] holds: same base name + country,
    // compatible parentheticals, and locations within [`DEDUP_RADIUS_KM`]. The
    // location check is what keeps genuinely distinct same-named agencies apart
    // (two far-apart "Valley Transit"s; BC Transit's regional systems, already
    // separated by their differing parentheticals). Within a match we keep the
    // most useful feed (pollable > auth-gated > static-only), so a dedup never
    // trades a polled feed for a dead one.
    let mut deduped: Vec<AgencyConfig> = Vec::new();
    let mut identities: Vec<Identity> = Vec::new(); // parallel to `deduped`
    for config in configs {
        let identity = Identity::of(&config);
        match identities
            .iter()
            .position(|kept| kept.same_agency(&identity))
        {
            Some(i) if identity.usefulness > identities[i].usefulness => {
                deduped[i] = config;
                identities[i] = identity;
            }
            Some(_) => {}
            None => {
                deduped.push(config);
                identities.push(identity);
            }
        }
    }

    Ok(deduped)
}

/// Farthest two feeds of the *same* agency ever appear (a coarse-located feed
/// vs. a precise one); comfortably below the nearest distinct same-named
/// agencies. See `collect_agencies`' pass 2.
const DEDUP_RADIUS_KM: f64 = 150.0;

/// A config's identity for the fuzzy same-agency dedup, precomputed once.
struct Identity {
    /// Display name with any parenthetical removed, normalized.
    base: String,
    /// The parenthetical qualifier, normalized (empty if none) — distinguishes
    /// sibling systems that share a base name, e.g. "BC Transit (Kamloops…)".
    paren: String,
    country: Option<String>,
    location: Option<GeoPoint>,
    /// Preference when collapsing a match: pollable (2) > auth-gated (1) >
    /// static-only (0).
    usefulness: u8,
}

impl Identity {
    fn of(config: &AgencyConfig) -> Self {
        let (base, paren) = split_name(&config.display_name);
        let usefulness = match (config.has_trip_updates(), config.requires_auth()) {
            (true, false) => 2,
            (true, true) => 1,
            (false, _) => 0,
        };
        Identity {
            base,
            paren,
            country: config.country_code.clone(),
            location: config.location,
            usefulness,
        }
    }

    fn same_agency(&self, other: &Identity) -> bool {
        self.base == other.base
            && self.country == other.country
            && (self.paren.is_empty() || other.paren.is_empty() || self.paren == other.paren)
            && match (self.location, other.location) {
                (Some(a), Some(b)) => a.distance_km(b) <= DEDUP_RADIUS_KM,
                _ => false,
            }
    }
}

/// Split a display name into a normalized base name and its parenthetical
/// qualifier — "Valley Metro (VM)" → ("valley metro", "vm").
fn split_name(name: &str) -> (String, String) {
    let (mut base, mut paren) = (String::new(), String::new());
    let mut depth = 0i32;
    for ch in name.chars() {
        match ch {
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            _ if depth > 0 => paren.push(ch),
            _ => base.push(ch),
        }
    }
    (normalize(&base), normalize(&paren))
}

/// Lowercase, drop punctuation, and collapse whitespace, so trivial formatting
/// differences ("Metro St. Louis" vs "Metro St Louis") don't defeat a name match.
fn normalize(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .to_lowercase();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}
