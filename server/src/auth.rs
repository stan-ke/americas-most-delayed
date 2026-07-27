//! Authentication for GTFS-realtime feeds that sit behind an API key.
//!
//! Most feeds we poll are open, but a handful gate their realtime data behind a
//! credential — an HTTP header (STM, OC Transpo), a query parameter (MTA Bus Time,
//! the Puget Sound OneBusAway server), or a value baked into the URL path (TriMet's
//! app id). This module is the one place that knows how those credentials are
//! applied, but it knows *nothing* hard-coded about which agencies they belong to.
//!
//! **The rules are data, not code.** Both the host→credential injection rules and
//! the hand-configured gated agencies live in a checked-in, secret-free
//! [`auth.json`](../../auth.json) (path overridable with `AMD_AUTH_FILE`). Adding a
//! new authenticated agency is a JSON edit plus a secret in `keys.env` — no
//! recompile. See [`AuthConfig`] for the schema.
//!
//! **Secrets never live in the source tree, nor in `auth.json`.** They're read at
//! startup from a git-ignored `keys.env` file (`KEY=value` lines, `#` comments), so
//! the operator drops the keys they were handed into `keys.env` and nothing
//! sensitive is ever committed. The path defaults to `./keys.env` and is overridable
//! with `AMD_KEYS_FILE`. `auth.json` and the agency configs reference credentials
//! only by *name*; the value is looked up in `keys.env` at fetch/build time.
//!
//! The design is deliberately decoupled from the catalog: [`FeedAuth::apply`]
//! matches an outbound request by its URL host and injects the right credential, so
//! *any* request pointed at a known host is authenticated. Path-embedded credentials
//! are spliced in via `{KEY}` placeholders in an agency's URLs (see
//! [`FeedAuth::authed_agencies`]), so even those need no bespoke Rust.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use reqwest::{Client, RequestBuilder};
use serde::Deserialize;

use crate::agency::{AgencyConfig, GeoPoint, GtfsRtUrls};

/// The JSON auth config (`auth.json`): the host→credential injection rules and the
/// hand-configured gated agencies. Holds no secrets — only credential *names*, which
/// are resolved against `keys.env` at runtime. Unknown fields (e.g. `"//"` comment
/// keys) are ignored, so the file can carry documentation inline.
#[derive(Debug, Default, Deserialize)]
struct AuthConfig {
    #[serde(default)]
    injections: Vec<Injection>,
    #[serde(default)]
    agencies: Vec<AuthAgency>,
}

/// How a credential rides on a request.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Inject {
    /// An HTTP request header, e.g. `{"header": "apiKey"}` → `apiKey: <secret>`.
    Header(String),
    /// A URL query parameter, e.g. `{"query": "key"}` → `?key=<secret>`.
    Query(String),
}

/// One host→credential rule: a request whose URL host matches `host` gets the
/// secret named `key` injected per `inject`.
///
/// Path-embedded credentials (TriMet bakes its app id into the URL path) aren't
/// here — they use a `{KEY}` placeholder in the agency's URLs instead, substituted
/// when the config is built (see [`FeedAuth::authed_agencies`]).
#[derive(Debug, Clone, Deserialize)]
struct Injection {
    host: String,
    key: String,
    inject: Inject,
}

/// A hand-configured feed the public catalogs gate behind auth (we hold a key) or
/// mislabel as having no realtime. Mirrors [`AgencyConfig`], plus:
///
/// - `requires_key`: when set, the agency is dropped unless that credential is
///   present in `keys.env` — so a missing key silently omits the agency rather than
///   polling it into a `401`. `null`/absent means always built (e.g. MTA Subway,
///   which is open but which the catalogs wrongly mark `no_realtime`).
/// - Any `{KEY}` placeholder in a URL is substituted from `keys.env` at build time;
///   an unresolved placeholder drops the agency. This is how path-embedded
///   credentials are injected without bespoke code.
#[derive(Debug, Clone, Deserialize)]
struct AuthAgency {
    slug: String,
    display_name: String,
    #[serde(default)]
    requires_key: Option<String>,
    static_url: String,
    /// Extra static GTFS zips merged into the same schedule index as `static_url`
    /// (e.g. MTA Bus's other-borough schedules). See
    /// [`AgencyConfig::extra_static_urls`].
    #[serde(default)]
    extra_static_urls: Vec<String>,
    #[serde(default)]
    trip_updates_urls: Vec<String>,
    #[serde(default)]
    vehicle_positions_urls: Vec<String>,
    #[serde(default)]
    country_code: Option<String>,
    /// `[lat, lon]` in degrees, used only for cross-catalog dedup.
    #[serde(default)]
    location: Option<[f64; 2]>,
}

