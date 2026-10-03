//! Kimi (.com and .ai) RFC 8628 device login, token refresh and credential files
//! (internal/auth/kimi, sdk/auth/kimi.go).
//!
//! The public functions are the building blocks the CLI flags and the management
//! `kimi-auth-url` flow both need: start, poll, build the record, write it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use chrono::{TimeZone, Utc};
use cpa_core::credential::{Credential, MetadataPatch};
use cpa_core::exec::{ExecError, FailureScope};
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use serde_json::{Map, Value, json};

use crate::kimi_http::{BUILD_VERSION, auth_error, go_arch, go_os, hostname, rfc3339_utc};
use crate::proxy::GoHeaders;
use cpa_common::json::GoValue;

pub const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
pub const DOMAIN_COM: &str = "kimi.com";
pub const DOMAIN_AI: &str = "kimi.ai";
pub const OAUTH_HOST_COM: &str = "https://auth.kimi.com";
pub const OAUTH_HOST_AI: &str = "https://auth.kimi.ai";
pub const API_BASE_COM: &str = "https://api.kimi.com/coding";
pub const API_BASE_AI: &str = "https://api.kimi.ai/coding";
/// SDK refresh lead (sdk/auth/kimi.go).
pub const REFRESH_LEAD: Duration = Duration::from_secs(300);
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(5);
const MAX_POLL_DURATION: Duration = Duration::from_secs(15 * 60);
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

pub fn is_ai_domain(domain: &str) -> bool {
    let d = domain.trim().to_ascii_lowercase();
    d == "kimi.ai" || d == "ai" || d == "kimi-ai" || d.ends_with(".kimi.ai")
}

pub fn is_com_domain(domain: &str) -> bool {
    let d = domain.trim().to_ascii_lowercase();
    d == "kimi.com" || d == "com" || d == "kimi" || d.ends_with(".kimi.com")
}

fn host_of(raw: &str) -> String {
    let raw = raw.trim();
    // net/url accepts scheme-relative authorities ("//host/path").
    let absolute = if raw.starts_with("//") {
        format!("http:{raw}")
    } else {
        raw.to_owned()
    };
    url::Url::parse(&absolute)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default()
}

fn is_ai_host(raw: &str) -> bool {
    let host = host_of(raw);
    host == "kimi.ai" || host.ends_with(".kimi.ai")
}

fn is_com_host(raw: &str) -> bool {
    let host = host_of(raw);
    host == "kimi.com" || host.ends_with(".kimi.com")
}

pub fn normalize_domain(domain: &str) -> &'static str {
    if is_ai_domain(domain) { DOMAIN_AI } else { DOMAIN_COM }
}

pub fn oauth_host(domain: &str) -> &'static str {
    if is_ai_domain(domain) {
        OAUTH_HOST_AI
    } else {
        OAUTH_HOST_COM
    }
}

pub fn api_base(domain: &str) -> &'static str {
    if is_ai_domain(domain) {
        API_BASE_AI
    } else {
        API_BASE_COM
    }
}

/// `ResolveKimiDomainFromAuth`: explicit domain, base URL and type win over the provider
/// and finally the file name.
pub fn resolve_domain(credential: &Credential) -> &'static str {
    let classify_domain = |d: &str| {
        if is_ai_domain(d) {
            Some(DOMAIN_AI)
        } else if is_com_domain(d) {
            Some(DOMAIN_COM)
        } else {
            None
        }
    };
    let classify_url = |u: &str| {
        if is_ai_host(u) {
            Some(DOMAIN_AI)
        } else if is_com_host(u) {
            Some(DOMAIN_COM)
        } else {
            None
        }
    };
    let attr = |k: &str| {
        credential
            .attributes
            .get(k)
            .map(String::as_str)
            .filter(|s| !s.is_empty())
    };
    let meta = |k: &str| credential.str(k).filter(|s| !s.trim().is_empty());
    attr("domain")
        .and_then(classify_domain)
        .or_else(|| attr("base_url").and_then(classify_url))
        .or_else(|| meta("domain").and_then(classify_domain))
        .or_else(|| meta("base_url").and_then(classify_url))
        .or_else(|| meta("type").and_then(classify_domain))
        .or_else(|| classify_domain(&credential.provider))
        .unwrap_or_else(|| {
            let id = credential.id.to_ascii_lowercase();
            let file = Path::new(&id)
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or_default()
                .to_owned();
            if [&id, &file]
                .iter()
                .any(|s| s.contains("kimi-ai") || s.contains("kimi.ai"))
            {
                DOMAIN_AI
            } else {
                DOMAIN_COM
            }
        })
}

