//! Claude OAuth acquisition and metadata preparation. Runtime owns patch publication.
//!
//! Refresh is single-flighted process-wide per (token endpoint, refresh token), so
//! independently constructed services never rotate the same token twice. Token and
//! profile calls use the credential's effective proxy through the shared transport.
//!
//! ponytail: Home KV identity is not ported (process-local only).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, SecondsFormat, Utc};
use cpa_core::credential::{Credential, MetadataPatch};
use cpa_core::exec::{ExecError, FailureScope, ResponseBody};
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

use crate::proxy::Proxy;
use crate::tls::Transport;
use crate::upstream::into_response;

pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const SCOPE: &str = "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
pub const REDIRECT_URI: &str = "http://localhost:54545/callback";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const ROLES_URL: &str = "https://api.anthropic.com/api/oauth/claude_cli/roles";

type RefreshResult = Shared<BoxFuture<'static, Result<MetadataPatch, ExecError>>>;
type Refreshes = Mutex<HashMap<[u8; 32], (Instant, RefreshResult)>>;

/// Process-wide in-flight and recently completed refreshes.
fn refreshes() -> &'static Refreshes {
    static REFRESHES: std::sync::OnceLock<Refreshes> = std::sync::OnceLock::new();
    REFRESHES.get_or_init(Refreshes::default)
}

#[derive(Clone)]
enum Source {
    Fixed(wreq::Client),
    Transport(Arc<Transport>),
}

#[derive(Clone)]
pub struct OAuth {
    source: Source,
    proxy: Proxy,
    token_url: String,
    profile_url: String,
    roles_url: String,
}

impl OAuth {
    pub fn new(client: wreq::Client) -> Self {
        Self::with_endpoints(client, TOKEN_URL, PROFILE_URL, ROLES_URL)
    }

    /// Explicit endpoints for local mocks. Never derive a token endpoint from auth metadata.
    pub fn with_endpoints(client: wreq::Client, token: &str, profile: &str, roles: &str) -> Self {
        Self {
            source: Source::Fixed(client),
            proxy: Proxy::Inherit,
            token_url: token.into(),
            profile_url: profile.into(),
            roles_url: roles.into(),
        }
    }

    /// The compact OAuth TLS profile from the shared, hook-aware transport.
    pub(crate) fn with_transport(transport: Arc<Transport>) -> Self {
        Self {
            source: Source::Transport(transport),
            proxy: Proxy::Inherit,
            token_url: TOKEN_URL.into(),
            profile_url: PROFILE_URL.into(),
            roles_url: ROLES_URL.into(),
        }
    }

    fn client(&self) -> Result<wreq::Client, ExecError> {
        match &self.source {
            Source::Fixed(client) => Ok(client.clone()),
            Source::Transport(t) => t
                .clients(&self.proxy)
                .map(|c| c.oauth.clone())
                .map_err(|_| ExecError::local(502, FailureScope::Transport, "OAuth transport failed")),
        }
    }

    async fn json(&self, request: wreq::RequestBuilder) -> Result<Value, ExecError> {
        let response = request
            .send()
            .await
            .map_err(|_| ExecError::local(502, FailureScope::Transport, "OAuth transport failed"))?;
        let backoff = if response.status().as_u16() == 429 {
            Some(refresh_backoff(response.headers()))
        } else {
            None
        };
        // Never expose a control-plane body: providers may echo tokens in errors.
        let response = into_response(response).await.map_err(|error| {
            let mut safe = ExecError::local(error.status, error.scope, "Claude OAuth acquisition failed");
            safe.retry_after = backoff.or(error.retry_after);
            safe
        })?;
        match response.body {
            ResponseBody::Buffered(body) => {
                serde_json::from_slice(&body).map_err(|_| acquisition_error("invalid Claude OAuth response"))
            }
            ResponseBody::Stream(_) => Err(acquisition_error("unexpected OAuth event stream")),
        }
    }

