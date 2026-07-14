use crate::{
    agency::{AgencyConfig, GeoPoint, GtfsRtUrlSet, PartialAgencyConfig, PartialGtfsRtUrlSet},
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

impl MobilityDataProvider {
    pub async fn new() -> Result<Self, anyhow::Error> {
        let catalog = get_catalog()
            .await
            .context("fetching MobilityData catalog")?;

        // Turn each catalog entry into a PartialAgencyConfig. Alongside, remember
        // each static feed's catalog status keyed by its id (which is also its
        // merge key), so a static-only feed can later be dropped unless it's
        // `active` — deprecated/inactive/dev feeds are defunct, not agencies
        // we're realistically missing realtime for.
        let mut gtfs_status: std::collections::HashMap<String, Status> =
            std::collections::HashMap::new();
        let mut partial_agencies = Vec::new();
        for source in catalog {
            if source.data_type == DataType::Gtfs {
                gtfs_status.insert(source.id.clone(), source.status);
            }
            // The catalog `name` is often blank; fall back to the operator name
            // and then the feed id so the leaderboard never shows an empty label.
            let display_name = [source.name.trim(), source.provider.trim()]
                .into_iter()
                .find(|s| !s.is_empty())
                .unwrap_or(source.id.as_str())
                .to_string();

            let mut config = PartialAgencyConfig {
                slug: Some(source.id.clone()),
                display_name: Some(display_name),
                // 0 is no authentication, 1/2 are basic/digest
                gtfs_rt_requires_auth: source.authentication_type.map(|t| matches!(t, 1 | 2)),
                country_code: Some(source.country_code.clone()),
                location: bbox_center(&source),
                ..Default::default()
            };

            if source.data_type == DataType::Gtfs {
                config.static_url = source.direct_download.clone();
            } else {
                let realtime_types = if source.entity_type.is_empty() {
                    vec![
                        RealtimeEntity::TripUpdates,
                        RealtimeEntity::VehiclePositions,
                        RealtimeEntity::ServiceAlerts,
                    ]
                } else {
                    source.entity_type
                };

                let url = source.direct_download.clone();
                let url_for_entity = |entity: RealtimeEntity| {
                    realtime_types
                        .contains(&entity)
                        .then_some(url.clone())
                        .flatten()
                };

                config.realtime_urls = Some(PartialGtfsRtUrlSet::Separate {
                    trip_updates_url: url_for_entity(RealtimeEntity::TripUpdates)
                        .into_iter()
                        .collect(),
                    vehicle_positions_url: url_for_entity(RealtimeEntity::VehiclePositions)
                        .into_iter()
                        .collect(),
                    service_alerts_url: url_for_entity(RealtimeEntity::ServiceAlerts)
                        .into_iter()
                        .collect(),
                });
                config.static_reference = source.static_reference.clone();
            }

            partial_agencies.push(config);
        }

        // Find duplicates by their static agency ID (slug) and merge them together
        let mut merged_agencies = std::collections::HashMap::new();
        for mut partial in partial_agencies {
            // Unify under the static feed's ID so static and real-time feeds merge seamlessly.
            let key = partial
                .static_reference
                .clone()
                .or_else(|| partial.slug.clone());

            // Normalize the slug so that when merged, we always keep the static feed's ID.
            partial.slug = key.clone();

            merged_agencies
                .entry(key)
                .and_modify(|existing: &mut PartialAgencyConfig| {
                    *existing = existing.merge_other(&partial).unwrap_or_else(|err| {
                        eprintln!(
                            "[{}] failed to merge duplicate agency configs: {err:#}",
                            existing
                                .display_name
                                .clone()
                                .unwrap_or_else(|| "unknown".to_string())
                        );
                        existing.clone()
                    });
                })
                .or_insert(partial);
        }

        // Upgrade every merged config to a complete AgencyConfig. Most catalog
        // entries only supply a static *or* a realtime feed and can't be fully
        // upgraded — that's expected at this scale, so we don't log each one.
        //
        // A merged entry that has a static schedule but no paired realtime feed
        // isn't dropped: we still surface it (with an empty realtime URL set) so
        // it shows up in `/status` as `no_realtime`, making a large agency the
        // catalog is missing GTFS-realtime for (like NJ Transit) visible rather
        // than silently absent. Only entries with neither a poll-able realtime
        // feed nor a static schedule are truly skipped.
        let mut complete_agencies = Vec::new();
        let mut static_only = 0usize;
        let mut dropped_inactive = 0usize;
        let mut skipped = 0usize;
        for partial in merged_agencies.into_values() {
            match partial.upgrade_to_complete() {
                Ok(complete) => complete_agencies.push(complete),
                Err(_) => match partial.static_url {
                    Some(static_url) => {
                        let active = partial
                            .slug
                            .as_deref()
                            .and_then(|id| gtfs_status.get(id))
                            .is_some_and(|status| *status == Status::Active);
                        if !active {
                            dropped_inactive += 1;
                            continue;
                        }
                        static_only += 1;
                        complete_agencies.push(AgencyConfig {
                            slug: partial.slug.unwrap_or_default(),
                            display_name: partial.display_name.unwrap_or_default(),
                            static_url,
                            realtime_urls: GtfsRtUrlSet::Separate {
                                trip_updates_url: Vec::new(),
                                vehicle_positions_url: Vec::new(),
                                service_alerts_url: Vec::new(),
                            },
                            gtfs_rt_requires_auth: partial.gtfs_rt_requires_auth,
                            country_code: partial.country_code,
                            location: partial.location,
                        });
                    }
                    None => skipped += 1,
                },
            }
        }

        println!(
            "MobilityData catalog: {} agencies ({static_only} active static-only, no realtime; \
             {dropped_inactive} inactive static-only dropped; \
             {skipped} entries had neither a paired realtime nor a static feed)",
            complete_agencies.len(),
        );

        Ok(MobilityDataProvider {
            agencies: complete_agencies,
        })
    }
}

impl GtfsCatalogProvider for MobilityDataProvider {
    fn get_agencies(&self) -> Vec<AgencyConfig> {
        self.agencies.clone()
    }
}
