use std::collections::HashMap;

use crate::{
    agency::{AgencyConfig, GeoPoint, GtfsRtUrls},
    catalogs::catalog::GtfsCatalogProvider,
};
use anyhow::Context;
use serde::{Deserialize, Deserializer};

static MOBILITYDATA_CATALOG_URL: &str = "https://files.mobilitydatabase.org/feeds_v2.csv";

pub type Catalog = Vec<Source>;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Source {
    pub id: String,
    pub data_type: DataType,

    #[serde(default, deserialize_with = "pipe_separated")]
    pub entity_type: Vec<RealtimeEntity>,

    #[serde(rename = "location.country_code")]
    pub country_code: String,
    #[serde(
        rename = "location.subdivision_name",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    pub subdivision_name: Option<String>,
    #[serde(
        rename = "location.municipality",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    pub municipality: Option<String>,

    #[serde(
        rename = "location.bounding_box.minimum_latitude",
        default,
        deserialize_with = "de_opt_number"
    )]
    pub min_latitude: Option<f64>,
    #[serde(
        rename = "location.bounding_box.maximum_latitude",
        default,
        deserialize_with = "de_opt_number"
    )]
    pub max_latitude: Option<f64>,
    #[serde(
        rename = "location.bounding_box.minimum_longitude",
        default,
        deserialize_with = "de_opt_number"
    )]
    pub min_longitude: Option<f64>,
    #[serde(
        rename = "location.bounding_box.maximum_longitude",
        default,
        deserialize_with = "de_opt_number"
    )]
    pub max_longitude: Option<f64>,
    #[serde(
        rename = "location.bounding_box.extracted_on",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    pub bbox_extracted_on: Option<String>,

    pub provider: String,

    #[serde(default, deserialize_with = "de_opt_bool")]
    pub is_official: Option<bool>,
    pub name: String,

    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub note: Option<String>,
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub feed_contact_email: Option<String>,
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub static_reference: Option<String>,

    #[serde(
        rename = "urls.direct_download",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    pub direct_download: Option<String>,
    #[serde(
        rename = "urls.authentication_type",
        default,
        deserialize_with = "de_opt_number"
    )]
    pub authentication_type: Option<u8>,
    #[serde(
        rename = "urls.authentication_info",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    pub authentication_info: Option<String>,
    #[serde(
        rename = "urls.api_key_parameter_name",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    pub api_key_parameter_name: Option<String>,
    #[serde(
        rename = "urls.latest",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    pub latest: Option<String>,
    #[serde(
        rename = "urls.license",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    pub license: Option<String>,

    pub status: Status,

    #[serde(default, deserialize_with = "pipe_separated")]
    pub features: Vec<String>,

    #[serde(
        rename = "redirect.id",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    pub redirect_id: Option<String>,
    #[serde(
        rename = "redirect.comment",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    pub redirect_comment: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataType {
    Gtfs,
    GtfsRt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RealtimeEntity {
    #[serde(rename = "sa")]
    ServiceAlerts,
    #[serde(rename = "tu")]
    TripUpdates,
    #[serde(rename = "vp")]
    VehiclePositions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Active,
    Deprecated,
    Development,
    Future,
    Inactive,
}

fn empty_string_as_none<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match Option::<String>::deserialize(deserializer)? {
        None => Ok(None),
        Some(s) if s.trim().is_empty() || s.trim() == "not_set" => Ok(None),
        Some(s) => s.parse().map(Some).map_err(serde::de::Error::custom),
    }
}

fn de_opt_number<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: std::str::FromStr + Deserialize<'de>,
    T::Err: std::fmt::Display,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumOrStr<U> {
        Num(U),
        Str(String),
    }

    match Option::<NumOrStr<T>>::deserialize(deserializer)? {
        None => Ok(None),
        Some(NumOrStr::Num(n)) => Ok(Some(n)),
        Some(NumOrStr::Str(s)) if s.trim().is_empty() => Ok(None),
        Some(NumOrStr::Str(s)) => s.trim().parse().map(Some).map_err(serde::de::Error::custom),
    }
}

fn de_opt_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum BoolLike {
        Bool(bool),
        Str(String),
    }

    match Option::<BoolLike>::deserialize(deserializer)? {
        None => Ok(None),
        Some(BoolLike::Bool(b)) => Ok(Some(b)),
        Some(BoolLike::Str(s)) => match s.trim() {
            "" | "not_set" => Ok(None),
            t if t.eq_ignore_ascii_case("true") => Ok(Some(true)),
            t if t.eq_ignore_ascii_case("false") => Ok(Some(false)),
            other => Err(serde::de::Error::custom(format!("invalid bool: {other:?}"))),
        },
    }
}

fn pipe_separated<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    use serde::de::IntoDeserializer;
    let s = String::deserialize(deserializer)?;
    if s.trim().is_empty() {
        return Ok(Vec::new());
    }
    s.split('|')
        .map(str::trim)
        .map(|part| T::deserialize(part.into_deserializer()))
        .collect()
}

/// The center of a source's bounding box, when the catalog provides all four
/// corners — a rough location for the agency, used only for dedup.
fn bbox_center(source: &Source) -> Option<GeoPoint> {
    let lat = (source.min_latitude? + source.max_latitude?) / 2.0;
    let lon = (source.min_longitude? + source.max_longitude?) / 2.0;
    Some(GeoPoint::new(lat, lon))
}

async fn get_catalog() -> anyhow::Result<Catalog> {
    let csv_data = reqwest::get(MOBILITYDATA_CATALOG_URL).await?.text().await?;
    let mut reader = csv::Reader::from_reader(csv_data.as_bytes());
    let mut catalog = Vec::new();
    for result in reader.deserialize() {
        let source: Source = result?;
        catalog.push(source);
    }
    Ok(catalog)
}

