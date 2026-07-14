//! Per-agency configuration and static GTFS loading.
//!
//! Each agency we want on the wall of shame is described once in [`AGENCIES`].
//! Adding a new transit system is a matter of appending one entry there.

use crate::macros::partial_config;

partial_config! {
    /// Static description of one transit feed we monitor, paired with
    /// [`PartialAgencyConfig`] for assembling config out of catalog sources
    /// that may each only supply some of the fields.
    pub struct AgencyConfig / PartialAgencyConfig {
        /// Short slug used for the on-disk cache filename, e.g. `"nj_transit_bus"`.
        required pub slug: String = "missing agency slug",
        /// Human-readable name shown on the leaderboard.
        required pub display_name: String = "missing agency display name",
        /// URL of the static GTFS zip (routes, trips, schedules).
        required pub static_url: String = "missing agency static URL",
        /// GTFS-realtime config
        nested pub realtime_urls: GtfsRtUrlSet as PartialGtfsRtUrlSet = "missing agency realtime URLs",
        default pub gtfs_rt_requires_auth: Option<bool> = None,
        default pub country_code: Option<String> = None,
        /// Approximate location of the agency's service area, when the catalog
        /// gives one. Used only to disambiguate same-named agencies during
        /// cross-catalog dedup (two "Valley Transit"s far apart aren't merged).
        default pub location: Option<GeoPoint> = None,
        /// Key used only while merging duplicate catalog entries; never part
        /// of the complete config.
        partial_only pub static_reference: String,
    }
}

/// A rough point on the globe, stored as fixed-point microdegrees so the config
/// can stay `Eq` (`f64` isn't). Precise enough for city-scale dedup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeoPoint {
    lat_e6: i32,
    lon_e6: i32,
}

impl GeoPoint {
    pub fn new(lat: f64, lon: f64) -> Self {
        GeoPoint {
            lat_e6: (lat * 1e6) as i32,
            lon_e6: (lon * 1e6) as i32,
        }
    }

    fn lat(self) -> f64 {
        self.lat_e6 as f64 / 1e6
    }

    fn lon(self) -> f64 {
        self.lon_e6 as f64 / 1e6
    }