    fn request(&self, method: wreq::Method, endpoint: &str) -> Result<wreq::RequestBuilder, ExecError> {
        let headers = if method == wreq::Method::GET {
            crate::wire::OAUTH_INSPECT
        } else {
            crate::wire::OAUTH_TOKEN
        };
        Ok(self
            .client()?
            .request(method, endpoint)
            .redirect(wreq::redirect::Policy::none())
            .orig_headers(crate::wire::order(headers))
            .header("accept", "application/json, text/plain, */*")
            .header("content-type", "application/json")
            .header("user-agent", "axios/1.15.2")
            .header("accept-encoding", "gzip, compress, deflate, br")
            .header("connection", "close"))
    }

    async fn profile(&self, token: &str) -> Result<Value, ExecError> {
        self.json(
            self.request(wreq::Method::GET, &self.profile_url)?
                .header("authorization", format!("Bearer {token}"))
                .header("cache-control", "no-cache"),
        )
        .await
    }

    async fn tokens(&self, body: Vec<u8>, previous_refresh: &str, login: bool) -> Result<MetadataPatch, ExecError> {
        let value = tokio::time::timeout(
            Duration::from_secs(30),
            self.json(self.request(wreq::Method::POST, &self.token_url)?.body(body)),
        )
        .await
        .map_err(|_| acquisition_error("Claude token acquisition timed out"))??;
        let access = value["access_token"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| acquisition_error("OAuth response has no access token"))?;
        let expires = value["expires_in"]
            .as_i64()
            .ok_or_else(|| acquisition_error("OAuth response has no expiry"))?;
        let now = Utc::now();
        let duration =
            chrono::Duration::try_seconds(expires).ok_or_else(|| acquisition_error("invalid OAuth expiry"))?;
        let expiry = now
            .checked_add_signed(duration)
            .ok_or_else(|| acquisition_error("invalid OAuth expiry"))?;
        let refresh = value["refresh_token"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(previous_refresh);
        let mut patch = MetadataPatch::default();
        patch.set.insert("access_token".into(), access.into());
        if !refresh.is_empty() {
            patch.set.insert("refresh_token".into(), refresh.into());
        }
        patch.set.insert("expired".into(), timestamp(expiry).into());
        patch.set.insert("last_refresh".into(), timestamp(now).into());
        patch.set.insert("type".into(), "claude".into());
        if login {
            copy_nonempty(
                &mut patch,
                &value,
                &[
                    ("/account/uuid", "account_uuid"),
                    ("/account/email_address", "email"),
                    ("/organization/uuid", "organization_uuid"),
                    ("/organization/name", "organization_name"),
                ],
            );
        }
        // Token rotation has already succeeded. Optional profile failure must not discard
        // the new refresh token or erase previously resolved identity (Go executor auth).
        if let Ok(Ok(profile)) = tokio::time::timeout(Duration::from_secs(10), self.profile(access)).await {
            copy_profile(&mut patch, &profile);
        }
        if login {
            let _ = tokio::time::timeout(Duration::from_secs(10), async {
                self.json(
                    self.request(wreq::Method::GET, &self.roles_url)?
                        .header("authorization", format!("Bearer {access}"))
                        .header("cache-control", "no-cache"),
                )
                .await
            })
            .await;
            for field in ["id_token", "email", "refresh_token"] {
                patch
                    .set
                    .entry(field.to_owned())
                    .or_insert(Value::String(String::new()));
            }
            patch.set.insert("claude_device_ids".into(), json!([random_hex(32)?]));
        }
        Ok(patch)
    }

    pub async fn refresh(&self, refresh: &str) -> Result<MetadataPatch, ExecError> {
        if refresh.is_empty() {
            return Err(acquisition_error("refresh token is required"));
        }
        let key: [u8; 32] = Sha256::digest(format!("{}\0{refresh}", self.token_url).as_bytes()).into();
        let result = {
            let mut map = refreshes().lock().expect("refresh state lock");
            map.retain(|_, (until, _)| *until > Instant::now());
            if let Some((_, result)) = map.get(&key) {
                result.clone()
            } else {
                if map.len() >= 64 {
                    return Err(acquisition_error("Claude refresh capacity reached"));
                }
                let oauth = self.clone();
                let refresh = refresh.to_owned();
                // A canceled caller must not abandon an already-rotated refresh token.
                let task = tokio::spawn(async move {
                    let result = oauth.refresh_with_retry(&refresh).await;
                    let retention = match &result {
                        // ponytail: retain successful exchanges for five minutes to protect
                        // duplicate stale file snapshots; runtime still owns publication.
                        Ok(_) => Duration::from_secs(300),
                        Err(error) => error
                            .retry_after
                            .unwrap_or(Duration::from_secs(5))
                            .clamp(Duration::from_secs(5), Duration::from_secs(300)),
                    };
                    if let Some((until, _)) = refreshes().lock().expect("refresh state lock").get_mut(&key) {
                        *until = Instant::now() + retention;
                    }
                    result
                });
                let result = async move {
                    task.await
                        .map_err(|_| acquisition_error("Claude refresh task failed"))?
                }
                .boxed()
                .shared();
                map.insert(key, (Instant::now() + Duration::from_secs(300), result.clone()));
                result
            }
        };
        result.await
    }

    async fn refresh_with_retry(&self, refresh: &str) -> Result<MetadataPatch, ExecError> {
        // Go's map marshaler sorts these four keys lexically.
        let body = serde_json::to_vec(&json!({
            "client_id": CLIENT_ID, "grant_type": "refresh_token",
            "refresh_token": refresh, "scope": SCOPE,
        }))
        .map_err(|_| acquisition_error("cannot encode OAuth request"))?;
        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(attempt)).await;
            }
            match self.tokens(body.clone(), refresh, false).await {
                Err(error) if error.status >= 500 && attempt < 2 => continue,
                result => return result,
            }
        }
        unreachable!("third attempt always returns")
    }

    pub async fn exchange(&self, code: &str, state: &str, verifier: &str) -> Result<MetadataPatch, ExecError> {
        #[derive(Serialize)]
        struct Exchange<'a> {
            grant_type: &'a str,
            code: &'a str,
            redirect_uri: &'a str,
            client_id: &'a str,
            code_verifier: &'a str,
            state: &'a str,
        }
        let mut parts = code.split('#');
        let code = parts.next().unwrap_or_default();
        let fragment = parts.next().filter(|s| !s.is_empty()).unwrap_or(state);
        let body = serde_json::to_vec(&Exchange {
            grant_type: "authorization_code",
            code,
            redirect_uri: REDIRECT_URI,
            client_id: CLIENT_ID,
            code_verifier: verifier,
            state: fragment,
        })
        .map_err(|_| acquisition_error("cannot encode OAuth request"))?;
        self.tokens(body, "", true).await
    }

    fn via(&self, proxy: &Proxy) -> Self {
        Self {
            proxy: proxy.clone(),
            ..self.clone()
        }
    }

    /// `ClaudeExecutor.Refresh`: rotate the token, then read the profile, using the
    /// credential's proxy.
    pub(crate) async fn refresh_credential(
        &self,
        credential: &Credential,
        proxy: &Proxy,
    ) -> Result<MetadataPatch, ExecError> {
        let token = refresh_token(credential);
        if token.is_empty() {
            return Ok(MetadataPatch::default());
        }
        self.via(proxy).refresh(token).await
    }

    /// One preparation under the runtime's readiness contract. A credential missing its
    /// identity gets only that (`PrepareRequestAuth`: device pool and account UUID), so
    /// a failing refresh never blocks requests. Anything else is a token refresh
    /// (`Refresh`): inside the refresh lead from the background loop, or on demand after
    /// an upstream 401 (`refreshAuthForRequest`), when nothing else is due.
    pub(crate) async fn prepare(&self, credential: &Credential, proxy: &Proxy) -> Result<MetadataPatch, ExecError> {
        if needs_identity(credential) {
            self.via(proxy).prepare_inner(credential).await
        } else {
            self.refresh_credential(credential, proxy).await
        }
    }

    async fn prepare_inner(&self, credential: &Credential) -> Result<MetadataPatch, ExecError> {
        let mut patch = MetadataPatch::default();
        let access = credential.str("access_token").unwrap_or_default().to_owned();
        if access.contains("sk-ant-oat") {
            if !canonical_pool(credential.metadata.get("claude_device_ids")) {
                let normalized = credential
                    .metadata
                    .get("claude_device_ids")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|s| s.trim().to_ascii_lowercase())
                    .find(|s| valid_device(s));
                patch.set.insert(
                    "claude_device_ids".into(),
                    json!([normalized.unwrap_or(random_hex(32)?)]),
                );
            }
            if credential.str("account_uuid").unwrap_or_default().trim().is_empty()
                && !patch.set.contains_key("account_uuid")
            {
                let setup = setup_token(credential);
                let profile = if setup {
                    None
                } else {
                    match tokio::time::timeout(Duration::from_secs(10), self.profile(&access)).await {
                        Ok(Ok(profile)) => Some(profile),
                        Ok(Err(error)) if error.status == 403 => None,
                        Ok(Err(error)) => return Err(error),
                        Err(_) => return Err(acquisition_error("Claude profile acquisition timed out")),
                    }
                };
                if let Some(profile) = profile {
                    copy_profile(&mut patch, &profile);
                }
                if !patch.set.contains_key("account_uuid") {
                    let seed = if credential.id.trim().is_empty() {
                        format!(
                            "{}|{access}",
                            if setup {
                                "claude-setup-token"
                            } else {
                                "claude-oauth-fallback"
                            }
                        )
                    } else {
                        format!("auth-id|{}", credential.id.trim())
                    };
                    let identity = uuid::Uuid::new_v5(
                        &uuid::Uuid::NAMESPACE_OID,
                        format!("cpa-claude-code-cli-account|{seed}").as_bytes(),
                    );
                    patch.set.insert("account_uuid".into(), identity.to_string().into());
                }
                patch
                    .set
                    .insert("claude_account_profile_checked_at".into(), timestamp(Utc::now()).into());
            }
        }
        Ok(patch)
    }
}

