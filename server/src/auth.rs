//! Authentication for GTFS-realtime feeds that sit behind an API key.
//!
//! Most feeds we poll are open, but a handful gate their realtime data behind a
//! credential — an HTTP header (STM, OC Transpo), a query parameter (MTA Bus Time,
//! the Puget Sound OneBusAway server), or a value baked into the URL path (TriMet's
//! app id). This module is the one place that knows about those credentials.
//!
//! **Secrets never live in the source tree.** They're read at startup from a
//! git-ignored `keys.env` file (`KEY=value` lines, `#` comments), so the operator
//! drops the keys they were handed into `keys.env` and nothing sensitive is ever
//! committed. The path defaults to `./keys.env` and is overridable with
//! `AMD_KEYS_FILE` (handy when running from a different working directory).
//!
//! The design is deliberately decoupled from the catalog: [`FeedAuth::apply`]
//! matches an outbound request by its URL host and injects the right credential, so
//! *any* request pointed at a known host is authenticated — whether the config was
//! hand-written (see [`crate::agency::authed_agencies`]) or came from a catalog.
//! Adding a new header/query-authenticated agency is two edits: drop its secret in
//! `keys.env`, and add one line to [`INJECTIONS`].

use std::collections::HashMap;
use std::path::Path;

use reqwest::{Client, RequestBuilder};

// Names of the credentials, as they appear in `keys.env`. Public so config
// construction can read a path-embedded secret (TriMet) or gate a feed on whether
// its key is present.
pub const STM_API_KEY: &str = "STM_API_KEY";
pub const OCTRANSPO_API_KEY: &str = "OCTRANSPO_API_KEY";
pub const MTA_BUSTIME_KEY: &str = "MTA_BUSTIME_KEY";
pub const PUGET_SOUND_KEY: &str = "PUGET_SOUND_KEY";
pub const TRIMET_APP_ID: &str = "TRIMET_APP_ID";

/// Every credential name we know about — used only for the startup summary, so a
/// key present in `keys.env` but missing (or vice versa) is visible at a glance.
const KNOWN_KEYS: &[&str] = &[
    STM_API_KEY,
    OCTRANSPO_API_KEY,
    MTA_BUSTIME_KEY,
    PUGET_SOUND_KEY,
    TRIMET_APP_ID,
];

