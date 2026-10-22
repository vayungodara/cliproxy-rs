//! Antigravity OAuth (internal/auth/antigravity, sdk/auth/antigravity.go) and the
//! executor's credential preparation (antigravity_executor_auth.go).
//!
//! An Antigravity credential is a Google OAuth token plus the Cloud Code project it
//! works in. Requests wait for the project ID (Go's `RequestAuthPreparer`): it comes from
//! `loadCodeAssist`, else from onboarding the user to the default tier. Tokens refresh in
//! the background 30 minutes before they expire, and before a project lookup when less
//! than five minutes remain.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use cpa_common::gostr::{GoStr, values_encode};
use cpa_common::json::{self as gj, GoValue};
use cpa_core::config::Config;
use cpa_core::credential::{Credential, MetadataPatch};
use cpa_core::exec::{ExecError, FailureScope};
use serde_json::{Map, Value};

use crate::antigravity_version as version;
use crate::meta_wire::{self, Slot};
use crate::proxy::{self, GoClients, GoHeaders, Proxy, Upstream};

pub const PROVIDER: &str = "antigravity";
/// The Antigravity desktop app's OAuth client (internal/auth/antigravity/constants.go).
pub const CLIENT_ID: &str = "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
pub const CLIENT_SECRET: &str = "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";
pub const DEFAULT_CALLBACK_PORT: u16 = 51121;
pub const SCOPES: [&str; 5] = [
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];
pub const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
pub const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const USERINFO_ENDPOINT: &str = "https://www.googleapis.com/oauth2/v2/userinfo?alt=json";
pub const API_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";
pub const DAILY_API_ENDPOINT: &str = "https://daily-cloudcode-pa.googleapis.com";
pub const API_VERSION: &str = "v1internal";
/// `AntigravityAuthenticator.RefreshLead`.
pub const REFRESH_LEAD: chrono::TimeDelta = chrono::TimeDelta::minutes(30);
/// `antigravityRequestTokenSafetyWindow`.
const TOKEN_SAFETY_WINDOW: chrono::TimeDelta = chrono::TimeDelta::minutes(5);
/// `antigravityCredentialAcquisitionTimeout`.
pub(crate) const ACQUISITION_TIMEOUT: Duration = Duration::from_secs(30);
const ONBOARD_ATTEMPTS: u32 = 5;
const ONBOARD_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);

/// A failed auth step: Go's error text, and the status of an `HTTPStatusError` (or a
/// `statusErr`), with its retry hint.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthError {
    pub status: Option<u16>,
    pub message: String,
    pub retry_after: Option<Duration>,
}

impl AuthError {
    fn plain(message: impl Into<String>) -> Self {
        Self {
            status: None,
            message: message.into(),
            retry_after: None,
        }
    }

    fn http(status: u16, message: impl Into<String>) -> Self {
        Self {
            status: Some(status),
            message: message.into(),
            retry_after: None,
        }
    }

    /// Go's `fmt.Errorf("%s: %w", prefix, err)`: the status survives wrapping.
    fn wrap(self, prefix: &str) -> Self {
        Self {
            message: format!("{prefix}: {}", self.message),
            ..self
        }
    }
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn transport_text(error: &ExecError) -> String {
    String::from_utf8_lossy(&error.body).into_owned()
}

/// `CredentialFileName`.
pub fn credential_file_name(email: &str) -> String {
    match email.trim() {
        "" => "antigravity.json".into(),
        email => format!("antigravity-{email}.json"),
    }
}

/// `AntigravityDefaultCallbackURI` for a callback port.
pub fn redirect_uri(port: u16) -> String {
    format!("http://localhost:{port}/oauth-callback")
}

/// `BuildAuthURL`.
pub fn auth_url(state: &str, redirect_uri: &str) -> String {
    let redirect = if redirect_uri.trim().is_empty() {
        self::redirect_uri(DEFAULT_CALLBACK_PORT)
    } else {
        redirect_uri.to_owned()
    };
    let scope = SCOPES.join(" ");
    let query = values_encode(&[
        ("access_type", "offline"),
        ("client_id", CLIENT_ID),
        ("prompt", "consent"),
        ("redirect_uri", &redirect),
        ("response_type", "code"),
        ("scope", &scope),
        ("state", state),
    ]);
    format!("{AUTH_ENDPOINT}?{query}")
}

/// A token endpoint answer (`TokenResponse`, `antigravityTokenRefreshData`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
    pub token_type: String,
}

