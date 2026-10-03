//! Codex (ChatGPT) OAuth: browser PKCE and device login, token refresh, and the auth
//! file Go writes (internal/auth/codex, sdk/auth/codex.go, sdk/auth/codex_device.go,
//! internal/runtime/executor/codex_executor_auth.go).
//!
//! Executors never persist: refresh returns a [`MetadataPatch`] that the runtime commits.
//! Only the explicit login commands write a credential file.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use chrono::{DateTime, Local, SecondsFormat, Utc};
use cpa_core::credential::{Credential, MetadataPatch};
use cpa_core::exec::{ExecError, FailureScope};
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const DEFAULT_CALLBACK_PORT: u16 = 1455;
pub const DEVICE_USERCODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
pub const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
pub const DEVICE_VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const DEFAULT_PLAN: &str = "free";
/// `CodexAuthenticator.RefreshLead`.
pub const REFRESH_LEAD: chrono::TimeDelta = chrono::TimeDelta::hours(24);

pub fn redirect_uri(port: u16) -> String {
    format!("http://localhost:{port}/auth/callback")
}

/// The JWT claims Go reads from a Codex ID token (`codex.JWTClaims`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Claims {
    pub email: String,
    pub account_id: String,
    /// Raw `chatgpt_plan_type`; see [`Claims::plan_type`].
    pub plan: String,
}

impl Claims {
    /// `GetPlanType`: trimmed plan or `free`.
    pub fn plan_type(&self) -> String {
        match self.plan.trim() {
            "" => DEFAULT_PLAN.into(),
            plan => plan.into(),
        }
    }
}

