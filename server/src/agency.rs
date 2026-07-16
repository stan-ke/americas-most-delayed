//! Per-agency configuration and static GTFS loading.
//!
//! [`AgencyConfig`] is the unit of "one feed we monitor". A single agency's data
//! is assembled from multiple catalog rows, so the catalog providers build one up
//! incrementally (see `catalogs::mobilitydata`) before handing over a complete
//! config here.

use crate::auth::{self, FeedAuth};

/// Static description of one transit feed we monitor.
#[derive(Debug, Clone)]
pub struct AgencyConfig {
    /// Short slug used for the on-disk cache filename, e.g. `"nj_transit_bus"`.
    pub slug: String,
    /// Human-readable name shown on the leaderboard.
    pub display_name: String,
    /// URL of the static GTFS zip (routes, trips, schedules).
    pub static_url: String,
    /// GTFS-realtime feed URLs.
    pub realtime_urls: GtfsRtUrls,
    pub gtfs_rt_requires_auth: Option<bool>,
    pub country_code: Option<String>,
    /// Approximate location of the agency's service area, when the catalog
    /// gives one. Used only to disambiguate same-named agencies during
    /// cross-catalog dedup (two "Valley Transit"s far apart aren't merged).
    pub location: Option<GeoPoint>,
}

/// A rough point on the globe
#[derive(Debug, Clone, Copy)]
pub struct GeoPoint {
    lat: f64,
    lon: f64,
}

impl GeoPoint {
    pub fn new(lat: f64, lon: f64) -> Self {
        GeoPoint { lat, lon }
    }

    /// Great-circle distance to another point, in kilometers (haversine).
    pub fn distance_km(self, other: GeoPoint) -> f64 {
        let (lat1, lat2) = (self.lat.to_radians(), other.lat.to_radians());
        let dlat = lat2 - lat1;
        let dlon = (other.lon - self.lon).to_radians();
        let a = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
        6371.0 * 2.0 * a.sqrt().asin()
    }
}

/// The GTFS-realtime feed URLs we monitor for one agency (each may hold more than
/// one URL — a mode-split feed merges several). Trip-updates drives the delay
/// computation; vehicle-positions is fetched for hot feeds to place and verify the
/// delayed vehicle. Service-alerts feeds exist in the catalogs but we consume none,
/// so they aren't kept.
#[derive(Debug, Clone, Default)]
pub struct GtfsRtUrls {
    pub trip_updates_url: Vec<String>,
    pub vehicle_positions_url: Vec<String>,
}

impl AgencyConfig {
    /// Whether this feed exposes at least one trip-updates URL — the minimum for
    /// it to be worth tracking at all (even if we can't currently poll it).
    pub fn has_trip_updates(&self) -> bool {
        !self.realtime_urls.trip_updates_url.is_empty()
    }

    /// Whether the realtime feed is behind authentication we don't have, so it
    /// can be surfaced in `/status` but never actually polled.
    pub fn requires_auth(&self) -> bool {
        self.gtfs_rt_requires_auth.unwrap_or(false)
    }

    /// Whether this feed exposes at least one vehicle-positions URL. We require it
    /// to poll a feed: without live positions we can't verify a delayed trip's
    /// vehicle is actually on its route, so an unverifiable feed is excluded.
    pub fn has_vehicle_positions(&self) -> bool {
        !self.realtime_urls.vehicle_positions_url.is_empty()
    }
}

/// NJ Transit is special: it isn't published in the MobilityData catalog, so we
/// hand-configure it and prepend it to the catalog-derived agency list.
pub fn nj_transit() -> AgencyConfig {
    AgencyConfig {
        slug: "nj_transit_bus".to_string(),
        display_name: "NJ Transit Bus".to_string(),
        static_url: "https://pcsdata.njtransit.com/api/GTFSG2/getGTFS".to_string(),
        realtime_urls: GtfsRtUrls {
            trip_updates_url: vec![
                "https://pcsdata.njtransit.com/api/GTFSG2/getTripUpdates".to_string(),
            ],
            vehicle_positions_url: vec![
                "https://pcsdata.njtransit.com/api/GTFSG2/getVehiclePositions".to_string(),
            ],
        },
        gtfs_rt_requires_auth: Some(false),
        country_code: Some("US".to_string()),
        location: Some(GeoPoint::new(40.735, -74.172)),
    }
}