impl Tokens {
    /// `json.Unmarshal` (`first_value`: `json.NewDecoder(...).Decode`, which reads only
    /// the first JSON value) into the token struct.
    fn decode(data: &[u8], first_value: bool, name: &str) -> Result<Self, String> {
        let mut tokens = Self::default();
        let mut fields = [
            ("access_token", Slot::Str(&mut tokens.access_token)),
            ("refresh_token", Slot::Str(&mut tokens.refresh_token)),
            ("expires_in", Slot::Int(&mut tokens.expires_in, "int64")),
            ("token_type", Slot::Str(&mut tokens.token_type)),
        ];
        let data = if first_value { first_json_value(data)? } else { data };
        meta_wire::unmarshal(data, "antigravity", name, &mut fields)?;
        Ok(tokens)
    }
}

/// What `json.Decoder.Decode` reads: the object at the start of `data` (after white
/// space), or the whole input for anything else, whose decoding fails either way.
fn first_json_value(data: &[u8]) -> Result<&[u8], String> {
    let start = data
        .iter()
        .position(|c| !matches!(c, b' ' | b'\t' | b'\r' | b'\n'))
        .ok_or("EOF")?;
    let value = &data[start..];
    if value.starts_with(b"{") {
        return meta_wire::object_end(value)
            .map(|end| &value[..end])
            .ok_or_else(|| "unexpected EOF".to_owned());
    }
    Ok(value)
}

/// `json.Unmarshal(body, &map[string]any)`: `None` for `null`.
fn decode_map(data: &[u8]) -> Result<Option<std::collections::BTreeMap<String, GoValue>>, String> {
    meta_wire::check_valid(data)?;
    let value = GoValue::parse_f64(data).map(GoValue::into_object);
    match value {
        Some(Ok(map)) => Ok(Some(map)),
        Some(Err(GoValue::Null)) => Ok(None),
        Some(Err(GoValue::Array(_))) => {
            Err("json: cannot unmarshal array into Go value of type map[string]interface {}".into())
        }
        Some(Err(GoValue::String(_))) => {
            Err("json: cannot unmarshal string into Go value of type map[string]interface {}".into())
        }
        Some(Err(GoValue::Bool(_))) => {
            Err("json: cannot unmarshal bool into Go value of type map[string]interface {}".into())
        }
        Some(Err(_)) => Err("json: cannot unmarshal number into Go value of type map[string]interface {}".into()),
        None => Err("json: cannot unmarshal number into Go value of type float64".into()),
    }
}

type GoMap = std::collections::BTreeMap<String, GoValue>;

fn go_str<'a>(map: &'a GoMap, key: &str) -> Option<&'a str> {
    match map.get(key) {
        Some(GoValue::String(s)) => Some(s),
        _ => None,
    }
}

/// `extractCloudaicompanionProject`.
fn extract_project(data: Option<&GoMap>) -> String {
    let Some(data) = data else {
        return String::new();
    };
    for key in ["cloudaicompanionProject", "projectId", "project"] {
        let found = match data.get(key) {
            Some(GoValue::String(s)) => s.trim(),
            Some(GoValue::Object(inner)) => go_str(inner, "id").map(str::trim).unwrap_or_default(),
            _ => "",
        };
        if !found.is_empty() {
            return found.to_owned();
        }
    }
    String::new()
}

/// `defaultAntigravityTierID`: the default allowed tier, else the current tier, else
/// `free-tier`.
fn default_tier(load: Option<&GoMap>) -> String {
    let Some(load) = load else {
        return "free-tier".into();
    };
    if let Some(GoValue::Array(tiers)) = load.get("allowedTiers") {
        for tier in tiers {
            let GoValue::Object(tier) = tier else { continue };
            if !matches!(tier.get("isDefault"), Some(GoValue::Bool(true))) {
                continue;
            }
            if let Some(id) = go_str(tier, "id").map(str::trim).filter(|id| !id.is_empty()) {
                return id.to_owned();
            }
        }
    }
    if let Some(GoValue::Object(current)) = load.get("currentTier")
        && let Some(id) = go_str(current, "id").map(str::trim).filter(|id| !id.is_empty())
    {
        return id.to_owned();
    }
    "free-tier".into()
}

/// The OAuth and Cloud Code calls of `AntigravityAuth`, over one client.
pub struct OAuth {
    client: wreq::Client,
    token_endpoint: String,
    userinfo_endpoint: String,
    api_endpoint: String,
    daily_api_endpoint: String,
    /// Pause between onboarding polls (two seconds; tests shorten it).
    poll_delay: Duration,
}

impl OAuth {
    pub fn new(client: wreq::Client) -> Self {
        Self::with_endpoints(
            client,
            TOKEN_ENDPOINT,
            USERINFO_ENDPOINT,
            API_ENDPOINT,
            DAILY_API_ENDPOINT,
        )
    }

