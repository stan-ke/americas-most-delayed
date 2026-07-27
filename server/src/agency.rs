//! Per-agency configuration and static GTFS loading.
//!
//! [`AgencyConfig`] is the unit of "one feed we monitor". A single agency's data
//! is assembled from multiple catalog rows, so the catalog providers build one up
//! incrementally (see `catalogs::mobilitydata`) before handing over a complete
//! config here.

/// Static description of one transit feed we monitor.
#[derive(Debug, Clone)]
pub struct AgencyConfig {
    /// Short slug used for the on-disk cache filename, e.g. `"nj_transit_bus"`.
    pub slug: String,
    /// Human-readable name shown on the leaderboard.
    pub display_name: String,
    /// URL of the static GTFS zip (routes, trips, schedules).
    pub static_url: String,
    /// Additional static GTFS zips merged into the same schedule index alongside
    /// [`static_url`](Self::static_url). Almost always empty — it exists for feeds
    /// whose one realtime stream spans several separately-published static zips
    /// (MTA Bus's realtime covers all five boroughs, but each borough's schedule is
    /// a distinct zip), so their non-primary trips can still resolve stop names and
    /// route shapes. Only [`static_url`] is used as a dedup key / census subject.
    pub extra_static_urls: Vec<String>,
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

    /// Every static GTFS zip backing this feed — the primary
    /// [`static_url`](Self::static_url) first, then any
    /// [`extra_static_urls`](Self::extra_static_urls). These are merged into one
    /// schedule index (see [`crate::gtfs::Gtfs::load`]); the primary stays first so
    /// its cache filename (`<slug>.zip`) is unchanged.
    pub fn all_static_urls(&self) -> Vec<String> {
        std::iter::once(self.static_url.clone())
            .chain(self.extra_static_urls.iter().cloned())
            .collect()
    }
}

/// NJ Transit is special: it isn't published in the MobilityData catalog, so we
/// hand-configure it and prepend it to the catalog-derived agency list.
pub fn nj_transit() -> AgencyConfig {
    AgencyConfig {
        slug: "nj_transit_bus".to_string(),
        display_name: "NJ Transit Bus".to_string(),
        static_url: "https://pcsdata.njtransit.com/api/GTFSG2/getGTFS".to_string(),
        extra_static_urls: Vec::new(),
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