/// Go `url.QueryEscape` for form bodies.
fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Go `url.Values.Encode`: keys sorted.
pub(crate) fn form(pairs: &[(&str, &str)]) -> String {
    let mut pairs = pairs.to_vec();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", query_escape(k), query_escape(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Device-flow device model (`getDeviceModel`), unlike the executor's lowercase form.
fn device_model() -> String {
    match go_os() {
        "darwin" => format!("macOS {}", go_arch()),
        "windows" => format!("Windows {}", go_arch()),
        "linux" => format!("Linux {}", go_arch()),
        os => format!("{os} {}", go_arch()),
    }
}

/// Device authorization response (RFC 8628).
#[derive(Debug, Clone, Default)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub expires_in: i64,
    pub interval: i64,
}

/// Go `json.Unmarshal` into typed fields: missing or null keep the zero value, a wrong
/// JSON type is an error.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct DeviceCodeWire {
    device_code: Option<String>,
    user_code: Option<String>,
    verification_uri: Option<String>,
    verification_uri_complete: Option<String>,
    expires_in: Option<i64>,
    interval: Option<i64>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct TokenWire {
    access_token: Option<String>,
    refresh_token: Option<String>,
    token_type: Option<String>,
    expires_in: Option<f64>,
    scope: Option<String>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct PollWire {
    error: Option<String>,
    error_description: Option<String>,
    #[serde(flatten)]
    token: TokenWire,
}

/// Token endpoint result. `expires_at` is Unix seconds, 0 when the server sent none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_at: i64,
    pub scope: String,
}

/// One device-flow client: fixed domain, OAuth host and device identity.
#[derive(Clone)]
pub struct DeviceFlow {
    client: wreq::Client,
    oauth_host: String,
    device_id: String,
    min_interval: Duration,
}