    /// Redirects OAuth traffic to local mocks without changing the login flow.
    pub fn with_endpoints(client: wreq::Client, token: &str, userinfo: &str, api: &str, daily_api: &str) -> Self {
        Self {
            client,
            token_endpoint: token.into(),
            userinfo_endpoint: userinfo.into(),
            api_endpoint: api.into(),
            daily_api_endpoint: daily_api.into(),
            poll_delay: Duration::from_secs(2),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_poll_delay(mut self, delay: Duration) -> Self {
        self.poll_delay = delay;
        self
    }

    async fn post(
        &self,
        url: &str,
        headers: GoHeaders,
        body: Vec<u8>,
        timeout: Option<Duration>,
    ) -> Result<Upstream, ExecError> {
        proxy::request(
            &self.client,
            wreq::Method::POST,
            url,
            headers,
            Some(Bytes::from(body)),
            timeout,
        )
        .await
    }

    /// `ExchangeCodeForTokens`.
    pub async fn exchange_code(&self, code: &str, redirect_uri: &str) -> Result<Tokens, AuthError> {
        const STEP: &str = "antigravity token exchange";
        let form = values_encode(&[
            ("code", code),
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
        ]);
        let mut headers = GoHeaders::new();
        headers.set("Content-Type", "application/x-www-form-urlencoded");
        let upstream = self
            .post(&self.token_endpoint, headers, form.into_bytes(), None)
            .await
            .map_err(|e| AuthError::plain(format!("{STEP}: execute request: {}", transport_text(&e))))?;
        let status = upstream.status;
        if !(200..300).contains(&status) {
            let body = proxy::read_all(upstream.body, 8 << 10, false)
                .await
                .map_err(|e| AuthError::plain(format!("{STEP}: read response: {}", transport_text(&e))))?;
            return Err(AuthError::http(
                status,
                status_message(STEP, status, &token_error_code(&body)),
            ));
        }
        let body = proxy::read_all(upstream.body, usize::MAX, false)
            .await
            .map_err(|e| AuthError::plain(format!("{STEP}: decode response: {}", transport_text(&e))))?;
        Tokens::decode(&body, true, "TokenResponse")
            .map_err(|e| AuthError::plain(format!("{STEP}: decode response: {e}")))
    }

    /// `FetchUserInfo`: the account email.
    pub async fn user_info(&self, access_token: &str) -> Result<String, AuthError> {
        const STEP: &str = "antigravity userinfo";
        let access_token = access_token.trim();
        if access_token.is_empty() {
            return Err(AuthError::plain(format!("{STEP}: missing access token")));
        }
        let mut headers = GoHeaders::new();
        headers.set("Authorization", format!("Bearer {access_token}"));
        headers.set("User-Agent", version::request_user_agent(""));
        let upstream = proxy::request(
            &self.client,
            wreq::Method::GET,
            &self.userinfo_endpoint,
            headers,
            None,
            None,
        )
        .await
        .map_err(|e| AuthError::plain(format!("{STEP}: execute request: {}", transport_text(&e))))?;
        let status = upstream.status;
        if !(200..300).contains(&status) {
            let body = proxy::read_all(upstream.body, 8 << 10, false)
                .await
                .map_err(|e| AuthError::plain(format!("{STEP}: read response: {}", transport_text(&e))))?;
            return Err(AuthError::http(status, status_message(STEP, status, &body)));
        }
        let body = proxy::read_all(upstream.body, usize::MAX, false)
            .await
            .map_err(|e| AuthError::plain(format!("{STEP}: decode response: {}", transport_text(&e))))?;
        let mut email = String::new();
        let mut fields = [("email", Slot::Str(&mut email))];
        first_json_value(&body)
            .and_then(|value| meta_wire::unmarshal(value, "antigravity", "userInfo", &mut fields))
            .map_err(|e| AuthError::plain(format!("{STEP}: decode response: {e}")))?;
        let email = email.trim();
        if email.is_empty() {
            return Err(AuthError::plain(format!("{STEP}: response missing email")));
        }
        Ok(email.to_owned())
    }

    /// `FetchProjectID`: `loadCodeAssist`'s project, else onboarding to its default tier.
    pub async fn project_id(&self, access_token: &str) -> Result<String, AuthError> {
        let body = br#"{"metadata":{"ideType":"ANTIGRAVITY"}}"#.to_vec();
        let mut headers = GoHeaders::new();
        headers.set("Authorization", format!("Bearer {access_token}"));
        headers.set("Accept", "*/*");
        headers.set("Content-Type", "application/json");
        headers.set("User-Agent", version::request_user_agent(""));
        let url = format!("{}/{API_VERSION}:loadCodeAssist", self.api_endpoint);
        let upstream = self
            .post(&url, headers, body, None)
            .await
            .map_err(|e| AuthError::plain(format!("execute request: {}", transport_text(&e))))?;
        let status = upstream.status;
        let data = proxy::read_all(upstream.body, usize::MAX, false)
            .await
            .map_err(|e| AuthError::plain(format!("read response: {}", transport_text(&e))))?;
        if !(200..300).contains(&status) {
            return Err(AuthError::http(
                status,
                format!(
                    "request failed with status {status}: {}",
                    String::from_utf8_lossy(cpa_common::gostr::trim_space(&data))
                ),
            ));
        }
        let load = decode_map(&data).map_err(|e| AuthError::plain(format!("decode response: {e}")))?;
        let project = extract_project(load.as_ref());
        if !project.is_empty() {
            return Ok(project);
        }
        let project = self.onboard(access_token, &default_tier(load.as_ref())).await?;
        if project.is_empty() {
            return Err(AuthError::plain(
                "project id not found in loadCodeAssist or onboardUser response",
            ));
        }
        Ok(project)
    }

    /// `OnboardUser`: polls `onboardUser` until the operation is done, five times at most.
    pub async fn onboard(&self, access_token: &str, tier: &str) -> Result<String, AuthError> {
        tracing::info!("Antigravity: onboarding user with tier: {tier}");
        let user_agent = version::onboard_user_user_agent("");
        let s = |v: &str| GoValue::String(v.to_owned());
        let metadata = GoMap::from([
            ("ide_type".to_owned(), s("ANTIGRAVITY")),
            (
                "ide_version".to_owned(),
                s(&version::version_from_user_agent(&user_agent)),
            ),
            ("ide_name".to_owned(), s("antigravity")),
        ]);
        let body = GoValue::Object(GoMap::from([
            ("tier_id".to_owned(), s(tier)),
            ("metadata".to_owned(), GoValue::Object(metadata)),
        ]))
        .marshal();
        let url = format!("{}/{API_VERSION}:onboardUser", self.daily_api_endpoint);
        for attempt in 1..=ONBOARD_ATTEMPTS {
            tracing::debug!("Polling attempt {attempt}/{ONBOARD_ATTEMPTS}");
            let mut headers = GoHeaders::new();
            headers.set("Authorization", format!("Bearer {access_token}"));
            headers.set("Accept", "*/*");
            headers.set("Content-Type", "application/json");
            headers.set("User-Agent", user_agent.clone());
            headers.set("X-Goog-Api-Client", version::GOOG_API_CLIENT_UA);
            let read = async {
                let upstream = self
                    .post(&url, headers, body.clone(), None)
                    .await
                    .map_err(|e| AuthError::plain(format!("execute request: {}", transport_text(&e))))?;
                let status = upstream.status;
                let data = proxy::read_all(upstream.body, usize::MAX, false)
                    .await
                    .map_err(|e| AuthError::plain(format!("read response: {}", transport_text(&e))))?;
                Ok::<_, AuthError>((status, data))
            };
            let (status, data) = tokio::time::timeout(ONBOARD_ATTEMPT_TIMEOUT, read)
                .await
                .map_err(|_| AuthError::plain("execute request: context deadline exceeded"))??;
            if status == 200 {
                let response = decode_map(&data).map_err(|e| AuthError::plain(format!("decode response: {e}")))?;
                let response = response.unwrap_or_default();
                if matches!(response.get("done"), Some(GoValue::Bool(true))) {
                    let project = match response.get("response") {
                        Some(GoValue::Object(inner)) => extract_project(Some(inner)),
                        _ => String::new(),
                    };
                    if project.is_empty() {
                        return Err(AuthError::plain("no project_id in response"));
                    }
                    tracing::info!("Successfully fetched project_id: {}", hide(&project));
                    return Ok(project);
                }
                tokio::time::sleep(self.poll_delay).await;
                continue;
            }
            // Go cuts the body to 500 bytes for its preview, then to 200 for the error.
            let preview = cpa_common::gostr::trim_space(&data);
            let preview = &preview[..preview.len().min(500)];
            let preview = &preview[..preview.len().min(200)];
            return Err(AuthError::http(
                status,
                format!("http {status}: {}", String::from_utf8_lossy(preview)),
            ));
        }
        Err(AuthError::plain(format!(
            "onboard user did not complete after {ONBOARD_ATTEMPTS} attempts"
        )))
    }

    /// The executor's `refreshTokenSingleFlight`: a refresh grant with Go's fixed Host
    /// and user agent. A 429 carries Google's retry hint.
    pub async fn refresh(&self, refresh_token: &str) -> Result<Tokens, ExecError> {
        let form = values_encode(&[
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ]);
        let mut headers = GoHeaders::new();
        headers.set("Host", "oauth2.googleapis.com");
        headers.set("Content-Type", "application/x-www-form-urlencoded");
        headers.set("User-Agent", "Go-http-client/2.0");
        let upstream = self
            .post(&self.token_endpoint, headers, form.into_bytes(), None)
            .await?;
        let status = upstream.status;
        let data = proxy::read_all(upstream.body, usize::MAX, false).await?;
        if !(200..300).contains(&status) {
            let code = token_error_code(&data);
            let message = if code.is_empty() {
                format!("token refresh failed with status {status}")
            } else {
                format!(
                    "token refresh failed with status {status}: {}",
                    String::from_utf8_lossy(&code)
                )
            };
            let mut error = crate::gemini::status_err(status, message);
            if status == 429 {
                error.retry_after = parse_retry_delay(&data);
            }
            return Err(error);
        }
        Tokens::decode(&data, false, "antigravityTokenRefreshData")
            .map_err(|e| ExecError::local(500, FailureScope::Credential, e))
    }
}

/// What a failed token endpoint answer may show: its OAuth `error` code (or a Google
/// API error's `status`) when it is a short plain token, else nothing. Go shows the whole
/// body; a token endpoint is sent the authorization code, the refresh token and the
/// client secret, so its body stays out of errors, logs and client answers, as for the
/// other providers' sign-ins (docs/DIFFERENCES-FROM-GO.md).
fn token_error_code(body: &[u8]) -> Vec<u8> {
    let error = gj::get(body, "error");
    let code = if error.kind == gj::Kind::String {
        error.bytes().into_owned()
    } else {
        error.get("status").bytes().into_owned()
    };
    if code.len() <= 64
        && code
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.'))
    {
        code
    } else {
        Vec::new()
    }
}

/// `HTTPStatusError` texts of the token exchange and user info calls.
fn status_message(step: &str, status: u16, body: &[u8]) -> String {
    let body = String::from_utf8_lossy(cpa_common::gostr::trim_space(body)).into_owned();
    if body.is_empty() {
        format!("{step}: request failed: status {status}")
    } else {
        format!("{step}: request failed: status {status}: {body}")
    }
}

/// `util.HideAPIKey` (on bytes, as Go): four bytes at each end of a value longer than
/// eight, two above four, one above two.
fn hide(value: &str) -> String {
    let bytes = value.as_bytes();
    let keep = match bytes.len() {
        9.. => 4,
        5.. => 2,
        3.. => 1,
        _ => return value.to_owned(),
    };
    String::from_utf8_lossy(&[&bytes[..keep], b"...", &bytes[bytes.len() - keep..]].concat()).into_owned()
}

/// `helps.ParseRetryDelay`: Google's `RetryInfo.retryDelay`, then
/// `ErrorInfo.metadata.quotaResetDelay`, then "after Ns" in the message, then a
/// positive "after 1h2m3s" in the lowercased message.
pub(crate) fn parse_retry_delay(body: &[u8]) -> Option<Duration> {
    let details = gj::get(body, "error.details");
    if details.exists() && details.is_array() {
        for detail in details.array() {
            if detail.get("@type").str() != "type.googleapis.com/google.rpc.RetryInfo" {
                continue;
            }
            let delay = detail.get("retryDelay").str().into_owned();
            if delay.is_empty() {
                continue;
            }
            // Go returns an error (no hint) when the first RetryInfo delay is malformed.
            return cpa_core::config::parse_duration(&delay).map(nanos);
        }
        for detail in details.array() {
            if detail.get("@type").str() != "type.googleapis.com/google.rpc.ErrorInfo" {
                continue;
            }
            let delay = detail.get("metadata.quotaResetDelay").str().into_owned();
            if delay.is_empty() {
                continue;
            }
            if let Some(parsed) = cpa_core::config::parse_duration(&delay) {
                return Some(nanos(parsed));
            }
        }
    }
    let message = gj::get(body, "error.message").str().into_owned();
    if message.is_empty() {
        return None;
    }
    static SECONDS: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"after\s+(\d+)s\.?").unwrap());
    static HUMAN: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"after\s+((?:\d+h)?(?:\d+m)?(?:\d+s)?)\.?").unwrap());
    // strconv.Atoi, then time.Duration(seconds) * time.Second (wrapping like Go).
    if let Some(seconds) = SECONDS.captures(&message).and_then(|c| c[1].parse::<i64>().ok()) {
        return Some(nanos(seconds.wrapping_mul(1_000_000_000)));
    }
    let lower = message.go_lower();
    let human = HUMAN.captures(&lower)?;
    cpa_core::config::parse_duration(&human[1])
        .filter(|d| *d > 0)
        .map(nanos)
}