/// `codex.ParseJWTToken`: payload only, no signature check. Go unmarshals into a typed
/// struct, so a claim with the wrong JSON type rejects the whole token.
// ponytail: Go's case-insensitive key matching is not reproduced; real tokens use exact keys.
pub fn parse_jwt(token: &str) -> Option<Claims> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    #[allow(dead_code)]
    struct Org {
        id: Option<String>,
        is_default: Option<bool>,
        role: Option<String>,
        title: Option<String>,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    #[allow(dead_code)]
    struct AuthInfo {
        chatgpt_account_id: Option<String>,
        chatgpt_plan_type: Option<String>,
        chatgpt_subscription_last_checked: Option<String>,
        chatgpt_user_id: Option<String>,
        groups: Option<Vec<Value>>,
        organizations: Option<Vec<Org>>,
        user_id: Option<String>,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    #[allow(dead_code)]
    struct Raw {
        at_hash: Option<String>,
        aud: Option<Vec<String>>,
        auth_provider: Option<String>,
        auth_time: Option<i64>,
        email: Option<String>,
        email_verified: Option<bool>,
        exp: Option<i64>,
        #[serde(rename = "https://api.openai.com/auth")]
        auth: Option<AuthInfo>,
        iat: Option<i64>,
        iss: Option<String>,
        jti: Option<String>,
        rat: Option<i64>,
        sid: Option<String>,
        sub: Option<String>,
    }
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = jwt_engine().decode(parts[1]).ok()?;
    let raw: Raw = serde_json::from_slice(&payload).ok()?;
    if let Some(checked) = raw
        .auth
        .as_ref()
        .and_then(|a| a.chatgpt_subscription_last_checked.as_deref())
        && DateTime::parse_from_rfc3339(checked).is_err()
    {
        return None;
    }
    let auth = raw.auth.unwrap_or_default();
    Some(Claims {
        email: raw.email.unwrap_or_default(),
        account_id: auth.chatgpt_account_id.unwrap_or_default(),
        plan: auth.chatgpt_plan_type.unwrap_or_default(),
    })
}

fn jwt_engine() -> GeneralPurpose {
    GeneralPurpose::new(
        &base64::alphabet::URL_SAFE,
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
    )
}

/// The `exp` claim of a JWT access token (`parseJWTExp`).
pub(crate) fn jwt_exp(token: &str) -> Option<DateTime<Utc>> {
    let parts: Vec<&str> = token.trim().split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = jwt_engine().decode(parts[1]).ok()?;
    let claims: Value = serde_json::from_slice(&payload).ok()?;
    let exp = match &claims["exp"] {
        Value::Number(n) => n.as_f64().map(|f| f as i64)?,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    if exp <= 0 {
        return None;
    }
    // normaliseUnix: millisecond timestamps are scaled down.
    let secs = if exp > 1_000_000_000_000 { exp / 1000 } else { exp };
    DateTime::from_timestamp(secs, 0)
}

/// Go `time.Now().Format(time.RFC3339)`: local offset, `Z` for UTC.
fn rfc3339(time: DateTime<Local>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Token fields from one successful exchange or refresh (`CodexTokenData`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
    pub email: String,
    pub plan_type: String,
    pub expired: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

type RefreshResult = Shared<BoxFuture<'static, Result<Tokens, ExecError>>>;
/// Completed refresh results kept for deduplication and failure backoff.
const MAX_RETAINED_REFRESHES: usize = 64;
type Refreshes = Arc<Mutex<HashMap<[u8; 32], (Instant, RefreshResult)>>>;

/// Codex OAuth endpoints and refresh coordination. Clones share the singleflight table.
#[derive(Clone)]
pub struct CodexOAuth {
    client: wreq::Client,
    token_url: String,
    device_usercode_url: String,
    device_token_url: String,
    refreshes: Refreshes,
    retry_delay: Duration,
}

impl CodexOAuth {
    pub fn new(client: wreq::Client) -> Self {
        Self::with_endpoints(client, TOKEN_URL, DEVICE_USERCODE_URL, DEVICE_TOKEN_URL)
    }

    /// Explicit endpoints for local mocks. Never derived from credential metadata.
    pub fn with_endpoints(client: wreq::Client, token: &str, usercode: &str, device_token: &str) -> Self {
        Self {
            client,
            token_url: token.into(),
            device_usercode_url: usercode.into(),
            device_token_url: device_token.into(),
            refreshes: Arc::default(),
            retry_delay: Duration::from_secs(1),
        }
    }

    /// Tests shorten Go's 1s-per-attempt refresh backoff.
    pub fn with_retry_delay(mut self, delay: Duration) -> Self {
        self.retry_delay = delay;
        self
    }

    /// The same endpoints and refresh table over another client: Go builds the refresh
    /// client per credential from its effective proxy (`NewCodexAuthWithProxyURL`).
    pub fn with_client(&self, client: wreq::Client) -> Self {
        Self { client, ..self.clone() }
    }

    /// Posts a form or JSON body as Go's `http.Client.Do` does (Go net/http headers,
    /// redirects followed, transparent gzip). Returns the status and a bounded body;
    /// transport faults never echo URLs or payloads.
    async fn post(&self, url: &str, content_type: &str, body: Vec<u8>) -> Result<(u16, bytes::Bytes), ExecError> {
        let failed = || ExecError::local(502, FailureScope::Transport, "codex oauth request failed");
        let mut headers = crate::proxy::GoHeaders::new();
        headers.set("Content-Type", content_type);
        headers.set("Accept", "application/json");
        let upstream = crate::proxy::send(&self.client, url, headers, body, None)
            .await
            .map_err(|_| failed())?;
        let body = crate::proxy::read_all(upstream.body, crate::proxy::MAX_ERROR_BODY, false)
            .await
            .map_err(|_| failed())?;
        Ok((upstream.status, body))
    }

    /// `exchange` keeps Go's `ExchangeCodeForTokens` error texts (status and body);
    /// refreshes keep the sanitized refresh error.
    async fn token_request(&self, form: &[(&str, &str)], exchange: bool) -> Result<Tokens, ExecError> {
        // url.Values.Encode sorts keys; callers pass them sorted.
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form)
            .finish();
        let (status, body) = self
            .post(&self.token_url, "application/x-www-form-urlencoded", body.into_bytes())
            .await?;
        if status != 200 && exchange {
            let message = format!(
                "token exchange failed with status {status}: {}",
                String::from_utf8_lossy(&body)
            );
            return Err(oauth_error(status, &message));
        }
        if status != 200 {
            return Err(token_error(status, &body));
        }
        // Go appends the decoder error (`failed to parse token response: %w`); serde's
        // diagnostics can quote response values such as tokens, so only the prefix is kept.
        let parsed: TokenResponse = serde_json::from_slice(&body).map_err(|_| {
            if exchange {
                oauth_error(502, "failed to parse token response")
            } else {
                oauth_error(502, "failed to parse codex token response")
            }
        })?;
        let id_token = parsed.id_token.unwrap_or_default();
        let claims = parse_jwt(&id_token).unwrap_or_default();
        let expires = chrono::TimeDelta::try_seconds(parsed.expires_in.unwrap_or(0)).unwrap_or_default();
        Ok(Tokens {
            plan_type: claims.plan_type(),
            account_id: claims.account_id,
            email: claims.email,
            id_token,
            access_token: parsed.access_token.unwrap_or_default(),
            refresh_token: parsed.refresh_token.unwrap_or_default(),
            expired: rfc3339(Local::now() + expires),
        })
    }

    /// `ExchangeCodeForTokensWithRedirect`.
    pub async fn exchange(&self, code: &str, redirect: &str, verifier: &str) -> Result<Tokens, ExecError> {
        if redirect.trim().is_empty() {
            return Err(oauth_error(400, "redirect URI is required for token exchange"));
        }
        self.token_request(
            &[
                ("client_id", CLIENT_ID),
                ("code", code),
                ("code_verifier", verifier),
                ("grant_type", "authorization_code"),
                ("redirect_uri", redirect.trim()),
            ],
            true,
        )
        .await
    }

    /// `RefreshTokensWithRetry(ctx, token, 3)`, single-flighted per refresh token and
    /// detached from the caller so a cancelled request cannot strand a rotated token.
    pub async fn refresh(&self, refresh_token: &str) -> Result<Tokens, ExecError> {
        if refresh_token.is_empty() {
            return Err(oauth_error(400, "refresh token is required"));
        }
        let key: [u8; 32] = Sha256::digest(refresh_token.as_bytes()).into();
        let shared = {
            let mut table = self.refreshes.lock().expect("refresh table");
            table.retain(|_, (until, _)| *until > Instant::now());
            if let Some((_, result)) = table.get(&key) {
                result.clone()
            } else {
                // Retained results are a cache, not an admission limit: evict the oldest
                // completed ones. In-flight entries are bounded by the credential count.
                while table.len() >= MAX_RETAINED_REFRESHES {
                    let oldest = table
                        .iter()
                        .filter(|(_, (_, result))| result.peek().is_some())
                        .min_by_key(|(_, (until, _))| *until)
                        .map(|(k, _)| *k);
                    match oldest {
                        Some(k) => table.remove(&k),
                        None => break,
                    };
                }
                let oauth = self.clone();
                let token = refresh_token.to_owned();
                let task = tokio::spawn(async move {
                    let result = oauth.refresh_with_retry(&token).await;
                    // ponytail: results are retained for 5 minutes, matching Go's refresh
                    // failure backoff; success retention absorbs stale duplicate snapshots.
                    if let Some((until, _)) = oauth.refreshes.lock().expect("refresh table").get_mut(&key) {
                        *until = Instant::now() + Duration::from_secs(300);
                    }
                    result
                });
                let shared = async move { task.await.map_err(|_| oauth_error(500, "codex refresh task failed"))? }
                    .boxed()
                    .shared();
                table.insert(key, (Instant::now() + Duration::from_secs(300), shared.clone()));
                shared
            }
        };
        shared.await
    }

    async fn refresh_with_retry(&self, refresh_token: &str) -> Result<Tokens, ExecError> {
        let mut last = None;
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(self.retry_delay * attempt).await;
            }
            let once = tokio::time::timeout(
                Duration::from_secs(30),
                self.token_request(
                    &[
                        ("client_id", CLIENT_ID),
                        ("grant_type", "refresh_token"),
                        ("refresh_token", refresh_token),
                        ("scope", "openid profile email"),
                    ],
                    false,
                ),
            )
            .await
            .unwrap_or_else(|_| Err(oauth_error(504, "codex token refresh timed out")));
            match once {
                Ok(tokens) => return Ok(tokens),
                Err(error) if String::from_utf8_lossy(&error.body).contains("refresh_token_reused") => {
                    return Err(error);
                }
                Err(error) => last = Some(error),
            }
        }
        let mut error = last.expect("three attempts ran");
        error.body = format!(
            "token refresh failed after 3 attempts: {}",
            String::from_utf8_lossy(&error.body)
        )
        .into();
        Err(error)
    }

    /// Requests a device code (`requestCodexDeviceUserCode`).
    pub async fn device_code(&self) -> Result<DeviceCode, ExecError> {
        let body = serde_json::to_vec(&json!({ "client_id": CLIENT_ID })).expect("static JSON");
        let (status, body) = self.post(&self.device_usercode_url, "application/json", body).await?;
        if !(200..300).contains(&status) {
            if status == 404 {
                return Err(oauth_error(
                    502,
                    &format!("codex device endpoint is unavailable (status {status})"),
                ));
            }
            return Err(oauth_error(
                502,
                &format!("codex device code request failed with status {status}"),
            ));
        }
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            device_auth_id: Option<String>,
            #[serde(default)]
            user_code: Option<String>,
            #[serde(default)]
            usercode: Option<String>,
            #[serde(default)]
            interval: Option<Value>,
        }
        let raw: Raw = serde_json::from_slice(&body)
            .map_err(|_| oauth_error(502, "failed to decode codex device code response"))?;
        let user_code = raw
            .user_code
            .filter(|s| !s.trim().is_empty())
            .or(raw.usercode)
            .unwrap_or_default()
            .trim()
            .to_owned();
        let device_auth_id = raw.device_auth_id.unwrap_or_default().trim().to_owned();
        if user_code.is_empty() || device_auth_id.is_empty() {
            return Err(oauth_error(502, "codex device flow did not return required fields"));
        }
        let seconds = match raw.interval {
            Some(Value::String(s)) => s.trim().parse::<u64>().ok(),
            Some(Value::Number(n)) => n.as_u64(),
            _ => None,
        }
        .filter(|s| *s > 0)
        .unwrap_or(5);
        Ok(DeviceCode {
            device_auth_id,
            user_code,
            interval: Duration::from_secs(seconds),
        })
    }

    /// Polls until the user approves (`pollCodexDeviceToken`), then exchanges the code.
    pub async fn device_poll(&self, code: &DeviceCode, deadline: Duration) -> Result<Tokens, ExecError> {
        let started = Instant::now();
        let body = serde_json::to_vec(&json!({
            "device_auth_id": code.device_auth_id,
            "user_code": code.user_code,
        }))
        .expect("strings serialize");
        loop {
            if started.elapsed() > deadline {
                return Err(oauth_error(
                    408,
                    "codex device authentication timed out after 15 minutes",
                ));
            }
            let (status, response) = self
                .post(&self.device_token_url, "application/json", body.clone())
                .await?;
            match status {
                200..=299 => {
                    let value: Value = serde_json::from_slice(&response)
                        .map_err(|_| oauth_error(502, "failed to decode codex device token response"))?;
                    let field = |k: &str| value[k].as_str().unwrap_or_default().trim().to_owned();
                    let (auth_code, verifier) = (field("authorization_code"), field("code_verifier"));
                    if auth_code.is_empty() || verifier.is_empty() || field("code_challenge").is_empty() {
                        return Err(oauth_error(
                            502,
                            "codex device flow token response missing required fields",
                        ));
                    }
                    return self.exchange(&auth_code, DEVICE_REDIRECT_URI, &verifier).await;
                }
                403 | 404 => tokio::time::sleep(code.interval).await,
                _ => {
                    return Err(oauth_error(
                        502,
                        &format!("codex device token polling failed with status {status}"),
                    ));
                }
            }
        }
    }

    /// The patch Go's `CodexExecutor.Refresh` applies to credential metadata.
    pub async fn refresh_patch(&self, refresh_token: &str) -> Result<MetadataPatch, ExecError> {
        let tokens = self.refresh(refresh_token).await?;
        Ok(refresh_patch(&tokens, Local::now()))
    }
}