/// Feed credentials loaded from `keys.env`, plus the rules loaded from `auth.json`
/// and the machinery to apply them.
pub struct FeedAuth {
    secrets: HashMap<String, String>,
    injections: Vec<Injection>,
    agencies: Vec<AuthAgency>,
}

impl FeedAuth {
    /// Load credentials from `keys.env` (path overridable with `AMD_KEYS_FILE`) and
    /// rules from `auth.json` (path overridable with `AMD_AUTH_FILE`).
    ///
    /// Neither file being present is an error — a missing `keys.env` just means no
    /// credentials are configured (gated feeds skipped), and a missing/invalid
    /// `auth.json` means no injection rules or hand-configured agencies. Prints a
    /// summary of which credentials were found, never their values.
    pub fn load() -> Self {
        let keys_path = std::env::var("AMD_KEYS_FILE").unwrap_or_else(|_| "keys.env".to_string());
        let auth_path = std::env::var("AMD_AUTH_FILE").unwrap_or_else(|_| "auth.json".to_string());
        let config = load_config(Path::new(&auth_path));
        let auth = FeedAuth {
            secrets: read_env_file(Path::new(&keys_path)),
            injections: config.injections,
            agencies: config.agencies,
        };
        auth.log_summary(&keys_path);
        auth
    }

    /// The secret for `key`, if configured and non-empty.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.secrets
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    /// Whether a secret is configured for `key`.
    pub fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Build a GET request for `url` with any matching header/query credential
    /// applied. A URL whose host matches no rule — the overwhelming majority of
    /// feeds — gets no credential, so this is cheap and safe to call on *every*
    /// outbound feed request rather than only the ones we know are gated.
    ///
    /// Query credentials are spliced into the URL directly (rather than via the
    /// request builder) so the same code path works regardless of which reqwest
    /// features are enabled.
    pub fn apply(&self, client: &Client, url: &str) -> RequestBuilder {
        let mut final_url = url.to_string();
        let mut headers: Vec<(&str, &str)> = Vec::new();
        for inj in &self.injections {
            if url_has_host(url, &inj.host)
                && let Some(secret) = self.get(&inj.key)
            {
                match &inj.inject {
                    Inject::Header(name) => headers.push((name.as_str(), secret)),
                    Inject::Query(name) => final_url = append_query(&final_url, name, secret),
                }
            }
        }
        let mut req = client.get(&final_url);
        for (name, value) in headers {
            req = req.header(name, value);
        }
        req
    }

    /// Build the hand-configured gated agencies from `auth.json`. An agency with a
    /// `requires_key` whose secret is absent is silently skipped; an agency with an
    /// unresolved `{KEY}` placeholder in any URL is skipped with a warning. The
    /// result is prepended after NJ Transit in `main::collect_agencies`, so these win
    /// cross-catalog dedup over the catalogs' `requires_auth` / `no_realtime`
    /// versions of the same agency.
    pub fn authed_agencies(&self) -> Vec<AgencyConfig> {
        let mut configs = Vec::new();
        for agency in &self.agencies {
            if let Some(key) = &agency.requires_key
                && !self.has(key)
            {
                continue;
            }
            let (
                Some(static_url),
                Some(extra_static_urls),
                Some(trip_updates_url),
                Some(vehicle_positions_url),
            ) = (
                self.substitute(&agency.static_url),
                self.substitute_all(&agency.extra_static_urls),
                self.substitute_all(&agency.trip_updates_urls),
                self.substitute_all(&agency.vehicle_positions_urls),
            )
            else {
                eprintln!(
                    "Feed auth: skipping '{}' — unresolved credential placeholder in a URL",
                    agency.slug
                );
                continue;
            };
            configs.push(AgencyConfig {
                slug: agency.slug.clone(),
                display_name: agency.display_name.clone(),
                static_url,
                extra_static_urls,
                realtime_urls: GtfsRtUrls {
                    trip_updates_url,
                    vehicle_positions_url,
                },
                gtfs_rt_requires_auth: Some(false),
                country_code: agency.country_code.clone(),
                location: agency.location.map(|[lat, lon]| GeoPoint::new(lat, lon)),
            });
        }
        configs
    }