/// A Go `time.Duration` (nanoseconds); negative durations clamp to zero.
fn nanos(value: i64) -> Duration {
    Duration::from_nanos(value.max(0) as u64)
}

/// `antigravityProjectIDFromAuth`.
pub(crate) fn project_id(credential: &Credential) -> &str {
    credential.str("project_id").map(str::trim).unwrap_or_default()
}

/// `metaStringValue(auth.Metadata, key)`.
fn meta_string<'a>(credential: &'a Credential, key: &str) -> &'a str {
    credential.str(key).map(str::trim).unwrap_or_default()
}

/// Whether requests must wait for preparation: a missing project
/// (`ShouldPrepareRequestAuth`), or an access token Go's `ensureAccessToken` would
/// refresh before sending (empty, without an expiry, or inside five minutes of it).
pub(crate) fn must_prepare(credential: &Credential) -> bool {
    project_id(credential).is_empty() || !token_fresh(credential, Utc::now())
}

/// `ensureAccessToken`'s fast path: a token valid beyond the safety window.
fn token_fresh(credential: &Credential, now: DateTime<Utc>) -> bool {
    !meta_string(credential, "access_token").is_empty()
        && crate::kimi_http::expiration(credential).is_some_and(|e| e > now + TOKEN_SAFETY_WINDOW)
}