pub struct DeviceCode {
    pub device_auth_id: String,
    pub user_code: String,
    pub interval: Duration,
}

fn refresh_patch(tokens: &Tokens, now: DateTime<Local>) -> MetadataPatch {
    let mut patch = MetadataPatch::default();
    let mut set = |k: &str, v: &str| {
        patch.set.insert(k.into(), Value::String(v.into()));
    };
    set("id_token", &tokens.id_token);
    set("access_token", &tokens.access_token);
    if !tokens.refresh_token.is_empty() {
        set("refresh_token", &tokens.refresh_token);
    }
    if !tokens.account_id.is_empty() {
        set("account_id", &tokens.account_id);
    }
    set("email", &tokens.email);
    set("expired", &tokens.expired);
    set("type", "codex");
    set("last_refresh", &rfc3339(now));
    let plan = match tokens.plan_type.trim() {
        "" => parse_jwt(&tokens.id_token).unwrap_or_default().plan_type(),
        plan => plan.to_owned(),
    };
    set("plan_type", &plan);
    patch
}

fn oauth_error(status: u16, message: &str) -> ExecError {
    ExecError::local(status, FailureScope::Credential, message)
}

/// Go reports `token refresh failed with status N: <body>`. Only the OAuth error code is
/// kept: token endpoints may echo request material, and refresh backoff keys on
/// `invalid_grant` / `refresh_token_reused`.
fn token_error(status: u16, body: &[u8]) -> ExecError {
    let code = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| match &v["error"] {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => o.get("code").and_then(Value::as_str).map(str::to_owned),
            _ => None,
        })
        .filter(|c| c.len() <= 64 && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.'))
        .unwrap_or_default();
    // A rejected grant makes the credential unusable (401, cooled and failed over); server
    // and rate-limit faults are transport-like and must not poison the credential.
    let (status_out, scope) = match status {
        400..=403 => (401, FailureScope::Credential),
        _ => (502, FailureScope::Transport),
    };
    ExecError::local(
        status_out,
        scope,
        format!("token refresh failed with status {status}: {code}"),
    )
}

