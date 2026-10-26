//! The Antigravity client version and user agents (internal/misc/antigravity_version.go).
//!
//! Requests present themselves as the Antigravity Hub desktop app. Its version comes
//! from the Hub auto-updater manifest, refreshed every three hours once [`start`] runs;
//! until a fetch succeeds, and whenever the cached value is older than six hours, the
//! built-in fallback is used.

use std::sync::{Once, RwLock};
use std::time::{Duration, Instant};

use crate::proxy::{self, GoClients, GoHeaders, Hooks, Proxy};

/// `antigravityFallbackVersion`.
pub const FALLBACK_VERSION: &str = "2.23.0";
const HUB_PLATFORM: &str = "darwin/arm64";
const CACHE_TTL: Duration = Duration::from_secs(6 * 3600);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// `AntigravityNodeAPIClientUA`.
pub const NODE_API_CLIENT_UA: &str = "google-api-nodejs-client/10.3.0";
/// `AntigravityGoogAPIClientUA`.
pub const GOOG_API_CLIENT_UA: &str = "gl-node/22.21.1";
/// `antigravityHubLatestManifestURL`.
pub const MANIFEST_URL: &str =
    "https://antigravity-hub-auto-updater-974169037036.us-central1.run.app/manifest/latest-arm64-mac.yml";
/// The manifest bytes read (`io.LimitReader(resp.Body, 4096)`).
const MANIFEST_LIMIT: usize = 4096;

/// The cached version and when it stops being trusted (`None`: never fetched).
struct Cached {
    version: String,
    expiry: Option<Instant>,
}

fn never_fetched() -> RwLock<Cached> {
    RwLock::new(Cached {
        version: FALLBACK_VERSION.into(),
        expiry: None,
    })
}

#[cfg(not(test))]
fn cached() -> &'static RwLock<Cached> {
    static CACHED: std::sync::LazyLock<RwLock<Cached>> = std::sync::LazyLock::new(never_fetched);
    &CACHED
}

/// In unit tests each thread has its own cache, so a test that sets a version never
/// changes the user agent of requests other tests compare with Go's.
#[cfg(test)]
fn cached() -> &'static RwLock<Cached> {
    thread_local! {
        static CACHED: &'static RwLock<Cached> = Box::leak(Box::new(never_fetched()));
    }
    CACHED.with(|cached| *cached)
}

fn read() -> std::sync::RwLockReadGuard<'static, Cached> {
    cached().read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write() -> std::sync::RwLockWriteGuard<'static, Cached> {
    cached().write().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `StartAntigravityVersionUpdater`: once per process, a task that fetches the manifest
/// now and every three hours. Call from within a Tokio runtime.
pub fn start() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        tokio::spawn(async {
            let interval = CACHE_TTL / 2;
            tracing::info!("periodic antigravity version refresh started (interval=3h0m0s)");
            let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
            refresh(fetch_latest(MANIFEST_URL).await, Instant::now());
            loop {
                ticker.tick().await;
                refresh(fetch_latest(MANIFEST_URL).await, Instant::now());
            }
        });
    });
}

/// `refreshAntigravityVersion` with the fetch's outcome at `now`: a fetched version is
/// trusted for six hours; a failure keeps a still-trusted value, else falls back.
pub(crate) fn refresh(fetched: Result<String, String>, now: Instant) {
    let mut cached = write();
    match fetched {
        Ok(version) => {
            tracing::info!(version = %version, "fetched latest antigravity version");
            cached.version = version;
            cached.expiry = Some(now + CACHE_TTL);
        }
        Err(error) if cached.version.is_empty() || cached.expiry.is_none_or(|e| now > e) => {
            tracing::warn!(%error, "failed to refresh antigravity version, using fallback version");
            cached.version = FALLBACK_VERSION.into();
            cached.expiry = Some(now + CACHE_TTL);
        }
        Err(error) => {
            tracing::debug!(%error, "failed to refresh antigravity version, keeping cached value");
        }
    }
}

/// `AntigravityLatestVersion`: the cached version while trusted, else the fallback.
pub fn latest_version() -> String {
    let cached = read();
    match cached.expiry {
        Some(expiry) if !cached.version.is_empty() && Instant::now() < expiry => cached.version.clone(),
        _ => FALLBACK_VERSION.into(),
    }
}

/// `AntigravityUserAgent`.
pub fn user_agent() -> String {
    format!("antigravity/hub/{} {HUB_PLATFORM}", latest_version())
}

fn antigravity_family(lower: &str) -> bool {
    lower.starts_with("antigravity/hub/") || lower.starts_with("antigravity/")
}