/// Whether the credential needs preparation at `now`: a missing project, a token Go's
/// `ensureAccessToken` would refresh, or a token inside the 30-minute refresh lead (the
/// background loop's `shouldRefresh`).
pub(crate) fn needs_prepare(credential: &Credential, now: DateTime<Utc>) -> bool {
    project_id(credential).is_empty()
        || !token_fresh(credential, now)
        || (!meta_string(credential, "refresh_token").is_empty()
            && crate::kimi_http::refresh_due(credential, Some(REFRESH_LEAD), now))
}

/// `refreshToken`'s metadata after a refresh grant at `now`.
fn refresh_patch(tokens: &Tokens, now: DateTime<Utc>) -> MetadataPatch {
    let mut set = Map::new();
    set.insert("access_token".into(), Value::String(tokens.access_token.clone()));
    if !tokens.refresh_token.is_empty() {
        set.insert("refresh_token".into(), Value::String(tokens.refresh_token.clone()));
    }
    set.insert("expires_in".into(), Value::from(tokens.expires_in));
    set.insert("timestamp".into(), Value::from(now.timestamp_millis()));
    let expired = now.with_timezone(&chrono::Local) + chrono::TimeDelta::seconds(tokens.expires_in);
    set.insert(
        "expired".into(),
        Value::String(expired.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
    );
    set.insert("type".into(), Value::String(PROVIDER.into()));
    MetadataPatch {
        set,
        remove: Vec::new(),
    }
}

/// `missingAntigravityProjectIDError`: 400 unless the cause carried a status.
fn missing_project(cause: Option<AuthError>) -> ExecError {
    let Some(cause) = cause else {
        return ExecError::local(400, FailureScope::Credential, "antigravity auth missing project_id");
    };
    let mut error = ExecError::local(
        cause.status.filter(|s| *s > 0).unwrap_or(400),
        FailureScope::Credential,
        format!("antigravity auth missing project_id: {}", cause.message),
    );
    error.retry_after = cause.retry_after;
    error
}

/// Credential preparation: `PrepareRequestAuth` for a credential without a project
/// (fresh access token, then the project lookup, whose failure blocks requests), else
/// the background `Refresh` (refresh grant; a project lookup failure only logs).
// ponytail: Go shares one refresh per refresh token across credentials (singleflight);
// the runtime's per-credential prepare lock covers the usual one-file-per-token case.
// Credits hints and refresh queues (antigravity_executor_credits.go) come with the
// executor.
pub(crate) async fn prepare(
    clients: &GoClients,
    credential: &Credential,
    cfg: &Config,
) -> Result<MetadataPatch, ExecError> {
    let oauth = OAuth::new(clients.get(&Proxy::effective(credential, cfg)));
    let now = Utc::now();
    let mut patch = MetadataPatch::default();
    let mut access = meta_string(credential, "access_token").to_owned();
    let fresh = token_fresh(credential, now);
    let missing = project_id(credential).is_empty();
    let lookup = |access: String| {
        let oauth = &oauth;
        async move {
            if access.trim().is_empty() {
                return Ok(String::new());
            }
            match tokio::time::timeout(ACQUISITION_TIMEOUT, oauth.project_id(&access)).await {
                Ok(result) => result,
                Err(_) => Err(AuthError::plain("context deadline exceeded")),
            }
        }
    };
    let refreshed = !missing || access.is_empty() || !fresh;
    if refreshed {
        let refresh = meta_string(credential, "refresh_token");
        if refresh.is_empty() {
            return Err(ExecError::local(401, FailureScope::Credential, "missing refresh token"));
        }
        let tokens = tokio::time::timeout(ACQUISITION_TIMEOUT, oauth.refresh(refresh))
            .await
            .map_err(|_| ExecError::local(500, FailureScope::Credential, "context deadline exceeded"))??;
        patch = refresh_patch(&tokens, now);
        access = tokens.access_token;
    }
    if !missing {
        return Ok(patch);
    }
    // A refresh already tried the lookup (ensureAntigravityProjectID, failure logged);
    // PrepareRequestAuth then looks again and reports its own outcome.
    let mut project = lookup(access.clone()).await;
    if refreshed {
        match &project {
            Ok(found) if !found.is_empty() => {}
            Ok(_) => project = lookup(access.clone()).await,
            Err(error) => {
                tracing::warn!("antigravity executor: ensure project id failed: {error}");
                project = lookup(access.clone()).await;
            }
        }
    }
    match project {
        Ok(project) if !project.is_empty() => {
            patch.set.insert("project_id".into(), Value::String(project));
            Ok(patch)
        }
        Ok(_) => Err(missing_project(None)),
        Err(cause) => Err(missing_project(Some(cause))),
    }
}

/// `-antigravity-login` options.
pub struct LoginOptions {
    pub no_browser: bool,
    pub callback_port: u16,
}

impl Default for LoginOptions {
    fn default() -> Self {
        Self {
            no_browser: false,
            callback_port: DEFAULT_CALLBACK_PORT,
        }
    }
}

/// `CompleteAntigravityOAuth`: the code exchange, the account email and the project,
/// as the credential file's name and metadata (`BuildAntigravityAuth`).
pub async fn complete(
    oauth: &OAuth,
    code: &str,
    redirect_uri: &str,
    now: DateTime<Utc>,
) -> Result<(String, Map<String, Value>), AuthError> {
    let tokens = oauth
        .exchange_code(code, redirect_uri)
        .await
        .map_err(|e| e.wrap("antigravity: token exchange failed"))?;
    let access = tokens.access_token.trim();
    if access.is_empty() {
        return Err(AuthError::plain(
            "antigravity: token exchange returned empty access token",
        ));
    }
    let email = oauth
        .user_info(access)
        .await
        .map_err(|e| e.wrap("antigravity: fetch user info failed"))?;
    let project = oauth
        .project_id(access)
        .await
        .map_err(|e| e.wrap("antigravity: failed to fetch project ID"))?;
    let project = project.trim();
    if project.is_empty() {
        return Err(AuthError::plain(
            "antigravity: project ID discovery returned empty project",
        ));
    }
    Ok(build_auth(&tokens, &email, project, now))
}

/// `BuildAntigravityAuth`: file name and metadata of a new credential.
pub fn build_auth(tokens: &Tokens, email: &str, project: &str, now: DateTime<Utc>) -> (String, Map<String, Value>) {
    let mut metadata = refresh_patch(tokens, now).set;
    // A login keeps the refresh token as returned, even when empty.
    metadata.insert("refresh_token".into(), Value::String(tokens.refresh_token.clone()));
    let email = email.trim();
    if !email.is_empty() {
        metadata.insert("email".into(), Value::String(email.into()));
    }
    let project = project.trim();
    if !project.is_empty() {
        metadata.insert("project_id".into(), Value::String(project.into()));
    }
    (credential_file_name(email), metadata)
}

/// Saves a CLI or management login through the shared file-token store.
pub fn write_login(auth_dir: &Path, file_name: &str, metadata: &Map<String, Value>) -> Result<PathBuf, String> {
    crate::devin_auth::save_metadata_file(auth_dir, file_name, metadata, PROVIDER)
}

/// One OAuth callback (`callbackResult`).
#[derive(Debug, Default)]
struct Callback {
    code: String,
    error: String,
    state: String,
}

/// `startAntigravityCallbackServer`: `/oauth-callback` (any method) answers with Go's
/// pages and hands the trimmed parameters to the login; other paths get Go's 404.
async fn serve_callbacks(listener: tokio::net::TcpListener, results: tokio::sync::mpsc::Sender<Callback>) {
    loop {
        let Ok((socket, _)) = listener.accept().await else {
            continue;
        };
        let results = results.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(10), respond(socket, results)).await;
        });
    }
}