pub struct MobilityDataProvider {
    agencies: Vec<AgencyConfig>,
}

/// Drop duplicate URLs, keeping first occurrence — two catalog rows can point a
/// bucket at the same feed, and we don't want to poll it twice.
fn dedup(mut urls: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    urls.retain(|url| seen.insert(url.clone()));
    urls
}

/// A single agency's config accumulated across catalog rows. A real agency is
/// split across a static-feed row and one or more realtime-feed rows sharing a
/// `static_reference`; we fold every matching row into one of these, then turn it
/// into a complete [`AgencyConfig`]. The first row seen wins each scalar field.
#[derive(Default)]
struct Build {
    display_name: Option<String>,
    gtfs_rt_requires_auth: Option<bool>,
    country_code: Option<String>,
    location: Option<GeoPoint>,
    static_url: Option<String>,
    trip_updates_url: Vec<String>,
    vehicle_positions_url: Vec<String>,
}

impl Build {
    fn fold(&mut self, source: &Source) {
        // The catalog `name` is often blank; fall back to the operator name and
        // then the feed id so the leaderboard never shows an empty label.
        let display_name = [source.name.trim(), source.provider.trim()]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or(source.id.as_str());
        self.display_name
            .get_or_insert_with(|| display_name.to_string());
        // 0 is no authentication, 1/2 are basic/digest.
        self.gtfs_rt_requires_auth = self
            .gtfs_rt_requires_auth
            .or_else(|| source.authentication_type.map(|t| matches!(t, 1 | 2)));
        self.country_code
            .get_or_insert_with(|| source.country_code.clone());
        self.location = self.location.or_else(|| bbox_center(source));

        if source.data_type == DataType::Gtfs {
            if self.static_url.is_none() {
                self.static_url = source.direct_download.clone();
            }
            return;
        }

        // A realtime row carries one direct-download URL that serves whichever
        // entity types it lists (an empty list means all of them). Route it into
        // the bucket(s) we actually consume; many rows across one agency
        // accumulate here. Service-alerts-only feeds contribute nothing.
        let Some(url) = source.direct_download.clone() else {
            return;
        };
        let all = source.entity_type.is_empty();
        let serves = |entity| all || source.entity_type.contains(&entity);
        if serves(RealtimeEntity::TripUpdates) {
            self.trip_updates_url.push(url.clone());
        }
        if serves(RealtimeEntity::VehiclePositions) {
            self.vehicle_positions_url.push(url);
        }
    }
}

impl MobilityDataProvider {
    pub async fn new() -> Result<Self, anyhow::Error> {
        let catalog = get_catalog()
            .await
            .context("fetching MobilityData catalog")?;

        // Group rows into one `Build` per real agency, keyed by the static feed's
        // id: a static row keys on its own id, a realtime row on its
        // `static_reference` (falling back to its own id when it names no static
        // feed). Alongside, remember each static feed's catalog status so a
        // static-only agency can later be dropped unless it's `active` —
        // deprecated/inactive/dev feeds are defunct, not agencies we're
        // realistically missing realtime for.
        let mut builds: HashMap<String, Build> = HashMap::new();
        let mut gtfs_status: HashMap<String, Status> = HashMap::new();
        for source in &catalog {
            if source.data_type == DataType::Gtfs {
                gtfs_status.insert(source.id.clone(), source.status);
            }
            let key = match source.data_type {
                DataType::Gtfs => source.id.clone(),
                DataType::GtfsRt => source
                    .static_reference
                    .clone()
                    .unwrap_or_else(|| source.id.clone()),
            };
            builds.entry(key).or_default().fold(source);
        }

        // Turn each group into a complete AgencyConfig. Most agencies expose a
        // static *or* a realtime feed but not both paired; that's expected at
        // this scale, so we don't log each one.
        //
        // A group that has a static schedule but no realtime trip-updates feed
        // isn't dropped: we still surface it (with an empty realtime URL set) so
        // it shows up in `/status` as `no_realtime`, making a large agency the
        // catalog is missing GTFS-realtime for (like NJ Transit) visible rather
        // than silently absent. Only groups with neither a trip-updates feed nor
        // a static schedule are truly skipped.
        let mut agencies = Vec::new();
        let mut static_only = 0usize;
        let mut dropped_inactive = 0usize;
        let mut skipped = 0usize;
        for (slug, build) in builds {
            let Some(static_url) = build.static_url else {
                skipped += 1;
                continue;
            };
            if build.trip_updates_url.is_empty() {
                // Static-only: keep it as a `no_realtime` config, but only if the
                // static feed is still active.
                let active = gtfs_status
                    .get(&slug)
                    .is_some_and(|status| *status == Status::Active);
                if !active {
                    dropped_inactive += 1;
                    continue;
                }
                static_only += 1;
            }
            agencies.push(AgencyConfig {
                slug,
                display_name: build.display_name.unwrap_or_default(),
                static_url,
                realtime_urls: GtfsRtUrls {
                    trip_updates_url: dedup(build.trip_updates_url),
                    vehicle_positions_url: dedup(build.vehicle_positions_url),
                },
                gtfs_rt_requires_auth: build.gtfs_rt_requires_auth,
                country_code: build.country_code,
                location: build.location,
            });
        }

        println!(
            "MobilityData catalog: {} agencies ({static_only} active static-only, no realtime; \
             {dropped_inactive} inactive static-only dropped; \
             {skipped} entries had neither a paired realtime nor a static feed)",
            agencies.len(),
        );

        Ok(MobilityDataProvider { agencies })
    }
}

impl GtfsCatalogProvider for MobilityDataProvider {
    fn get_agencies(&self) -> Vec<AgencyConfig> {
        self.agencies.clone()
    }
}