/// `ShouldPrepareRequestAuth`: an OAuth token without a canonical device pool or account.
pub(crate) fn needs_prepare(credential: &Credential) -> bool {
    refresh_due(credential, Utc::now()) || needs_identity(credential)
}

/// Go `ClaudeExecutor.ShouldPrepareRequestAuth`: an OAuth token without a canonical
/// device pool or account UUID cannot be cloaked, so requests wait for preparation.
pub(crate) fn needs_identity(credential: &Credential) -> bool {
    credential.str("access_token").is_some_and(|s| s.contains("sk-ant-oat"))
        && (!canonical_pool(credential.metadata.get("claude_device_ids"))
            || credential.str("account_uuid").unwrap_or_default().trim().is_empty())
}

fn refresh_token(credential: &Credential) -> &str {
    credential
        .str("refresh_token")
        .filter(|s| !s.is_empty())
        .or_else(|| credential.str("refreshToken"))
        .unwrap_or_default()
}

fn refresh_due(credential: &Credential, now: DateTime<Utc>) -> bool {
    !refresh_token(credential).is_empty()
        && credential
            .str("expired")
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .is_some_and(|expiry| expiry <= now + chrono::Duration::hours(4))
}

fn setup_token(credential: &Credential) -> bool {
    ["skip_account_profile", "is_setup_token", "setup_token"]
        .iter()
        .any(|key| credential.metadata.get(*key).and_then(Value::as_bool) == Some(true))
        || credential
            .attributes
            .get("auth_kind")
            .is_some_and(|s| matches!(s.to_ascii_lowercase().as_str(), "setup_token" | "setup-token"))
        || credential
            .str("scopes")
            .filter(|s| !s.is_empty())
            .or_else(|| credential.str("scope"))
            .filter(|s| !s.is_empty())
            .is_some_and(|s| {
                let scopes = s.to_ascii_lowercase();
                !scopes.contains("user:profile") && !scopes.contains("user:office")
            })
}