/// Refresh token from metadata (`Refresh` reads only `refresh_token`).
pub(crate) fn refresh_token(credential: &Credential) -> &str {
    credential.str("refresh_token").unwrap_or_default()
}

/// Expiry Go's refresh scheduler uses: the access token's JWT `exp`, else metadata.
pub(crate) fn expiry(credential: &Credential) -> Option<DateTime<Utc>> {
    if let Some(exp) = credential.str("access_token").and_then(jwt_exp) {
        return Some(exp);
    }
    ["expired", "expire", "expires_at", "expiresAt", "expiry", "expires"]
        .iter()
        .find_map(|k| credential.metadata.get(*k).and_then(parse_time))
}

fn parse_time(value: &Value) -> Option<DateTime<Utc>> {
    match value {
        Value::String(s) if s.trim().parse::<i64>().is_ok() => {
            parse_time(&Value::Number(s.trim().parse::<i64>().ok()?.into()))
        }
        Value::String(s) => {
            let s = s.trim();
            DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|t| t.with_timezone(&Utc))
                .or_else(|| {
                    ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"]
                        .iter()
                        .find_map(|f| chrono::NaiveDateTime::parse_from_str(s, f).ok().map(|t| t.and_utc()))
                })
        }
        Value::Number(n) => {
            let n = n.as_f64()? as i64;
            let secs = if n > 1_000_000_000_000 { n / 1000 } else { n };
            (secs > 0).then(|| DateTime::from_timestamp(secs, 0)).flatten()
        }
        _ => None,
    }
}