    /// Great-circle distance to another point, in kilometers (haversine).
    pub fn distance_km(self, other: GeoPoint) -> f64 {
        let (lat1, lat2) = (self.lat().to_radians(), other.lat().to_radians());
        let dlat = lat2 - lat1;
        let dlon = (other.lon() - self.lon()).to_radians();
        let a = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
        6371.0 * 2.0 * a.sqrt().asin()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GtfsRtUrlSet {
    Combined {
        url: Vec<String>,
    },
    Separate {
        trip_updates_url: Vec<String>,
        vehicle_positions_url: Vec<String>,
        service_alerts_url: Vec<String>,
    },
}

impl GtfsRtUrlSet {
    pub fn trip_updates_url(&self) -> &[String] {
        match self {
            GtfsRtUrlSet::Combined { url } => url.as_slice(),
            GtfsRtUrlSet::Separate {
                trip_updates_url, ..
            } => trip_updates_url.as_slice(),
        }
    }

    /// The vehicle-positions feed URL(s), fetched only for hot feeds to place the
    /// most-delayed vehicle on the map. A `Combined` feed serves everything from
    /// one URL; a `Separate` feed has a dedicated one (possibly empty).
    pub fn vehicle_positions_url(&self) -> &[String] {
        match self {
            GtfsRtUrlSet::Combined { url } => url.as_slice(),
            GtfsRtUrlSet::Separate {
                vehicle_positions_url,
                ..
            } => vehicle_positions_url.as_slice(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartialGtfsRtUrlSet {
    Combined {
        url: Vec<String>,
    },
    Separate {
        trip_updates_url: Vec<String>,
        vehicle_positions_url: Vec<String>,
        service_alerts_url: Vec<String>,
    },
}

impl PartialGtfsRtUrlSet {
    pub fn merge_other(
        &self,
        other: &PartialGtfsRtUrlSet,
    ) -> Result<PartialGtfsRtUrlSet, anyhow::Error> {
        match (self, other) {
            (
                PartialGtfsRtUrlSet::Combined { url: url1 },
                PartialGtfsRtUrlSet::Combined { url: url2 },
            ) => {
                let mut url = url1.clone();
                url.extend(url2.clone());
                url.dedup();
                Ok(PartialGtfsRtUrlSet::Combined { url })
            }
            (
                PartialGtfsRtUrlSet::Separate {
                    trip_updates_url: tu1,
                    vehicle_positions_url: vp1,
                    service_alerts_url: sa1,
                },
                PartialGtfsRtUrlSet::Separate {
                    trip_updates_url: tu2,
                    vehicle_positions_url: vp2,
                    service_alerts_url: sa2,
                },
            ) => {
                let mut trip_updates_url = tu1.clone();
                trip_updates_url.extend(tu2.clone());
                trip_updates_url.dedup();
                let mut vehicle_positions_url = vp1.clone();
                vehicle_positions_url.extend(vp2.clone());
                vehicle_positions_url.dedup();
                let mut service_alerts_url = sa1.clone();
                service_alerts_url.extend(sa2.clone());
                service_alerts_url.dedup();
                Ok(PartialGtfsRtUrlSet::Separate {
                    trip_updates_url,
                    vehicle_positions_url,
                    service_alerts_url,
                })
            }
            _ => anyhow::bail!("cannot merge GtfsRtUrlSet of different variants"),
        }
    }

    pub fn upgrade_to_complete(&self) -> Result<GtfsRtUrlSet, anyhow::Error> {
        match self {
            PartialGtfsRtUrlSet::Combined { url } => {
                if url.is_empty() {
                    anyhow::bail!("missing combined GTFS-realtime URL")
                }
                Ok(GtfsRtUrlSet::Combined { url: url.clone() })
            }
            PartialGtfsRtUrlSet::Separate {
                trip_updates_url,
                vehicle_positions_url,
                service_alerts_url,
            } => {
                if trip_updates_url.is_empty() {
                    anyhow::bail!("missing trip updates GTFS-realtime URL")
                }
                Ok(GtfsRtUrlSet::Separate {
                    trip_updates_url: trip_updates_url.clone(),
                    vehicle_positions_url: vehicle_positions_url.clone(),
                    service_alerts_url: service_alerts_url.clone(),
                })
            }
        }
    }
}

impl AgencyConfig {
    /// Whether this feed exposes at least one trip-updates URL — the minimum for
    /// it to be worth tracking at all (even if we can't currently poll it).
    pub fn has_trip_updates(&self) -> bool {
        !self.realtime_urls.trip_updates_url().is_empty()
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
        !self.realtime_urls.vehicle_positions_url().is_empty()
    }
}

/// NJ Transit is special: it isn't published in the MobilityData catalog, so we
/// hand-configure it and prepend it to the catalog-derived agency list.
pub fn nj_transit() -> AgencyConfig {
    AgencyConfig {
        slug: "nj_transit_bus".to_string(),
        display_name: "NJ Transit Bus".to_string(),
        static_url: "https://pcsdata.njtransit.com/api/GTFSG2/getGTFS".to_string(),
        realtime_urls: GtfsRtUrlSet::Separate {
            trip_updates_url: vec![
                "https://pcsdata.njtransit.com/api/GTFSG2/getTripUpdates".to_string(),
            ],
            vehicle_positions_url: vec![
                "https://pcsdata.njtransit.com/api/GTFSG2/getVehiclePositions".to_string(),
            ],
            service_alerts_url: vec![],
        },
        gtfs_rt_requires_auth: Some(false),
        country_code: Some("US".to_string()),
        location: Some(GeoPoint::new(40.735, -74.172)),
    }
}
