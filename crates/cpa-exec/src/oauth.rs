//! Claude OAuth acquisition and metadata preparation. Runtime owns patch publication.
//!
//! Refresh is single-flighted process-wide per (token endpoint, refresh token), so
//! independently constructed services never rotate the same token twice. Token and
//! profile calls use the credential's effective proxy through the shared transport.
//!
//! In Home mode the device pool of a dispatched credential is shared through Home KV
//! (`ensure_device_pool`), so every node presents the same device.

use std::collections::HashMap;
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

/// Process-wide refresh state keyed by token endpoint and refresh token: the exchange in
/// flight (`claudeRefreshGroup`) and the 429 block (`claudeRefreshBlock`).
#[derive(Default)]
struct Refreshes {
    in_flight: HashMap<[u8; 32], RefreshResult>,
    blocked: HashMap<[u8; 32], Instant>,
}

fn refreshes() -> std::sync::MutexGuard<'static, Refreshes> {
    static REFRESHES: std::sync::OnceLock<Mutex<Refreshes>> = std::sync::OnceLock::new();
    REFRESHES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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

    /// `ClaudeAuth.RefreshTokensWithRetry` with three attempts: each caller owns its
    /// retry budget and backoff, so a caller that joins a failing exchange late still
    /// retries, and a canceled caller stops retrying.
    pub async fn refresh(&self, refresh: &str) -> Result<MetadataPatch, ExecError> {
        if refresh.is_empty() {
            return Err(acquisition_error("refresh token is required"));
        }
        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(attempt)).await;
            }
            // isClaudeRefreshRetryable: 5xx and transport failures; never a 429.
            match self.refresh_once(refresh).await {
                Err(error) if error.status >= 500 && attempt < 2 => continue,
                result => return result,
            }
        }
        unreachable!("third attempt always returns")
    }

    /// `ClaudeAuth.RefreshTokens`: concurrent attempts for one token share a single
    /// exchange; a finished result is never reused, so a forced refresh after an
    /// upstream 401 always exchanges again. After a 429 the token is blocked until its
    /// Retry-After; a success clears the block.
    async fn refresh_once(&self, refresh: &str) -> Result<MetadataPatch, ExecError> {
        let key: [u8; 32] = Sha256::digest(format!("{}\0{refresh}", self.token_url).as_bytes()).into();
        let result = {
            let mut state = refreshes();
            let now = Instant::now();
            if let Some(until) = state.blocked.get(&key).copied() {
                if until > now {
                    let mut error =
                        ExecError::local(429, FailureScope::Credential, "Claude refresh temporarily blocked");
                    error.retry_after = Some(until - now);
                    return Err(error);
                }
                state.blocked.remove(&key);
            }
            if let Some(result) = state.in_flight.get(&key) {
                result.clone()
            } else {
                let oauth = self.clone();
                let refresh = refresh.to_owned();
                // A canceled caller must not abandon an already-rotated refresh token.
                let task = tokio::spawn(async move {
                    // Go's map marshaler sorts these four keys lexically.
                    let body = serde_json::to_vec(&json!({
                        "client_id": CLIENT_ID, "grant_type": "refresh_token",
                        "refresh_token": refresh, "scope": SCOPE,
                    }))
                    .map_err(|_| acquisition_error("cannot encode OAuth request"));
                    let result = match body {
                        Ok(body) => oauth.tokens(body, &refresh, false).await,
                        Err(error) => Err(error),
                    };
                    let mut state = refreshes();
                    state.in_flight.remove(&key);
                    match &result {
                        Ok(_) => {
                            state.blocked.remove(&key);
                        }
                        Err(error) if error.status == 429 => {
                            let wait = error.retry_after.unwrap_or(Duration::from_secs(5));
                            state.blocked.insert(key, Instant::now() + wait);
                        }
                        Err(_) => {}
                    }
                    result
                });
                let result = async move {
                    task.await
                        .map_err(|_| acquisition_error("Claude refresh task failed"))?
                }
                .boxed()
                .shared();
                state.in_flight.insert(key, result.clone());
                result
            }
        };
        result.await
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

    pub(crate) fn via(&self, proxy: &Proxy) -> Self {
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
                let device = ensure_device_pool(credential).await?;
                patch.set.insert("claude_device_ids".into(), json!([device]));
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
pub(crate) fn needs_prepare(credential: &Credential, now: DateTime<Utc>) -> bool {
    refresh_due(credential, now) || needs_identity(credential)
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

/// Go `NormalizeDeviceIDPool` for the one-device pool: the first valid ID, trimmed
/// and lower-cased.
fn normalize_pool<'a>(values: impl IntoIterator<Item = &'a str>) -> Option<String> {
    values
        .into_iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .find(|s| valid_device(s))
}

/// Go `EnsureClaudeCredentialDevicePoolRequired` for a credential without a canonical
/// pool: its own valid device or a new one, and in Home mode the pool Home KV holds
/// for the credential's auth index (written once with NX, canonicalized with XX).
async fn ensure_device_pool(credential: &Credential) -> Result<String, ExecError> {
    let candidate = normalize_pool(
        credential
            .metadata
            .get("claude_device_ids")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str),
    );
    match cpa_home::kv::current_client() {
        Ok(None) => candidate.map_or_else(|| random_hex(32), Ok),
        Ok(Some(client)) => home_device_pool(&client, credential, candidate).await,
        Err(error) => Err(pool_error(format!("Home KV client: {error}"))),
    }
}

fn pool_error(message: String) -> ExecError {
    ExecError::local(
        500,
        FailureScope::Transport,
        format!("ensure Claude credential device pool: {message}"),
    )
}

async fn home_device_pool(
    client: &cpa_home::Client,
    credential: &Credential,
    candidate: Option<String>,
) -> Result<String, ExecError> {
    let mut identity = cpa_core::config::credentials::auth_index(credential).trim().to_owned();
    if identity.is_empty() {
        identity = credential.id.trim().to_owned();
    }
    if identity.is_empty() {
        return Err(pool_error("credential identity is empty".into()));
    }
    let key = format!(
        "cpa:claude:credential-device-pool:{}",
        cpa_home::kv::hash_key_part(&identity)
    );
    let encode = |device: &str| format!(r#"["{device}"]"#).into_bytes();
    let stored = client
        .kv_get(&key)
        .await
        .map_err(|e| pool_error(format!("Home KV get: {e}")))?;
    if let Some(stored) = stored.and_then(|raw| serde_json::from_slice::<Vec<String>>(&raw).ok())
        && let Some(device) = normalize_pool(stored.iter().map(String::as_str))
    {
        // Go `HasCanonicalDeviceIDPool`.
        if stored.len() != 1 || stored[0] != device {
            let options = cpa_home::SetOptions {
                xx: true,
                ..Default::default()
            };
            let written = client
                .kv_set(&key, &encode(&device), options)
                .await
                .map_err(|e| pool_error(format!("canonicalize Home KV value: {e}")))?;
            if !written {
                return Err(pool_error("canonical Home KV value was not written".into()));
            }
        }
        return Ok(device);
    }
    let device = match candidate {
        Some(device) => device,
        None => random_hex(32)?,
    };
    let options = cpa_home::SetOptions {
        nx: true,
        ..Default::default()
    };
    client
        .kv_set(&key, &encode(&device), options)
        .await
        .map_err(|e| pool_error(format!("Home KV set: {e}")))?;
    let raw = client
        .kv_get(&key)
        .await
        .map_err(|e| pool_error(format!("Home KV reread: {e}")))?
        .ok_or_else(|| pool_error("Home KV value missing after set".into()))?;
    let stored: Vec<String> = serde_json::from_slice(&raw).map_err(|e| {
        pool_error(format!(
            "decode Home KV value: {}",
            cpa_home::error::redacted_decode_text(&e)
        ))
    })?;
    normalize_pool(stored.iter().map(String::as_str))
        .ok_or_else(|| pool_error("Home KV pool has 0 entries, want 1".into()))
}

fn valid_device(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn canonical_pool(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_array)
        .is_some_and(|a| a.len() == 1 && a[0].as_str().is_some_and(valid_device))
}

pub(crate) fn random_hex(n: usize) -> Result<String, ExecError> {
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

pub(crate) fn authorize_url(state: &str, challenge: &str) -> String {
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

#[cfg(test)]
#[path = "oauth_tests.rs"]
mod tests;