/// `Manager.shouldRefresh` with the Codex lead (24h). Without an expiry, `last_refresh`
/// older than the lead (or missing) is due.
// ponytail: refresh_interval_seconds overrides and unauthorized/invalid-grant lifecycle
// gating live in the runtime scheduler (M4-0027).
pub(crate) fn refresh_due(credential: &Credential, now: DateTime<Utc>) -> bool {
    if refresh_token(credential).is_empty() {
        return false;
    }
    if let Some(expiry) = expiry(credential) {
        return expiry - now <= REFRESH_LEAD;
    }
    ["last_refresh", "lastRefresh", "last_refreshed_at", "lastRefreshedAt"]
        .iter()
        .find_map(|k| credential.metadata.get(*k).and_then(parse_time))
        .is_none_or(|last| now - last >= REFRESH_LEAD)
}

/// The access token is present and not expired.
pub(crate) fn access_usable(credential: &Credential, now: DateTime<Utc>) -> bool {
    !credential.str("access_token").unwrap_or_default().is_empty() && expiry(credential).is_none_or(|exp| exp > now)
}

/// `CredentialFileName(email, plan, hash, true)`.
pub fn credential_file_name(email: &str, plan: &str, account_hash: &str) -> String {
    let email = email.trim();
    let plan: Vec<String> = plan
        .trim()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect();
    let plan = plan.join("-");
    let hash = account_hash.trim();
    match (hash.is_empty(), plan.is_empty()) {
        (false, true) => format!("codex-{hash}-{email}.json"),
        (false, false) => format!("codex-{hash}-{email}-{plan}.json"),
        (true, true) => format!("codex-{email}.json"),
        (true, false) => format!("codex-{email}-{plan}.json"),
    }
}

/// `buildAuthRecord` + `CodexTokenStorage` + `MergeMetadata`: the file name and the JSON
/// object Go saves after a login.
pub fn login_record(tokens: &Tokens, last_refresh: &str) -> Result<(String, Map<String, Value>), ExecError> {
    if tokens.email.is_empty() {
        return Err(oauth_error(502, "codex token storage missing account information"));
    }
    let mut plan = match tokens.plan_type.trim() {
        "" => DEFAULT_PLAN.to_owned(),
        plan => plan.to_owned(),
    };
    let mut hash = String::new();
    if let Some(claims) = (!tokens.id_token.is_empty())
        .then(|| parse_jwt(&tokens.id_token))
        .flatten()
    {
        if !claims.plan.trim().is_empty() {
            plan = claims.plan.trim().to_owned();
        }
        let account = claims.account_id.trim();
        if !account.is_empty() {
            hash = Sha256::digest(account.as_bytes())[..4]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
        }
    }
    let name = credential_file_name(&tokens.email, &plan, &hash);
    let mut record = Map::new();
    for (k, v) in [
        ("id_token", &tokens.id_token),
        ("access_token", &tokens.access_token),
        ("refresh_token", &tokens.refresh_token),
        ("account_id", &tokens.account_id),
        ("last_refresh", &last_refresh.to_owned()),
        ("email", &tokens.email),
        ("type", &"codex".to_owned()),
        ("expired", &tokens.expired),
        ("plan_type", &plan),
    ] {
        record.insert(k.into(), Value::String(v.clone()));
    }
    record.insert("disabled".into(), Value::Bool(false));
    Ok((name, record))
}