impl DeviceFlow {
    /// `device_id` empty creates a fresh in-memory UUID, as Go does per login.
    pub fn new(client: wreq::Client, domain: &str, device_id: &str) -> Self {
        let device_id = device_id.trim();
        Self {
            client,
            oauth_host: oauth_host(domain).to_owned(),
            device_id: if device_id.is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                device_id.to_owned()
            },
            min_interval: MIN_POLL_INTERVAL,
        }
    }

    /// Lowers the 5s polling floor so tests do not wait for real time.
    #[cfg(test)]
    pub(crate) fn with_min_interval(mut self, interval: Duration) -> Self {
        self.min_interval = interval;
        self
    }

    /// Points the flow at a local mock. Never derive this from credential metadata.
    pub fn with_oauth_host(mut self, host: &str) -> Self {
        self.oauth_host = host.trim_end_matches('/').to_owned();
        self
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    fn token_url(&self) -> String {
        format!("{}/api/oauth/token", self.oauth_host)
    }

    async fn post(&self, url: &str, body: String) -> Result<(u16, Vec<u8>), ExecError> {
        let mut headers = GoHeaders::new();
        headers.set("User-Agent", "Go-http-client/1.1");
        headers.set("Content-Type", "application/x-www-form-urlencoded");
        headers.set("Accept", "application/json");
        headers.set("X-Msh-Platform", "CLIProxyAPI");
        headers.set("X-Msh-Version", BUILD_VERSION);
        headers.set("X-Msh-Device-Name", hostname().unwrap_or_else(|| "unknown".into()));
        headers.set("X-Msh-Device-Model", device_model());
        headers.set("X-Msh-Device-Id", self.device_id.clone());
        let upstream = crate::proxy::send(&self.client, url, headers, body, Some(Duration::from_secs(30)))
            .await
            .map_err(|_| ExecError::local(502, FailureScope::Transport, "kimi: token request failed"))?;
        let status = upstream.status;
        let body = crate::proxy::read_all(upstream.body, crate::proxy::MAX_ERROR_BODY, false)
            .await
            .map_err(|_| ExecError::local(502, FailureScope::Transport, "kimi: failed to read token response"))?;
        Ok((status, body.to_vec()))
    }

    /// Starts the device flow.
    pub async fn request_device_code(&self) -> Result<DeviceCode, ExecError> {
        let url = format!("{}/api/oauth/device_authorization", self.oauth_host);
        let (status, body) = self.post(&url, form(&[("client_id", CLIENT_ID)])).await?;
        if status != 200 {
            return Err(auth_error(
                status,
                format!("kimi: device code request failed with status {status}"),
            ));
        }
        let wire: DeviceCodeWire =
            serde_json::from_slice(&body).map_err(|_| auth_error(502, "kimi: failed to parse device code response"))?;
        Ok(DeviceCode {
            device_code: wire.device_code.unwrap_or_default(),
            user_code: wire.user_code.unwrap_or_default(),
            verification_uri: wire.verification_uri.unwrap_or_default(),
            verification_uri_complete: wire.verification_uri_complete.unwrap_or_default(),
            expires_in: wire.expires_in.unwrap_or_default(),
            interval: wire.interval.unwrap_or_default(),
        })
    }

    /// One poll. `Ok(None)` means keep polling (authorization_pending / slow_down).
    async fn exchange(&self, device_code: &str) -> Result<Option<Tokens>, ExecError> {
        let body = form(&[
            ("client_id", CLIENT_ID),
            ("device_code", device_code),
            ("grant_type", DEVICE_GRANT),
        ]);
        let (_, body) = self.post(&self.token_url(), body).await?;
        let wire: PollWire =
            serde_json::from_slice(&body).map_err(|_| auth_error(502, "kimi: failed to parse token response"))?;
        match wire.error.as_deref().unwrap_or_default() {
            "" => {}
            "authorization_pending" | "slow_down" => return Ok(None),
            "expired_token" => return Err(auth_error(400, "kimi: device code expired")),
            "access_denied" => return Err(auth_error(403, "kimi: access denied by user")),
            other => {
                let description = wire.error_description.unwrap_or_default();
                return Err(auth_error(400, format!("kimi: OAuth error: {other} - {description}")));
            }
        }
        tokens(wire.token)
            .map(Some)
            .ok_or_else(|| auth_error(502, "kimi: empty access token in response"))
    }

    /// Polls until authorized, denied or expired (interval at least 5s, at most 15 minutes).
    pub async fn poll(&self, code: &DeviceCode) -> Result<Tokens, ExecError> {
        let interval = Duration::from_secs(code.interval.max(0) as u64).max(self.min_interval);
        let mut deadline = tokio::time::Instant::now() + MAX_POLL_DURATION;
        if code.expires_in > 0 {
            deadline = deadline.min(tokio::time::Instant::now() + Duration::from_secs(code.expires_in as u64));
        }
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        loop {
            ticker.tick().await;
            if tokio::time::Instant::now() > deadline {
                return Err(auth_error(400, "kimi: device code expired"));
            }
            if let Some(tokens) = self.exchange(&code.device_code).await? {
                return Ok(tokens);
            }
        }
    }

    /// Refresh grant, single-flighted per token endpoint and refresh token across every
    /// client in the process (kimiRefreshGroup). Callers that go away do not cancel it.
    pub async fn refresh(&self, refresh_token: &str) -> Result<Tokens, ExecError> {
        let refresh_token = refresh_token.trim().to_owned();
        if refresh_token.is_empty() {
            return Err(auth_error(400, "kimi: refresh token is required"));
        }
        type Flight = Shared<BoxFuture<'static, Result<Tokens, ExecError>>>;
        static FLIGHTS: LazyLock<Mutex<HashMap<String, Flight>>> = LazyLock::new(Mutex::default);
        let key = format!("{}:{refresh_token}", self.token_url());
        let flight = {
            let mut flights = FLIGHTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(flight) = flights.get(&key) {
                flight.clone()
            } else {
                let flow = self.clone();
                let task_key = key.clone();
                let task = tokio::spawn(async move {
                    let result = flow.refresh_once(&refresh_token).await;
                    FLIGHTS
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&task_key);
                    result
                });
                let flight = async move { task.await.map_err(|_| auth_error(502, "kimi: refresh task failed"))? }
                    .boxed()
                    .shared();
                flights.insert(key, flight.clone());
                flight
            }
        };
        flight.await
    }

    async fn refresh_once(&self, refresh_token: &str) -> Result<Tokens, ExecError> {
        let body = form(&[
            ("client_id", CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ]);
        let (status, body) = self.post(&self.token_url(), body).await?;
        if status == 401 || status == 403 {
            return Err(auth_error(
                status,
                format!("kimi: refresh token rejected (status {status})"),
            ));
        }
        if status != 200 {
            // ponytail: Go appends the response body; it is withheld here because token
            // endpoints may echo credentials and this message can reach clients.
            return Err(auth_error(status, format!("kimi: refresh failed with status {status}")));
        }
        let wire: TokenWire =
            serde_json::from_slice(&body).map_err(|_| auth_error(502, "kimi: failed to parse refresh response"))?;
        tokens(wire).ok_or_else(|| auth_error(502, "kimi: empty access token in refresh response"))
    }
}