async fn respond(
    mut socket: tokio::net::TcpStream,
    results: tokio::sync::mpsc::Sender<Callback>,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut head = Vec::new();
    let mut byte = [0];
    while head.len() < 8192 && !head.ends_with(b"\r\n\r\n") {
        if socket.read(&mut byte).await? == 0 {
            return Ok(());
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head);
    let target = text
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .nth(1)
        .unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let (content_type, extra, body) = if path == "/oauth-callback" {
        // url.Values.Get: the first value of each parameter.
        let first = |key: &str| {
            url::form_urlencoded::parse(query.as_bytes())
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.trim().to_owned())
                .unwrap_or_default()
        };
        let callback = Callback {
            code: first("code"),
            error: first("error"),
            state: first("state"),
        };
        let ok = !callback.code.is_empty() && callback.error.is_empty();
        let _ = results.try_send(callback);
        let body = if ok {
            "<h1>Login successful</h1><p>You can close this window.</p>"
        } else {
            "<h1>Login failed</h1><p>Please check the CLI output.</p>"
        };
        ("text/html; charset=utf-8", "", body)
    } else {
        (
            "text/plain; charset=utf-8",
            "X-Content-Type-Options: nosniff\r\n",
            "404 page not found\n",
        )
    };
    let status = if path == "/oauth-callback" {
        "200 OK"
    } else {
        "404 Not Found"
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await?;
    socket.shutdown().await
}

/// `-antigravity-login`: the browser flow, then `Manager.Login`'s save. Returns the
/// credential path and the account label.
pub async fn login(auth_dir: &Path, cfg: &Config, options: &LoginOptions) -> Result<(PathBuf, String), String> {
    let port = if options.callback_port == 0 {
        DEFAULT_CALLBACK_PORT
    } else {
        options.callback_port
    };
    let state = crate::oauth::random_hex(16).map_err(|e| transport_text(&e))?;
    let listener = match crate::claude_login::listen_any(port) {
        Ok(listener) => listener,
        Err(_) => tokio::net::TcpListener::bind(("0.0.0.0", port))
            .await
            .map_err(|e| format!("antigravity: failed to start callback server: listen tcp :{port}: {e}"))?,
    };
    let (tx, mut callbacks) = tokio::sync::mpsc::channel(1);
    let server = tokio::spawn(serve_callbacks(listener, tx));
    let redirect = redirect_uri(port);
    let url = auth_url(&state, &redirect);
    let show_manual = || {
        crate::claude_login::print_ssh_tunnel_instructions(port);
        println!("Visit the following URL to continue authentication:\n{url}");
    };
    if options.no_browser {
        show_manual();
    } else {
        println!("Opening browser for antigravity authentication");
        if !open_browser(&url) {
            tracing::warn!("No browser available; please open the URL manually");
            show_manual();
        }
    }
    println!("Waiting for antigravity authentication callback...");
    let result = async {
        let callback = wait_for_callback(&mut callbacks).await?;
        if !callback.error.is_empty() {
            return Err(format!("antigravity: authentication failed: {}", callback.error));
        }
        if callback.state != state {
            return Err("antigravity: invalid state".to_owned());
        }
        if callback.code.is_empty() {
            return Err("antigravity: missing authorization code".to_owned());
        }
        // NewAntigravityAuth(cfg, nil): the global proxy (util.SetProxy).
        let proxy = Proxy::parse(
            cfg.document
                .get("requests")
                .and_then(|r| r.get("proxy-url"))
                .and_then(serde_yaml_ng::Value::as_str)
                .unwrap_or_default(),
        );
        let oauth = OAuth::new(GoClients::new(Default::default()).get(&proxy));
        let (file_name, metadata) = complete(&oauth, &callback.code, &redirect, Utc::now())
            .await
            .map_err(|e| e.message)?;
        if let Some(project) = metadata.get("project_id").and_then(Value::as_str) {
            tracing::info!("antigravity: obtained project ID {}", hide(project));
            println!("Antigravity authentication successful");
            println!("Using GCP project: {}", hide(project));
        }
        let label = metadata
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or("antigravity")
            .to_owned();
        let path = write_login(auth_dir, &file_name, &metadata)?;
        Ok((path, label))
    }
    .await;
    server.abort();
    result
}

/// The wait: the first callback, a pasted callback URL after 15 seconds, or the
/// five-minute timeout.
async fn wait_for_callback(callbacks: &mut tokio::sync::mpsc::Receiver<Callback>) -> Result<Callback, String> {
    let deadline = tokio::time::sleep(Duration::from_secs(300));
    tokio::pin!(deadline);
    let manual = async {
        tokio::time::sleep(Duration::from_secs(15)).await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            use std::io::Write;
            print!("Paste the antigravity callback URL (or press Enter to keep waiting): ");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            let _ = tx.send(std::io::stdin().read_line(&mut line).map(|_| line));
        });
        rx.await
            .unwrap_or_else(|_| Err(std::io::Error::other("prompt thread ended")))
    };
    tokio::pin!(manual);
    let mut prompted = false;
    loop {
        tokio::select! {
            biased;
            Some(callback) = callbacks.recv() => return Ok(callback),
            () = &mut deadline => return Err("antigravity: authentication timed out".into()),
            input = &mut manual, if !prompted => {
                prompted = true;
                let input = input.map_err(|e| e.to_string())?;
                match crate::claude_login::parse_callback_input(&input)? {
                    Some(parsed) => {
                        return Ok(Callback {
                            code: parsed.code,
                            error: parsed.error,
                            state: parsed.state,
                        })
                    }
                    None => continue,
                }
            }
        }
    }
}

/// Opens `url` in the desktop browser; `false` when none is available.
fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "linux")]
    let command = std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    #[cfg(target_os = "macos")]
    let command = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let command = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let command: std::io::Result<std::process::Child> = Err(std::io::Error::other("no browser"));
    command.is_ok()
}

#[cfg(test)]
#[path = "antigravity_auth_tests.rs"]
mod tests;