/// `MergeExistingAuthMetadata`: keep non-token fields the user set on an older file.
fn merge_existing(record: &mut Map<String, Value>, existing: Map<String, Value>) {
    const TOKEN_KEYS: &[&str] = &[
        "access_token",
        "refresh_token",
        "id_token",
        "session_id",
        "expired",
        "last_refresh",
        "expires_in",
        "timestamp",
        "token_type",
        "user_code",
        "verification_uri",
        "verification_uri_complete",
    ];
    // Go's fresh login metadata has no `disabled`, so MergeExistingAuthMetadata keeps an
    // existing boolean and FileStore.Save writes it; anything else becomes false.
    let disabled = existing.get("disabled").and_then(Value::as_bool).unwrap_or(false);
    for (k, v) in existing {
        if TOKEN_KEYS.contains(&k.trim().to_ascii_lowercase().as_str()) {
            continue;
        }
        record.entry(k).or_insert(v);
    }
    record.insert("disabled".into(), Value::Bool(disabled));
}

/// Go `json.NewEncoder(f).Encode(map)`: sorted keys, HTML-escaped strings, final newline.
pub fn encode_go_json(value: &Value) -> String {
    fn write(out: &mut String, value: &Value) {
        match value {
            Value::Object(map) => {
                let sorted: BTreeMap<&String, &Value> = map.iter().collect();
                out.push('{');
                for (i, (k, v)) in sorted.into_iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&crate::codex_json::go_quote(k, true));
                    out.push(':');
                    write(out, v);
                }
                out.push('}');
            }
            Value::Array(items) => {
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(out, v);
                }
                out.push(']');
            }
            Value::String(s) => out.push_str(&crate::codex_json::go_quote(s, true)),
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    write(&mut out, value);
    out.push('\n');
    out
}

/// Writes a login record into `auth_dir` (atomic, 0600), merging an existing file the way
/// Go's login manager does. Blocking.
pub fn write_login(auth_dir: &Path, tokens: &Tokens) -> Result<PathBuf, ExecError> {
    let (name, mut record) = login_record(tokens, &rfc3339(Local::now()))?;
    if name.contains(['/', '\\']) {
        return Err(oauth_error(400, "invalid credential email filename"));
    }
    let path = auth_dir.join(&name);
    if let Ok(raw) = std::fs::read(&path)
        && let Ok(existing) = serde_json::from_slice::<Map<String, Value>>(&raw)
    {
        merge_existing(&mut record, existing);
    }
    let text = encode_go_json(&Value::Object(record));
    // ponytail: Go truncates in place with default permissions; this writes a 0600 temp
    // file and renames it, like the Claude login.
    publish(auth_dir, &path, text.as_bytes()).map_err(|_| oauth_error(500, "cannot publish codex credential"))?;
    Ok(path)
}