/// Hand-configured feeds the public catalogs either gate behind auth (which we now
/// hold a key for) or mislabel as having no realtime. Prepended after
/// [`nj_transit`] in `main::collect_agencies`, so they win cross-catalog dedup over
/// the catalogs' `requires_auth` / `no_realtime` versions of the same agency.
///
/// Each *keyed* feed is built only when its credential is present in `keys.env`, so
/// a missing key silently drops the agency rather than polling it into a `401`. The
/// credentials themselves are injected per-host at fetch time (see [`crate::auth`]);
/// the URLs here carry no secrets — the one exception is TriMet, whose app id goes
/// in the URL path and so is spliced in here from the loaded secret.
pub fn authed_agencies(auth: &FeedAuth) -> Vec<AgencyConfig> {
    let mut configs = Vec::new();

    // Société de transport de Montréal — `apiKey` request header on api.stm.info.
    if auth.has(auth::STM_API_KEY) {
        configs.push(AgencyConfig {
            slug: "stm".to_string(),
            display_name: "Société de transport de Montréal".to_string(),
            static_url: "https://www.stm.info/sites/default/files/gtfs/gtfs_stm.zip".to_string(),
            realtime_urls: GtfsRtUrls {
                trip_updates_url: vec![
                    "https://api.stm.info/pub/od/gtfs-rt/ic/v2/tripUpdates".to_string(),
                ],
                vehicle_positions_url: vec![
                    "https://api.stm.info/pub/od/gtfs-rt/ic/v2/vehiclePositions".to_string(),
                ],
            },
            gtfs_rt_requires_auth: Some(false),
            country_code: Some("CA".to_string()),
            location: Some(GeoPoint::new(45.51, -73.59)),
        });
    }

    // OC Transpo (Ottawa) — `Ocp-Apim-Subscription-Key` header (Azure API gateway).
    if auth.has(auth::OCTRANSPO_API_KEY) {
        configs.push(AgencyConfig {
            slug: "oc_transpo".to_string(),
            display_name: "OC Transpo".to_string(),
            static_url:
                "https://oct-gtfs-emasagcnfmcgeham.z01.azurefd.net/public-access/GTFSExport.zip"
                    .to_string(),
            realtime_urls: GtfsRtUrls {
                trip_updates_url: vec![
                    "https://nextrip-public-api.azure-api.net/octranspo/gtfs-rt-tp/beta/v1/TripUpdates".to_string(),
                ],
                vehicle_positions_url: vec![
                    "https://nextrip-public-api.azure-api.net/octranspo/gtfs-rt-vp/beta/v1/VehiclePositions".to_string(),
                ],
            },
            gtfs_rt_requires_auth: Some(false),
            country_code: Some("CA".to_string()),
            location: Some(GeoPoint::new(45.42, -75.70)),
        });
    }

    // TriMet (Portland) — the app id is baked into the URL path, not a header/query,
    // so it's spliced in here rather than injected by `crate::auth`.
    if let Some(app_id) = auth.get(auth::TRIMET_APP_ID) {
        configs.push(AgencyConfig {
            slug: "trimet".to_string(),
            display_name: "TriMet".to_string(),
            static_url: "https://developer.trimet.org/schedule/gtfs.zip".to_string(),
            realtime_urls: GtfsRtUrls {
                trip_updates_url: vec![format!(
                    "https://developer.trimet.org/ws/V1/TripUpdate/appID/{app_id}"
                )],
                vehicle_positions_url: vec![format!(
                    "https://developer.trimet.org/ws/V1/VehiclePositions/appID/{app_id}"
                )],
            },
            gtfs_rt_requires_auth: Some(false),
            country_code: Some("US".to_string()),
            location: Some(GeoPoint::new(45.52, -122.68)),
        });
    }

    // MTA Bus Time — `key` query parameter. One combined realtime feed covers *all*
    // NYC boroughs (the keys.txt "all boroughs" note): only the static schedules are
    // per-borough, not the realtime feed. We compare against the Manhattan static;
    // buses elsewhere still surface whenever the feed reports a delay directly
    // (trip/stop-level), which is the common case for BusTime.
    if auth.has(auth::MTA_BUSTIME_KEY) {
        configs.push(AgencyConfig {
            slug: "mta_bus".to_string(),
            display_name: "MTA Bus".to_string(),
            static_url: "http://web.mta.info/developers/data/nyct/bus/google_transit_manhattan.zip"
                .to_string(),
            realtime_urls: GtfsRtUrls {
                trip_updates_url: vec!["https://gtfsrt.prod.obanyc.com/tripUpdates".to_string()],
                vehicle_positions_url: vec![
                    "https://gtfsrt.prod.obanyc.com/vehiclePositions".to_string(),
                ],
            },
            gtfs_rt_requires_auth: Some(false),
            country_code: Some("US".to_string()),
            location: Some(GeoPoint::new(40.73, -73.99)),
        });
    }

    // Puget Sound OneBusAway — `key` query parameter. The keys.txt note asks for
    // trip updates across *all* agencies on the regional OBA server, so we merge
    // every agency's per-id feed into one config (the scheduler polls the whole
    // `Vec`) and schedule-compare against the regional Consolidated GTFS that OBA is
    // itself built from — so the merged trip ids line up.
    if auth.has(auth::PUGET_SOUND_KEY) {
        // OBA numeric agency ids that carry realtime on the Puget Sound server: King
        // County Metro (1), Pierce (3), Intercity (19), Kitsap (20), Seattle
        // Streetcar (23), Community Transit (29), Sound Transit (40), WA State
        // Ferries (95), Everett (97).
        const AGENCY_IDS: &[u32] = &[1, 3, 19, 20, 23, 29, 40, 95, 97];
        const BASE: &str = "https://api.pugetsound.onebusaway.org/api/gtfs_realtime";
        configs.push(AgencyConfig {
            slug: "puget_sound_oba".to_string(),
            display_name: "Puget Sound (OneBusAway)".to_string(),
            static_url: "https://gtfs.sound.obaweb.org/prod/gtfs_puget_sound_consolidated.zip"
                .to_string(),
            realtime_urls: GtfsRtUrls {
                trip_updates_url: AGENCY_IDS
                    .iter()
                    .map(|id| format!("{BASE}/trip-updates-for-agency/{id}.pb"))
                    .collect(),
                vehicle_positions_url: AGENCY_IDS
                    .iter()
                    .map(|id| format!("{BASE}/vehicle-positions-for-agency/{id}.pb"))
                    .collect(),
            },
            gtfs_rt_requires_auth: Some(false),
            country_code: Some("US".to_string()),
            location: Some(GeoPoint::new(47.61, -122.33)),
        });
    }

    // MTA Subway — *not* authenticated: the per-line feeds on api-endpoint.mta.info
    // are open. Hand-configured because the catalogs mark the subway `no_realtime`
    // despite these live feeds existing (the keys.txt subway note). Each URL is one
    // FeedMessage carrying both trip updates and vehicle positions, so it appears
    // under both — the vehicle-positions entry is what keeps the feed in the poll
    // rotation (a feed with none is skipped). Subway positions are stop-based rather
    // than coordinates, so the map has nothing to plot, but the delays still rank.
    let subway_feeds: Vec<String> = [
        "gtfs", "gtfs-ace", "gtfs-bdfm", "gtfs-g", "gtfs-jz", "gtfs-nqrw", "gtfs-l", "gtfs-si",
    ]
    .iter()
    .map(|feed| format!("https://api-endpoint.mta.info/Dataservice/mtagtfsfeeds/nyct%2F{feed}"))
    .collect();
    configs.push(AgencyConfig {
        slug: "mta_subway".to_string(),
        display_name: "MTA Subway".to_string(),
        static_url: "http://web.mta.info/developers/data/nyct/subway/google_transit.zip"
            .to_string(),
        realtime_urls: GtfsRtUrls {
            trip_updates_url: subway_feeds.clone(),
            vehicle_positions_url: subway_feeds,
        },
        gtfs_rt_requires_auth: Some(false),
        country_code: Some("US".to_string()),
        location: Some(GeoPoint::new(40.73, -73.99)),
    });

    configs
}