fn tokens(wire: TokenWire) -> Option<Tokens> {
    let access = wire.access_token.unwrap_or_default();
    if access.is_empty() {
        return None;
    }
    let expires_in = wire.expires_in.unwrap_or(0.0);
    Some(Tokens {
        access_token: access,
        refresh_token: wire.refresh_token.unwrap_or_default(),
        token_type: wire.token_type.unwrap_or_default(),
        expires_at: if expires_in > 0.0 {
            Utc::now().timestamp() + expires_in as i64
        } else {
            0
        },
        scope: wire.scope.unwrap_or_default(),
    })
}

fn expiry_string(expires_at: i64) -> Option<String> {
    (expires_at > 0)
        .then(|| Utc.timestamp_opt(expires_at, 0).single().map(rfc3339_utc))
        .flatten()
}

/// Metadata patch for a successful refresh (KimiExecutor.Refresh). `base_url` is the
/// effective base the executor resolved for this credential.
pub fn refresh_patch(credential: &Credential, tokens: &Tokens, base_url: &str, now_local: String) -> MetadataPatch {
    let domain = resolve_domain(credential);
    let mut patch = MetadataPatch::default();
    patch
        .set
        .insert("access_token".into(), tokens.access_token.clone().into());
    if !tokens.refresh_token.is_empty() {
        patch
            .set
            .insert("refresh_token".into(), tokens.refresh_token.clone().into());
    }
    if let Some(expired) = expiry_string(tokens.expires_at) {
        patch.set.insert("expired".into(), expired.into());
    }
    if credential.str("type").is_none_or(str::is_empty) {
        let kind = if is_ai_domain(domain) { "kimi-ai" } else { "kimi" };
        patch.set.insert("type".into(), kind.into());
    }
    if !credential.metadata.contains_key("domain") {
        patch.set.insert("domain".into(), domain.into());
    }
    if !credential.metadata.contains_key("base_url") {
        patch.set.insert("base_url".into(), base_url.into());
    }
    patch.set.insert("last_refresh".into(), now_local.into());
    patch
}

/// A finished login: the credential file name and its JSON object.
pub struct LoginRecord {
    pub file_name: String,
    pub metadata: Map<String, Value>,
    pub label: String,
}

/// Builds the saved credential exactly as Go's token storage plus flattened metadata,
/// including FileStore's `disabled: false`.
pub fn login_record(provider: &str, tokens: &Tokens, device_id: &str, now_ms: i64) -> LoginRecord {
    let ai = provider != "kimi";
    let (domain, prefix, display) = if ai {
        (DOMAIN_AI, "kimi-ai", "Kimi.ai")
    } else {
        (DOMAIN_COM, "kimi", "Kimi")
    };
    let base_url = api_base(domain);
    let mut metadata = Map::new();
    // Typed fields first (KimiTokenStorage tags), then metadata overrides them.
    metadata.insert("access_token".into(), tokens.access_token.clone().into());
    metadata.insert("refresh_token".into(), tokens.refresh_token.clone().into());
    metadata.insert("token_type".into(), tokens.token_type.clone().into());
    if !tokens.scope.is_empty() {
        metadata.insert("scope".into(), tokens.scope.clone().into());
    }
    let device_id = device_id.trim();
    if !device_id.is_empty() {
        metadata.insert("device_id".into(), device_id.into());
    }
    if let Some(expired) = expiry_string(tokens.expires_at) {
        metadata.insert("expired".into(), expired.clone().into());
    }
    metadata.insert("type".into(), provider.into());
    metadata.insert("domain".into(), domain.into());
    metadata.insert("base_url".into(), base_url.into());
    for (k, v) in [
        ("type", json!(provider)),
        ("access_token", json!(tokens.access_token)),
        ("refresh_token", json!(tokens.refresh_token)),
        ("token_type", json!(tokens.token_type)),
        ("scope", json!(tokens.scope)),
        ("timestamp", json!(now_ms)),
        ("domain", json!(domain)),
        ("base_url", json!(base_url)),
        ("disabled", json!(false)),
    ] {
        metadata.insert(k.into(), v);
    }
    LoginRecord {
        file_name: format!("{prefix}-{now_ms}.json"),
        metadata,
        label: format!("{display} User"),
    }
}