fn publish(dir: &Path, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)?;
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).map_err(std::io::Error::other)?;
    let temp = dir.join(format!(
        ".codex-{}.tmp",
        nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        std::fs::File::open(dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// `GenerateRandomState`: 16 random bytes, hex.
fn random_state() -> Result<String, ExecError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| oauth_error(500, "secure random source unavailable"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// `GenerateAuthURL`. Go's `url.Values.Encode` sorts parameters.
pub fn authorize_url(state: &str, challenge: &str, port: u16) -> String {
    let redirect = redirect_uri(port);
    let mut url = Url::parse(AUTH_URL).expect("constant URL");
    url.query_pairs_mut().extend_pairs([
        ("client_id", CLIENT_ID),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("codex_cli_simplified_flow", "true"),
        ("id_token_add_organizations", "true"),
        ("prompt", "login"),
        ("redirect_uri", &redirect),
        ("response_type", "code"),
        ("scope", "openid email profile offline_access"),
        ("state", state),
    ]);
    url.into()
}

pub struct LoginOptions {
    pub callback_port: u16,
    pub no_browser: bool,
}

impl Default for LoginOptions {
    fn default() -> Self {
        Self {
            callback_port: DEFAULT_CALLBACK_PORT,
            no_browser: false,
        }
    }
}

fn open_browser(url: &str) {
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = url;
}

/// Browser PKCE login (`-codex-login`). Only the explicit CLI flag calls this.
pub async fn login(auth_dir: &Path, options: &LoginOptions) -> Result<PathBuf, ExecError> {
    let port = options.callback_port;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|_| oauth_error(500, &format!("port {port} is already in use")))?;
    let (verifier, challenge) = crate::oauth::pkce()?;
    let state = random_state()?;
    let url = authorize_url(&state, &challenge, port);
    if !options.no_browser {
        println!("Opening browser for Codex authentication");
        open_browser(&url);
    }
    println!("Visit the following URL to continue authentication:\n{url}");
    println!("Waiting for Codex authentication callback...");
    let manual = async {
        // Go offers one paste prompt after 15 seconds for remote/headless logins.
        tokio::time::sleep(Duration::from_secs(15)).await;
        match prompt_line(
            "Paste the Codex callback URL (or press Enter to keep waiting): ",
            || {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line).map(|_| line)
            },
        )
        .await
        {
            Ok(line) if line.trim().is_empty() => std::future::pending().await,
            Ok(line) => parse_manual_callback(&line),
            Err(_) => Err(oauth_error(400, "failed to read callback URL")),
        }
    };
    let callback = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(300), callback(&listener)) => {
            result.map_err(|_| oauth_error(408, "timeout waiting for OAuth callback"))?
        }
        result = manual => result,
    }?;
    if let Some(error) = callback.error {
        return Err(oauth_error(400, &format!("codex oauth error: {error}")));
    }
    if callback.state != state {
        return Err(oauth_error(400, "state mismatch"));
    }
    let client = plain_client()?;
    let tokens = CodexOAuth::new(client)
        .exchange(&callback.code, &redirect_uri(port), &verifier)
        .await?;
    let dir = auth_dir.to_owned();
    tokio::task::spawn_blocking(move || write_login(&dir, &tokens))
        .await
        .map_err(|_| oauth_error(500, "credential publication failed"))?
}

/// One prompt on a detached OS thread (`misc.AsyncPrompt`). Unlike `spawn_blocking`, an
/// abandoned read never keeps the runtime from shutting down once the callback wins.
/// EOF yields the partial line, as Go's prompt does.
async fn prompt_line(
    prompt: &'static str,
    read: impl FnOnce() -> std::io::Result<String> + Send + 'static,
) -> std::io::Result<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        use std::io::Write;
        print!("{prompt}");
        let _ = std::io::stdout().flush();
        let _ = tx.send(read());
    });
    rx.await
        .unwrap_or_else(|_| Err(std::io::Error::other("prompt thread ended")))
}

/// Device-code login (`-codex-device-login`).
pub async fn device_login(auth_dir: &Path, options: &LoginOptions) -> Result<PathBuf, ExecError> {
    let oauth = CodexOAuth::new(plain_client()?);
    let code = oauth.device_code().await?;
    println!("Starting Codex device authentication...");
    println!("Codex device URL: {DEVICE_VERIFICATION_URL}");
    println!("Codex device code: {}", code.user_code);
    if !options.no_browser {
        open_browser(DEVICE_VERIFICATION_URL);
    }
    let tokens = oauth.device_poll(&code, Duration::from_secs(15 * 60)).await?;
    let dir = auth_dir.to_owned();
    tokio::task::spawn_blocking(move || write_login(&dir, &tokens))
        .await
        .map_err(|_| oauth_error(500, "credential publication failed"))?
}

fn plain_client() -> Result<wreq::Client, ExecError> {
    wreq::Client::builder()
        .redirect(wreq::redirect::Policy::none())
        .build()
        .map_err(|_| oauth_error(500, "cannot create OAuth transport"))
}

#[derive(Debug, PartialEq, Eq)]
struct Callback {
    code: String,
    state: String,
    error: Option<String>,
}