fn copy_nonempty(patch: &mut MetadataPatch, value: &Value, fields: &[(&str, &str)]) {
    for (pointer, key) in fields {
        if let Some(s) = value
            .pointer(pointer)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        {
            patch.set.insert((*key).into(), s.into());
        }
    }
}

fn copy_profile(patch: &mut MetadataPatch, profile: &Value) {
    copy_nonempty(
        patch,
        profile,
        &[
            ("/account/uuid", "account_uuid"),
            ("/account/email", "email"),
            ("/organization/uuid", "organization_uuid"),
            ("/organization/name", "organization_name"),
        ],
    );
}

fn valid_device(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn canonical_pool(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_array)
        .is_some_and(|a| a.len() == 1 && a[0].as_str().is_some_and(valid_device))
}

fn random_hex(n: usize) -> Result<String, ExecError> {
    let mut bytes = vec![0; n];
    getrandom::fill(&mut bytes).map_err(|_| acquisition_error("secure random source unavailable"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn acquisition_error(message: &str) -> ExecError {
    ExecError::local(502, FailureScope::Credential, message)
}
fn refresh_backoff(headers: &http::HeaderMap) -> Duration {
    let now = std::time::SystemTime::now();
    let read = |key| headers.get(key).and_then(|v| v.to_str().ok()).map(str::trim);
    read("retry-after")
        .and_then(|raw| crate::quota::retry_after(raw, now))
        .and_then(|deadline| deadline.duration_since(now).ok())
        .or_else(|| {
            read("retry-after-ms")
                .and_then(|raw| raw.parse::<f64>().ok())
                .and_then(|ms| Duration::try_from_secs_f64(ms / 1000.0).ok())
        })
        .unwrap_or(Duration::from_secs(5))
        .clamp(Duration::from_secs(5), Duration::from_secs(300))
}
fn timestamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// PKCE verifier is 96 random bytes encoded as 128 URL-safe characters, as in Go.
pub fn pkce() -> Result<(String, String), ExecError> {
    let mut bytes = [0; 96];
    getrandom::fill(&mut bytes).map_err(|_| acquisition_error("secure random source unavailable"))?;
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Ok((verifier, challenge))
}

fn authorize_url(state: &str, challenge: &str) -> String {
    let mut url = Url::parse("https://claude.ai/oauth/authorize").expect("constant URL");
    // Go url.Values.Encode sorts keys alphabetically.
    url.query_pairs_mut().extend_pairs([
        ("client_id", CLIENT_ID),
        ("code", "true"),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("redirect_uri", REDIRECT_URI),
        ("response_type", "code"),
        ("scope", SCOPE),
        ("state", state),
    ]);
    url.into()
}

/// Browser login. This is called only by the explicit CLI flag, never during tests.
pub async fn login(auth_dir: &Path) -> Result<PathBuf, ExecError> {
    // ponytail: browser callback only; manual paste/no-browser options, Go's success
    // HTML/redirect and legacy filename migration remain for the complete CLI port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:54545")
        .await
        .map_err(|_| acquisition_error("cannot bind Claude callback port 54545"))?;
    let (verifier, challenge) = pkce()?;
    let state = random_hex(32)?;
    let url = authorize_url(&state, &challenge);
    println!("Open this URL to authorize Claude:\n{url}");
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open")
        .arg(&url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(&url).spawn();
    let code = tokio::time::timeout(Duration::from_secs(300), callback(&listener, &state))
        .await
        .map_err(|_| acquisition_error("Claude login callback timed out"))??;
    let transport = Arc::new(Transport::new(crate::proxy::Hooks::default()));
    let patch = OAuth::with_transport(transport)
        .exchange(&code, &state, &verifier)
        .await?;
    let directory = auth_dir.to_owned();
    tokio::task::spawn_blocking(move || write_login(&directory, patch))
        .await
        .map_err(|_| acquisition_error("credential publication failed"))?
}

async fn callback(listener: &tokio::net::TcpListener, state: &str) -> Result<String, ExecError> {
    loop {
        let (mut socket, _) = listener
            .accept()
            .await
            .map_err(|_| acquisition_error("callback listener failed"))?;
        // ponytail: bounded HTTP/1 callback parser, one request per connection; no general HTTP server.
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            let mut header = Vec::new();
            let mut byte = [0];
            while header.len() < 8192 && !header.ends_with(b"\r\n\r\n") {
                if socket.read(&mut byte).await? == 0 { break; }
                header.push(byte[0]);
            }
            let code = parse_callback(&header, state);
            let (status, body) = if code.is_ok() { ("200 OK", "Authorization received. You can close this window.") }
                else { ("400 Bad Request", "Invalid OAuth callback.") };
            socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await?;
            Ok::<_, std::io::Error>(code)
        }).await;
        if let Ok(Ok(Ok(code))) = result {
            return Ok(code);
        }
    }
}

fn parse_callback(header: &[u8], state: &str) -> Result<String, ExecError> {
    let header = std::str::from_utf8(header).map_err(|_| acquisition_error("invalid callback"))?;
    let mut request = header.lines().next().unwrap_or_default().split_whitespace();
    if request.next() != Some("GET") {
        return Err(acquisition_error("invalid callback method"));
    }
    let url = Url::parse(&format!("http://localhost{}", request.next().unwrap_or_default()))
        .map_err(|_| acquisition_error("invalid callback URL"))?;
    if url.path() != "/callback" {
        return Err(acquisition_error("invalid callback path"));
    }
    let pairs: Vec<_> = url.query_pairs().collect();
    let get = |name| {
        pairs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_ref())
            .unwrap_or_default()
    };
    if !get("error").is_empty() || get("state") != state || get("code").is_empty() {
        return Err(acquisition_error("invalid callback state or code"));
    }
    // The fragment is also sent to the exchange. Do not allow it to replace validated state.
    if get("code")
        .split('#')
        .nth(1)
        .is_some_and(|fragment| !fragment.is_empty() && fragment != state)
    {
        return Err(acquisition_error("invalid callback fragment state"));
    }
    Ok(get("code").into())
}

fn write_login(directory: &Path, patch: MetadataPatch) -> Result<PathBuf, ExecError> {
    let string = |key: &str| patch.set.get(key).and_then(Value::as_str).unwrap_or_default().trim();
    let email = string("email");
    if email.contains(['/', '\\']) {
        return Err(acquisition_error("invalid credential email filename"));
    }
    let identity = if string("organization_uuid").is_empty() {
        string("account_uuid")
    } else {
        string("organization_uuid")
    };
    let prefix = if identity.is_empty() {
        "claude".into()
    } else {
        let digest = Sha256::digest(identity.as_bytes());
        format!(
            "claude-{}",
            digest[..4].iter().map(|b| format!("{b:02x}")).collect::<String>()
        )
    };
    let path = directory.join(format!("{prefix}-{email}.json"));
    let mut metadata = match std::fs::read(&path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|_| acquisition_error("invalid existing credential file"))?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(_) => return Err(acquisition_error("cannot read existing credential file")),
    };
    patch.apply(&mut metadata);
    let publish = || -> std::io::Result<()> {
        use std::io::Write;
        let mut directory_options = std::fs::DirBuilder::new();
        directory_options.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directory_options.mode(0o700);
        }
        directory_options.create(directory)?;
        let temporary = directory.join(format!(
            ".claude-{}.tmp",
            random_hex(16).map_err(std::io::Error::other)?
        ));
        let result = (|| {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            serde_json::to_writer(&mut file, &metadata)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            std::fs::rename(&temporary, &path)?;
            std::fs::File::open(directory)?.sync_all()
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    };
    publish().map_err(|_| acquisition_error("cannot publish Claude credential"))?;
    Ok(path)
}

#[cfg(test)]
#[path = "oauth_tests.rs"]
mod tests;