/// Go `json.Encoder` output of a credential map: sorted keys, two-space indent, newline.
pub(crate) fn encode_credential(metadata: &Map<String, Value>) -> String {
    String::from_utf8_lossy(&GoValue::from_json(&Value::Object(metadata.clone())).encode_indented()).into_owned()
}

/// Writes a credential file atomically with mode 0600 in a 0700 directory.
///
/// Go uses `os.Create` (0666 before umask) and an in-place write; on Unix a private temp
/// file and rename is the deliberate, safer choice here (contracts review). Windows has
/// no mode bits, so it gets Go's behaviour: create the directory, then create or
/// truncate the file in place.
#[cfg(not(unix))]
pub(crate) fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(path.parent().unwrap_or_else(|| Path::new(".")))?;
    std::fs::write(path, contents)
}

/// Writes a credential file atomically with mode 0600 in a 0700 directory.
///
/// Go uses `os.Create` (0666 before umask) and an in-place write; a private temp file and
/// rename is the deliberate, safer choice here (contracts review).
#[cfg(unix)]
pub(crate) fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(directory)?;
    let mut suffix = [0u8; 8];
    getrandom::fill(&mut suffix).map_err(std::io::Error::other)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("credential");
    let temporary = directory.join(format!(
        ".{name}.{}.tmp",
        suffix.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        std::fs::File::open(directory)?.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Opens `url` in the desktop browser; returns whether a launcher started.
pub(crate) fn open_browser(url: &str) -> bool {
    let launcher = match go_os() {
        "darwin" => "open",
        "windows" => "rundll32",
        _ => "xdg-open",
    };
    let mut command = std::process::Command::new(launcher);
    if go_os() == "windows" {
        command.arg("url.dll,FileProtocolHandler");
    }
    command
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

/// Writes a finished login into `auth_dir` under its Go file name (management
/// `kimi-auth-url`); returns the path.
pub fn write_login(auth_dir: &Path, record: &LoginRecord) -> Result<PathBuf, ExecError> {
    let path = auth_dir.join(&record.file_name);
    write_private(&path, encode_credential(&record.metadata).as_bytes())
        .map_err(|_| auth_error(500, "kimi: cannot write credential file"))?;
    Ok(path)
}

/// `--kimi-login` (`provider` "kimi") or `--kimi-ai-login` ("kimi-ai"): device flow
/// through the configured `requests.proxy-url`, then the credential file in `auth-dir`.
pub async fn login(provider: &str, cfg: &cpa_core::config::Config, no_browser: bool) -> Result<PathBuf, ExecError> {
    let domain = if provider == "kimi" { DOMAIN_COM } else { DOMAIN_AI };
    let proxy = cfg
        .document
        .get("requests")
        .and_then(|r| r.get("proxy-url"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let client = crate::proxy::GoClients::new(crate::proxy::Hooks::default()).get(&crate::proxy::Proxy::parse(proxy));
    let flow = DeviceFlow::new(client, domain, "");
    login_with(flow, provider, &cfg.auth_dir, no_browser).await
}

pub(crate) async fn login_with(
    flow: DeviceFlow,
    provider: &str,
    auth_dir: &Path,
    no_browser: bool,
) -> Result<PathBuf, ExecError> {
    let display = if provider == "kimi" { "Kimi" } else { "Kimi.ai" };
    println!("Starting {display} authentication...");
    let code = flow.request_device_code().await?;
    let url = if code.verification_uri_complete.is_empty() {
        &code.verification_uri
    } else {
        &code.verification_uri_complete
    };
    print!("\nTo authenticate, please visit:\n{url}\n\n");
    if !code.user_code.is_empty() {
        print!("User code: {}\n\n", code.user_code);
    }
    if !no_browser && open_browser(url) {
        println!("Browser opened automatically.");
    }
    println!("Waiting for authorization...");
    if code.expires_in > 0 {
        println!("(This will timeout in {} seconds if not authorized)", code.expires_in);
    }
    let tokens = flow.poll(&code).await?;
    print!("\n{display} authentication successful!\n");
    let record = login_record(provider, &tokens, flow.device_id(), Utc::now().timestamp_millis());
    let path = auth_dir.join(&record.file_name);
    println!("Saving credentials to {}", path.display());
    let contents = encode_credential(&record.metadata);
    let target = path.clone();
    tokio::task::spawn_blocking(move || write_private(&target, contents.as_bytes()))
        .await
        .map_err(|_| auth_error(500, "kimi: credential publication failed"))?
        .map_err(|_| auth_error(500, "kimi: cannot write credential file"))?;
    println!("Authentication saved to {}", path.display());
    println!("Authenticated as {}", record.label);
    println!("{display} authentication successful!");
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(id: &str, provider: &str, meta: Value) -> Credential {
        let mut c = Credential::from_file(
            Path::new("/a"),
            &Path::new("/a").join(id),
            meta.as_object().unwrap().clone(),
        )
        .unwrap();
        c.provider = provider.into();
        c
    }

    #[test]
    fn domain_resolution_follows_go_precedence() {
        // Cases from kimi_test.go TestResolveKimiDomainFromAuth.
        let c = credential(
            "x.json",
            "kimi",
            json!({"type":"kimi","base_url":"https://api.kimi.ai/coding"}),
        );
        assert_eq!(resolve_domain(&c), DOMAIN_AI, "base_url beats type");
        let c = credential("x.json", "kimi", json!({"type":"kimi-ai","domain":"kimi.com"}));
        assert_eq!(resolve_domain(&c), DOMAIN_COM, "domain beats type");
        let c = credential("kimi-ai-1.json", "kimi", json!({"type":"x"}));
        assert_eq!(resolve_domain(&c), DOMAIN_COM, "provider kimi is explicit");
        let c = credential("kimi-ai-1.json", "other", json!({"type":"x"}));
        assert_eq!(resolve_domain(&c), DOMAIN_AI, "file name is the last resort");
        let mut c = credential("x.json", "kimi", json!({"type":"kimi","domain":"kimi.com"}));
        c.attributes.insert("domain".into(), "kimi.ai".into());
        assert_eq!(resolve_domain(&c), DOMAIN_AI, "attributes beat metadata");
        assert!(is_ai_domain(" SUB.Kimi.AI ") && !is_ai_domain("notkimi.ai"));
    }

    #[test]
    fn scheme_relative_base_url_resolves_domain() {
        let c = credential("x.json", "other", json!({"type":"x","base_url":"//api.kimi.ai/coding"}));
        assert_eq!(resolve_domain(&c), DOMAIN_AI);
    }

    #[test]
    fn oauth_fields_decode_with_go_types() {
        let ok: TokenWire =
            serde_json::from_str(r#"{"access_token":"a","expires_in":3600.5,"scope":null,"x":1}"#).unwrap();
        assert_eq!(ok.expires_in, Some(3600.5));
        assert!(serde_json::from_str::<TokenWire>(r#"{"access_token":"new","expires_in":"3600"}"#).is_err());
        assert!(serde_json::from_str::<TokenWire>(r#"{"access_token":"new","refresh_token":5}"#).is_err());
        assert!(serde_json::from_str::<DeviceCodeWire>(r#"{"device_code":"d","interval":1.5}"#).is_err());
    }

    #[test]
    fn form_matches_go_values_encode() {
        assert_eq!(
            form(&[("grant_type", DEVICE_GRANT), ("client_id", "a b*~")]),
            "client_id=a+b%2A~&grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"
        );
    }
}