/// `misc.ParseOAuthCallback` for a pasted URL or query string.
fn parse_manual_callback(input: &str) -> Result<Callback, ExecError> {
    let trimmed = input.trim();
    let candidate = if trimmed.contains("://") {
        trimmed.to_owned()
    } else if trimmed.starts_with('?') {
        format!("http://localhost{trimmed}")
    } else if trimmed.contains(['/', '?', '#', ':']) {
        format!("http://{trimmed}")
    } else if trimmed.contains('=') {
        format!("http://localhost/?{trimmed}")
    } else {
        return Err(oauth_error(400, "invalid callback URL"));
    };
    let url = Url::parse(&candidate).map_err(|_| oauth_error(400, "invalid callback URL"))?;
    fn first<'a>(
        mut pairs: impl Iterator<Item = (std::borrow::Cow<'a, str>, std::borrow::Cow<'a, str>)>,
        k: &str,
    ) -> String {
        pairs
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.trim().to_owned())
            .unwrap_or_default()
    }
    let query = |k: &str| first(url.query_pairs(), k);
    let fragment = |k: &str| {
        url.fragment()
            .map(|f| first(url::form_urlencoded::parse(f.as_bytes()), k))
            .unwrap_or_default()
    };
    let pick = |k: &str| Some(query(k)).filter(|s| !s.is_empty()).unwrap_or_else(|| fragment(k));
    let (mut code, mut state, mut error) = (pick("code"), pick("state"), pick("error"));
    let description = pick("error_description");
    if !code.is_empty()
        && state.is_empty()
        && let Some((c, s)) = code.clone().split_once('#')
    {
        code = c.to_owned();
        state = s.to_owned();
    }
    if error.is_empty() && !description.is_empty() {
        error = description;
    }
    if code.is_empty() && error.is_empty() {
        return Err(oauth_error(400, "callback URL missing code"));
    }
    Ok(Callback {
        code,
        state,
        error: (!error.is_empty()).then_some(error),
    })
}

const SUCCESS_HTML: &str = "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"UTF-8\"><title>Authentication Successful - Codex</title></head><body><h1>Authentication successful</h1><p>You can close this window and return to the terminal.</p></body></html>";

/// The local callback listener (`/auth/callback`, `/success`). One request per
/// connection; anything else gets 404.
// ponytail: bounded HTTP/1 parser, not a general server; Go's styled success page with the
// platform setup notice is reduced to a short page.
async fn callback(listener: &tokio::net::TcpListener) -> Result<Callback, ExecError> {
    loop {
        let (mut socket, _) = listener
            .accept()
            .await
            .map_err(|_| oauth_error(500, "callback listener failed"))?;
        let handled = tokio::time::timeout(Duration::from_secs(10), async {
            let mut head = Vec::new();
            let mut byte = [0u8];
            while head.len() < 8192 && !head.ends_with(b"\r\n\r\n") {
                if socket.read(&mut byte).await? == 0 {
                    break;
                }
                head.push(byte[0]);
            }
            let (response, result) = route_callback(&head);
            socket.write_all(response.as_bytes()).await?;
            Ok::<_, std::io::Error>(result)
        })
        .await;
        if let Ok(Ok(Some(result))) = handled {
            return Ok(result);
        }
    }
}

fn http_response(status: &str, content_type: &str, extra: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn route_callback(head: &[u8]) -> (String, Option<Callback>) {
    let text = String::from_utf8_lossy(head);
    let mut line = text.lines().next().unwrap_or_default().split_whitespace();
    let (method, target) = (line.next().unwrap_or_default(), line.next().unwrap_or_default());
    let Ok(url) = Url::parse(&format!("http://localhost{target}")) else {
        return (http_response("400 Bad Request", "text/plain", "", "bad request"), None);
    };
    match url.path() {
        "/success" => (
            http_response("200 OK", "text/html; charset=utf-8", "", SUCCESS_HTML),
            None,
        ),
        "/auth/callback" if method != "GET" => (
            http_response(
                "405 Method Not Allowed",
                "text/plain; charset=utf-8",
                "",
                "Method not allowed\n",
            ),
            None,
        ),
        "/auth/callback" => {
            let get = |k: &str| {
                url.query_pairs()
                    .find(|(key, _)| key == k)
                    .map(|(_, v)| v.into_owned())
                    .unwrap_or_default()
            };
            let bad = |message: &str, error: &str| {
                (
                    http_response(
                        "400 Bad Request",
                        "text/plain; charset=utf-8",
                        "",
                        &format!("{message}\n"),
                    ),
                    Some(Callback {
                        code: String::new(),
                        state: String::new(),
                        error: Some(error.to_owned()),
                    }),
                )
            };
            let error = get("error");
            if !error.is_empty() {
                return bad(&format!("OAuth error: {error}"), &error);
            }
            if get("code").is_empty() {
                return bad("No authorization code received", "no_code");
            }
            if get("state").is_empty() {
                return bad("No state parameter received", "no_state");
            }
            (
                http_response("302 Found", "text/html; charset=utf-8", "Location: /success\r\n", ""),
                Some(Callback {
                    code: get("code"),
                    state: get("state"),
                    error: None,
                }),
            )
        }
        _ => (
            http_response("404 Not Found", "text/plain; charset=utf-8", "", "404 page not found\n"),
            None,
        ),
    }
}

#[cfg(test)]
#[path = "codex_oauth_tests.rs"]
mod tests;
