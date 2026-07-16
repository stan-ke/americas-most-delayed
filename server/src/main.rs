//! America's Most Delayed — a wall of shame for late public transit.

mod agency;
mod api;
mod catalogs;
mod delay;
mod gtfs;
mod history;
mod realtime;
mod scheduler;
mod wire;

use std::collections::HashSet;

use anyhow::Result;

use crate::agency::{AgencyConfig, GeoPoint};
use crate::catalogs::catalog::GtfsCatalogProvider;
use crate::catalogs::mobilitydata::MobilityDataProvider;
use crate::catalogs::transitland::TransitlandProvider;

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
    let configs = collect_agencies().await?;
    println!("Monitoring {} agencies", configs.len());

    // Spawn the polling tasks, then serve the API/frontend for as long as the
    // process lives.
    let scheduler = scheduler::start(configs)?;
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
async fn collect_agencies() -> Result<Vec<AgencyConfig>> {
    let mut configs = vec![agency::nj_transit()];
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