    /// Replace every `{KEY}` placeholder in `template` with the secret named `KEY`.
    /// Returns `None` if any referenced secret is missing or empty, so an agency with
    /// a broken URL is dropped rather than polled with a literal `{KEY}` in it. A
    /// template with no placeholders passes through unchanged.
    fn substitute(&self, template: &str) -> Option<String> {
        let mut out = String::with_capacity(template.len());
        let mut rest = template;
        while let Some(start) = rest.find('{') {
            out.push_str(&rest[..start]);
            let after = &rest[start + 1..];
            let end = after.find('}')?;
            out.push_str(self.get(&after[..end])?);
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        Some(out)
    }

    /// [`Self::substitute`] over a list; `None` if any entry has an unresolved
    /// placeholder.
    fn substitute_all(&self, templates: &[String]) -> Option<Vec<String>> {
        templates.iter().map(|t| self.substitute(t)).collect()
    }

    /// Every credential name the config references — the union of injection-rule
    /// keys, agency `requires_key`s, and `{KEY}` placeholders in agency URLs. Used
    /// only for the startup summary, so a credential present in `keys.env` but unused
    /// (or referenced but missing) is visible at a glance.
    fn known_keys(&self) -> Vec<String> {
        let mut keys = BTreeSet::new();
        for inj in &self.injections {
            keys.insert(inj.key.clone());
        }
        for agency in &self.agencies {
            if let Some(key) = &agency.requires_key {
                keys.insert(key.clone());
            }
            let urls = std::iter::once(&agency.static_url)
                .chain(&agency.extra_static_urls)
                .chain(&agency.trip_updates_urls)
                .chain(&agency.vehicle_positions_urls);
            for url in urls {
                collect_placeholders(url, &mut keys);
            }
        }
        keys.into_iter().collect()
    }

    /// Print which referenced credentials were found (by name — never the value).
    fn log_summary(&self, path: &str) {
        let known = self.known_keys();
        let present: Vec<&String> = known.iter().filter(|k| self.has(k)).collect();
        if present.is_empty() {
            println!("Feed auth: no credentials loaded from {path} (gated feeds will be skipped)");
        } else {
            println!(
                "Feed auth: loaded {} credential(s) from {path}: {}",
                present.len(),
                present
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let missing: Vec<&String> = known.iter().filter(|k| !self.has(k)).collect();
            if !missing.is_empty() {
                println!(
                    "Feed auth: no value for: {}",
                    missing
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
    }
}

/// Load and parse `auth.json`. A missing file yields an empty config (no rules, no
/// hand-configured agencies); a malformed file is logged and likewise treated as
/// empty, so a JSON typo degrades gracefully rather than aborting startup.
fn load_config(path: &Path) -> AuthConfig {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) => {
            println!(
                "Feed auth: no rules loaded from {} ({err}) — auth injections and \
                 hand-configured feeds disabled",
                path.display()
            );
            return AuthConfig::default();
        }
    };
    match serde_json::from_str(&contents) {
        Ok(config) => config,
        Err(err) => {
            eprintln!(
                "Feed auth: {} is malformed ({err}) — auth injections and \
                 hand-configured feeds disabled",
                path.display()
            );
            AuthConfig::default()
        }
    }
}

/// Add every `{KEY}` placeholder name in `template` to `keys`.
fn collect_placeholders(template: &str, keys: &mut BTreeSet<String>) {
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else { break };
        keys.insert(after[..end].to_string());
        rest = &after[end + 1..];
    }
}

/// Parse a `keys.env`-style file into a map. `KEY=value` lines; `#` comments and
/// blank lines ignored; surrounding whitespace and a single pair of double quotes
/// around the value are stripped. A missing/unreadable file yields an empty map.
fn read_env_file(path: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(contents) = std::fs::read_to_string(path) else {
        return map;
    };
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim().to_string();
            let value = value.trim().trim_matches('"').to_string();
            if !key.is_empty() {
                map.insert(key, value);
            }
        }
    }
    map
}

/// Append a `name=value` query parameter to a URL, choosing `?` or `&` as needed
/// and percent-encoding the value so an odd character in a key can't corrupt the
/// query string.
fn append_query(url: &str, name: &str, value: &str) -> String {
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}{name}={}", encode_query_value(value))
}

/// Percent-encode a query-parameter value, leaving the URL-unreserved set intact.
fn encode_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Whether `url`'s host is `host` (or a subdomain of it). Compares the authority
/// only, so a host name appearing in the path or query never counts as a match.
fn url_has_host(url: &str, host: &str) -> bool {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    // Drop any `userinfo@` prefix and `:port` suffix.
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let hostname = host_port.split(':').next().unwrap_or(host_port);
    hostname == host || hostname.ends_with(&format!(".{host}"))
}