/// How a credential rides on a request.
#[derive(Clone, Copy)]
enum Inject {
    /// An HTTP request header, e.g. `apiKey: <secret>`.
    Header(&'static str),
    /// A URL query parameter, e.g. `?key=<secret>`.
    Query(&'static str),
}

/// One host→credential rule: a request whose URL host matches `host` gets the
/// secret named `key` injected per `inject`.
///
/// Path-embedded credentials (TriMet bakes its app id into the URL path) aren't
/// here — there's no generic way to splice a value into an arbitrary path, so those
/// are built into the URL when the config is constructed.
struct Injection {
    host: &'static str,
    key: &'static str,
    inject: Inject,
}

/// The header/query credentials, keyed by the host they authenticate. Add a line
/// to authenticate a new host; its secret is looked up by `key` in `keys.env`, and
/// the rule stays inert until that secret is present.
const INJECTIONS: &[Injection] = &[
    Injection {
        host: "api.stm.info",
        key: STM_API_KEY,
        inject: Inject::Header("apiKey"),
    },
    Injection {
        host: "nextrip-public-api.azure-api.net",
        key: OCTRANSPO_API_KEY,
        inject: Inject::Header("Ocp-Apim-Subscription-Key"),
    },
    Injection {
        host: "gtfsrt.prod.obanyc.com",
        key: MTA_BUSTIME_KEY,
        inject: Inject::Query("key"),
    },
    Injection {
        host: "api.pugetsound.onebusaway.org",
        key: PUGET_SOUND_KEY,
        inject: Inject::Query("key"),
    },
];

/// Feed credentials loaded from `keys.env`, plus the machinery to apply them.
pub struct FeedAuth {
    secrets: HashMap<String, String>,
}

impl FeedAuth {
    /// Load credentials from `keys.env` (path overridable with `AMD_KEYS_FILE`).
    ///
    /// A missing file is not an error — it just means no credentials are configured,
    /// so the gated feeds are skipped. Prints a summary of which credentials were
    /// found, never their values.
    pub fn load() -> Self {
        let path = std::env::var("AMD_KEYS_FILE").unwrap_or_else(|_| "keys.env".to_string());
        let auth = FeedAuth {
            secrets: read_env_file(Path::new(&path)),
        };
        auth.log_summary(&path);
        auth
    }

    /// Build directly from a secrets map — used by tests.
    #[cfg(test)]
    pub fn from_secrets(secrets: HashMap<String, String>) -> Self {
        FeedAuth { secrets }
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
        for inj in INJECTIONS {
            if url_has_host(url, inj.host)
                && let Some(secret) = self.get(inj.key)
            {
                match inj.inject {
                    Inject::Header(name) => headers.push((name, secret)),
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

    /// Print which known credentials were found (by name — never the value).
    fn log_summary(&self, path: &str) {
        let present: Vec<&str> = KNOWN_KEYS
            .iter()
            .copied()
            .filter(|k| self.has(k))
            .collect();
        if present.is_empty() {
            println!(
                "Feed auth: no credentials loaded from {path} (gated feeds will be skipped)"
            );
        } else {
            println!(
                "Feed auth: loaded {} credential(s) from {path}: {}",
                present.len(),
                present.join(", ")
            );
            let missing: Vec<&str> = KNOWN_KEYS
                .iter()
                .copied()
                .filter(|k| !self.has(k))
                .collect();
            if !missing.is_empty() {
                println!("Feed auth: no value for: {}", missing.join(", "));
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn auth_with(pairs: &[(&str, &str)]) -> FeedAuth {
        FeedAuth::from_secrets(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    #[test]
    fn injects_header_credential() {
        let auth = auth_with(&[(STM_API_KEY, "SECRET_STM")]);
        let client = reqwest::Client::new();
        let url = "https://api.stm.info/pub/od/gtfs-rt/ic/v2/tripUpdates";
        let req = auth.apply(&client, url).build().unwrap();
        assert_eq!(req.headers().get("apiKey").unwrap(), "SECRET_STM");
    }

    #[test]
    fn injects_query_credential() {
        let auth = auth_with(&[(MTA_BUSTIME_KEY, "SECRET_MTA")]);
        let client = reqwest::Client::new();
        let url = "https://gtfsrt.prod.obanyc.com/tripUpdates";
        let req = auth.apply(&client, url).build().unwrap();
        assert_eq!(
            req.url().query_pairs().find(|(k, _)| k == "key").unwrap().1,
            "SECRET_MTA"
        );
    }

    #[test]
    fn leaves_unknown_and_unconfigured_hosts_untouched() {
        // Known host but no secret configured, and an entirely unknown host.
        let auth = auth_with(&[]);
        let client = reqwest::Client::new();
        for url in [
            "https://api.stm.info/x",
            "https://example.com/gtfs-rt/tripUpdates",
        ] {
            let req = auth.apply(&client, url).build().unwrap();
            assert!(req.headers().get("apiKey").is_none());
            assert!(req.url().query().is_none());
        }
    }

    #[test]
    fn host_matching_is_authority_only() {
        assert!(url_has_host("https://api.stm.info/x?y=1", "api.stm.info"));
        assert!(url_has_host("https://sub.api.stm.info/x", "api.stm.info"));
        // A host that only appears in the path must not match.
        assert!(!url_has_host("https://evil.example/api.stm.info", "api.stm.info"));
        // A different host that merely ends with a similar string must not match.
        assert!(!url_has_host("https://notapi.stm.info.evil.com/x", "api.stm.info"));
    }

    #[test]
    fn env_file_parsing() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("amd_keys_test_{}.env", std::process::id()));
        std::fs::write(
            &path,
            "# a comment\n\nSTM_API_KEY = abc123 \nQUOTED=\"v a l\"\nEMPTY=\n",
        )
        .unwrap();
        let map = read_env_file(&path);
        std::fs::remove_file(&path).ok();
        assert_eq!(map.get("STM_API_KEY").unwrap(), "abc123");
        assert_eq!(map.get("QUOTED").unwrap(), "v a l");
        assert_eq!(map.get("EMPTY").unwrap(), "");
    }

    /// Live end-to-end check that every hand-configured feed authenticates and
    /// decodes. Ignored by default (hits the network and needs a real `keys.env`);
    /// run with `cargo test -- --ignored --nocapture` after setting `AMD_KEYS_FILE`.
    #[tokio::test]
    #[ignore = "hits live agency feeds; needs keys.env (set AMD_KEYS_FILE)"]
    async fn live_feeds_authenticate() {
        let auth = FeedAuth::load();
        let client = reqwest::Client::builder()
            .user_agent("AmericasMostDelayed/1.0 (auth test)")
            .build()
            .unwrap();
        let configs = crate::agency::authed_agencies(&auth);
        assert!(
            !configs.is_empty(),
            "no hand-configured agencies built — is keys.env present and populated?"
        );
        let mut failures = Vec::new();
        for config in &configs {
            for url in &config.realtime_urls.trip_updates_url {
                match crate::realtime::fetch_feed(&client, &auth, url).await {
                    Ok(feed) => println!(
                        "OK   [{}] {url} -> {} entities",
                        config.display_name,
                        feed.entity.len()
                    ),
                    Err(err) => {
                        println!("FAIL [{}] {url}: {err:#}", config.display_name);
                        failures.push(format!("{}: {url}", config.display_name));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "feeds that failed to authenticate/decode: {failures:#?}");
    }
}