/// `antigravityBaseUserAgent`: a configured user agent without its Node client suffix,
/// or the Hub user agent when none is configured.
// ponytail: matching uses ASCII lower case, so offsets stay valid in the original. Go's
// strings.ToLower can change byte lengths, which shifts its cut when a non-ASCII
// character precedes the suffix; every token matched here is ASCII.
fn base_user_agent(user_agent: &str) -> String {
    let user_agent = user_agent.trim();
    if user_agent.is_empty() {
        return self::user_agent();
    }
    let lower = user_agent.to_ascii_lowercase();
    if antigravity_family(&lower)
        && let Some(index) = lower.find(" google-api-nodejs-client/")
    {
        let trimmed = user_agent[..index].trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    user_agent.to_owned()
}

/// `AntigravityRequestUserAgent`.
pub fn request_user_agent(user_agent: &str) -> String {
    base_user_agent(user_agent)
}

/// `AntigravityLoadCodeAssistUserAgent`.
pub fn load_code_assist_user_agent(user_agent: &str) -> String {
    request_user_agent(user_agent)
}

/// `AntigravityOnboardUserUserAgent`: the Antigravity user agent with the Node API
/// client suffix; a non-Antigravity configured value is kept as is.
pub fn onboard_user_user_agent(user_agent: &str) -> String {
    let user_agent = user_agent.trim();
    if user_agent.is_empty() {
        return format!("{} {NODE_API_CLIENT_UA}", self::user_agent());
    }
    let lower = user_agent.to_ascii_lowercase();
    if !antigravity_family(&lower) || lower.contains("google-api-nodejs-client/") {
        return user_agent.to_owned();
    }
    format!("{} {NODE_API_CLIENT_UA}", base_user_agent(user_agent))
}

/// `AntigravityVersionFromUserAgent`: the version token of an Antigravity user agent,
/// else the latest version.
pub fn version_from_user_agent(user_agent: &str) -> String {
    let base = base_user_agent(user_agent);
    let lower = base.to_ascii_lowercase();
    let rest = if lower.starts_with("antigravity/hub/") {
        &base["antigravity/hub/".len()..]
    } else if lower.starts_with("antigravity/") {
        &base["antigravity/".len()..]
    } else {
        return latest_version();
    };
    let token = rest.split([' ', '\t']).next().unwrap_or_default().trim();
    if token.is_empty() {
        latest_version()
    } else {
        token.to_owned()
    }
}

/// `isValidAntigravitySemVersion`: three dot-separated runs of ASCII digits.
fn valid_version(version: &str) -> bool {
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// `fetchAntigravityLatestVersion`: GET the manifest (a client with Go's default
/// transport: environment proxies, ten-second timeout) and read its `version`.
pub(crate) async fn fetch_latest(url: &str) -> Result<String, String> {
    let client = GoClients::new(Hooks::default()).get(&Proxy::Inherit);
    match tokio::time::timeout(FETCH_TIMEOUT, fetch_manifest(&client, url)).await {
        Ok(result) => result,
        Err(_) => Err("fetch antigravity Hub updater manifest: context deadline exceeded".into()),
    }
}

async fn fetch_manifest(client: &wreq::Client, url: &str) -> Result<String, String> {
    let mut headers = GoHeaders::new();
    headers.set("User-Agent", "electron-builder");
    headers.set("Cache-Control", "no-cache");
    let upstream = proxy::request(client, wreq::Method::GET, url, headers, None, None)
        .await
        .map_err(|e| {
            format!(
                "fetch antigravity Hub updater manifest: {}",
                String::from_utf8_lossy(&e.body)
            )
        })?;
    if upstream.status != 200 {
        return Err(format!(
            "antigravity Hub updater manifest returned status {}",
            upstream.status
        ));
    }
    let raw = proxy::read_all(upstream.body, MANIFEST_LIMIT, false)
        .await
        .map_err(|e| {
            format!(
                "read antigravity Hub updater manifest: {}",
                String::from_utf8_lossy(&e.body)
            )
        })?;
    manifest_version(&raw)
}

/// The manifest's `version` as yaml.v3 decodes it into a string field: any scalar's
/// text; a document that is not a mapping, or not YAML, fails.
fn manifest_version(raw: &[u8]) -> Result<String, String> {
    use serde_yaml_ng::Value;
    let decode_error = |e: String| format!("decode antigravity Hub updater manifest: {e}");
    let document: Value = if raw.iter().all(u8::is_ascii_whitespace) {
        Value::Null
    } else {
        serde_yaml_ng::from_slice(raw).map_err(|e| decode_error(e.to_string()))?
    };
    let version = match &document {
        Value::Null => String::new(),
        Value::Mapping(map) => match map.get("version") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::Bool(b)) => b.to_string(),
            Some(_) => return Err(decode_error("cannot unmarshal a collection into a string".into())),
        },
        _ => return Err(decode_error("cannot unmarshal the document into the manifest".into())),
    };
    let version = version.trim();
    if version.is_empty() {
        return Err("antigravity Hub updater manifest returned empty version".into());
    }
    if !valid_version(version) {
        return Err(format!(
            "antigravity Hub updater manifest returned invalid version {}",
            cpa_common::gostr::quote(version)
        ));
    }
    Ok(version.to_owned())
}

#[cfg(test)]
#[path = "antigravity_version_tests.rs"]
mod tests;
