//! Meta Muse RFC 8628 device login, API-key minting and credential files
//! (internal/auth/meta/meta.go, sdk/auth/meta.go, internal/cmd/meta_login.go).
//!
//! Meta has no refresh-token grant. The device flow yields a DCA token (`dca:...`), which
//! is exchanged for an LLM API key at the mint endpoint; refresh re-mints from the saved
//! DCA token. The SDK refresh lead is nil: refresh happens on demand only.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use cpa_common::json::{self as gj, GoValue};
use cpa_core::exec::{ExecError, FailureScope};
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::kimi_auth::{form, open_browser, write_private};
use crate::kimi_http::rfc3339_utc;
use crate::meta_wire::{Slot, check_valid, unmarshal};
use crate::proxy::{GoHeaders, MAX_ERROR_BODY, read_all, send};

pub const DEFAULT_API_BASE_URL: &str = "https://api.meta.ai/v1";
pub const AUTH_HOST: &str = "https://auth.meta.com";
pub const CLIENT_ID: &str = "1031625952748946";
pub const DEFAULT_MINT_URL: &str = "https://api.meta.ai/muse-code/key";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const AUTH_USER_AGENT: &str = "muse-code/1.0.2";
/// Poll interval in server units when the device response has none.
const DEFAULT_POLL_INTERVAL: u64 = 5;
/// Upper bound on waiting for the user (MaxPollDuration), in seconds.
const MAX_POLL_SECONDS: u64 = 15 * 60;
/// `httpClientTimeout` for every credential-acquisition call.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Device authorization response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub expires_in: i64,
    pub interval: i64,
}

impl DeviceCode {
    #[cfg(test)]
    pub(crate) fn decode_for_test(raw: &[u8]) -> Result<Self, String> {
        Self::decode(raw)
    }

    /// `json.Unmarshal` into DeviceCodeResponse (`TokenEndpoint` is `json:"-"`).
    fn decode(raw: &[u8]) -> Result<Self, String> {
        let mut c = Self::default();
        unmarshal(
            raw,
            "meta",
            "DeviceCodeResponse",
            &mut [
                ("device_code", Slot::Str(&mut c.device_code)),
                ("user_code", Slot::Str(&mut c.user_code)),
                ("verification_uri", Slot::Str(&mut c.verification_uri)),
                ("verification_uri_complete", Slot::Str(&mut c.verification_uri_complete)),
                ("expires_in", Slot::Int(&mut c.expires_in, "int")),
                ("interval", Slot::Int(&mut c.interval, "int")),
            ],
        )?;
        Ok(c)
    }
}

/// The DCA token from the token endpoint. `expires_at` is Unix seconds, 0 when unknown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenData {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: i64,
    pub expires_at: i64,
    pub error: String,
    pub error_description: String,
}

impl TokenData {
    #[cfg(test)]
    pub(crate) fn decode_for_test(raw: &[u8]) -> (Self, Result<(), String>) {
        Self::decode(raw)
    }

    /// `json.Unmarshal` into TokenData. Fields decoded before a type error are kept, as
    /// Go does; the poll loop relies on that for error answers.
    fn decode(raw: &[u8]) -> (Self, Result<(), String>) {
        let mut t = Self::default();
        let result = unmarshal(
            raw,
            "meta",
            "TokenData",
            &mut [
                ("access_token", Slot::Str(&mut t.access_token)),
                ("token_type", Slot::Str(&mut t.token_type)),
                ("expires_in", Slot::Int(&mut t.expires_in, "int")),
                ("expires_at", Slot::Int(&mut t.expires_at, "int64")),
                ("error", Slot::Str(&mut t.error)),
                ("error_description", Slot::Str(&mut t.error_description)),
            ],
        );
        (t, result)
    }
}

/// The mint endpoint's answer (MintedKeyResponse).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MintedKey {
    pub api_key: String,
    pub base_url: String,
    pub user_email: String,
    pub user_full_name: String,
    pub subs_tier_name: String,
    pub subs_tier_id: String,
    pub is_subs_active: bool,
    pub has_payment_method: bool,
    pub require_payment: bool,
    pub can_subscribe: bool,
}

impl MintedKey {
    /// `json.Unmarshal` into MintedKeyResponse.
    pub fn decode(raw: &[u8]) -> Result<Self, String> {
        let mut m = Self::default();
        unmarshal(
            raw,
            "meta",
            "MintedKeyResponse",
            &mut [
                ("api_key", Slot::Str(&mut m.api_key)),
                ("base_url", Slot::Str(&mut m.base_url)),
                ("user_email", Slot::Str(&mut m.user_email)),
                ("user_full_name", Slot::Str(&mut m.user_full_name)),
                ("subs_tier_name", Slot::Str(&mut m.subs_tier_name)),
                ("subs_tier_id", Slot::Str(&mut m.subs_tier_id)),
                ("is_subs_active", Slot::Bool(&mut m.is_subs_active)),
                ("has_payment_method", Slot::Bool(&mut m.has_payment_method)),
                ("require_payment", Slot::Bool(&mut m.require_payment)),
                ("can_subscribe", Slot::Bool(&mut m.can_subscribe)),
            ],
        )?;
        Ok(m)
    }
}

/// Token data, minted key and user identity from one login (MetaAuthBundle).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bundle {
    pub token: TokenData,
    pub minted: Option<MintedKey>,
    pub email: String,
    pub name: String,
}

/// Why minting failed: Go's message without the mint endpoint's response body. Go appends
/// the body, which can carry key material, and the message reaches logs and API clients;
/// it is never kept here, so no formatting of this error can leak it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MintError {
    /// A non-2xx answer (its HTTP status).
    Status(u16),
    Other(String),
}

impl MintError {
    /// The message, safe for logs and API clients.
    pub fn redacted(&self) -> String {
        self.to_string()
    }
}

impl std::fmt::Display for MintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Status(status) => write!(f, "meta auth: mint key failed (HTTP {status})"),
            Self::Other(message) => f.write_str(message),
        }
    }
}

/// The mint endpoint: `META_MINT_URL` when set, otherwise Meta's production URL.
pub fn mint_url() -> String {
    std::env::var("META_MINT_URL")
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_MINT_URL.to_owned())
}

/// One Meta OAuth client (MetaAuth). Endpoints are fixed in production; tests point them
/// at local mocks.
#[derive(Clone)]
pub struct MetaAuth {
    client: wreq::Client,
    device_url: String,
    token_url: String,
    mint_url: String,
    /// Length of one server-side second while polling; shortened only in tests.
    poll_unit: Duration,
    /// Fixed clock for tests; `None` is the wall clock.
    fixed_now: Option<DateTime<Utc>>,
}

impl MetaAuth {
    pub fn new(client: wreq::Client) -> Self {
        Self {
            client,
            device_url: format!("{AUTH_HOST}/oidc/device/authorization/"),
            token_url: format!("{AUTH_HOST}/oidc/device/token/"),
            mint_url: mint_url(),
            poll_unit: Duration::from_secs(1),
            fixed_now: None,
        }
    }

    fn now(&self) -> DateTime<Utc> {
        self.fixed_now.unwrap_or_else(Utc::now)
    }

    /// Points device authorization and token polling at another host (local mocks).
    pub fn with_auth_host(mut self, host: &str) -> Self {
        let host = host.trim_end_matches('/');
        self.device_url = format!("{host}/oidc/device/authorization/");
        self.token_url = format!("{host}/oidc/device/token/");
        self
    }

    /// Overrides the mint endpoint (SetMintURL).
    pub fn with_mint_url(mut self, url: &str) -> Self {
        let url = url.trim();
        if !url.is_empty() {
            self.mint_url = url.to_owned();
        }
        self
    }

    #[cfg(test)]
    pub(crate) fn with_poll_unit(mut self, unit: Duration) -> Self {
        self.poll_unit = unit;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_fixed_now(mut self, now: DateTime<Utc>) -> Self {
        self.fixed_now = Some(now);
        self
    }

    async fn post(
        &self,
        url: &str,
        headers: GoHeaders,
        body: impl Into<bytes::Bytes>,
    ) -> Result<(u16, bytes::Bytes), String> {
        let upstream = send(&self.client, url, headers, body, Some(HTTP_TIMEOUT))
            .await
            .map_err(|e| String::from_utf8_lossy(&e.body).into_owned())?;
        let status = upstream.status;
        let body = read_all(upstream.body, MAX_ERROR_BODY, false)
            .await
            .map_err(|e| String::from_utf8_lossy(&e.body).into_owned())?;
        Ok((status, body))
    }

    fn form_headers() -> GoHeaders {
        let mut h = GoHeaders::new();
        h.set("Content-Type", "application/x-www-form-urlencoded");
        h.set("Accept", "application/json");
        h.set("User-Agent", AUTH_USER_AGENT);
        h
    }

    /// StartDeviceFlow.
    pub async fn start_device_flow(&self) -> Result<DeviceCode, String> {
        let (status, body) = self
            .post(
                &self.device_url,
                Self::form_headers(),
                form(&[("client_id", CLIENT_ID)]),
            )
            .await
            .map_err(|e| format!("meta device flow: request failed: {e}"))?;
        if !(200..300).contains(&status) {
            // Go appends the response body; it is withheld from logs and clients.
            return Err(format!("meta device flow failed (HTTP {status})"));
        }
        let code = DeviceCode::decode(&body).map_err(|e| format!("meta device flow: parse response: {e}"))?;
        if code.device_code.trim().is_empty() || code.user_code.trim().is_empty() {
            return Err("meta device flow: response missing required device_code or user_code".into());
        }
        Ok(code)
    }

    /// WaitForAuthorization: polls on a ticker until approved, denied or expired, then
    /// mints the API key. A mint failure is logged and the bundle carries only the DCA token.
    pub async fn wait_for_authorization(&self, code: &DeviceCode) -> Result<Bundle, String> {
        if code.device_code.is_empty() {
            return Err("meta auth: missing device code response".into());
        }
        let mut interval = if code.interval > 0 {
            code.interval as u64
        } else {
            DEFAULT_POLL_INTERVAL
        };
        let mut max_seconds = MAX_POLL_SECONDS;
        if code.expires_in > 0 {
            max_seconds = max_seconds.min(code.expires_in as u64);
        }
        let deadline = tokio::time::Instant::now() + self.poll_unit * max_seconds as u32;
        let ticker = |seconds: u64| {
            let period = self.poll_unit * seconds as u32;
            let mut t = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            // Go's ticker channel holds one pending tick; a slow poll is followed by one
            // immediate poll, not a burst.
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            t
        };
        let mut tick = ticker(interval);
        let timed_out = || "meta auth: authorization timed out or canceled: context deadline exceeded".to_owned();
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return Err(timed_out()),
                _ = tick.tick() => {}
            }
            let body = form(&[
                ("grant_type", DEVICE_GRANT),
                ("device_code", &code.device_code),
                ("client_id", CLIENT_ID),
            ]);
            let polled =
                match tokio::time::timeout_at(deadline, self.post(&self.token_url, Self::form_headers(), body)).await {
                    Err(_) => return Err(timed_out()),
                    Ok(polled) => polled,
                };
            let (status, body) = match polled {
                Ok(answer) => answer,
                Err(_) => {
                    tracing::warn!("meta auth: poll request error (retrying)");
                    continue;
                }
            };
            if status == 200 {
                let (mut token, decoded) = TokenData::decode(&body);
                decoded.map_err(|e| format!("meta auth: parse token response: {e}"))?;
                if token.access_token.is_empty() {
                    return Err("meta auth: response missing access_token".into());
                }
                if token.expires_in > 0 {
                    token.expires_at = self.now().timestamp() + token.expires_in;
                }
                let mut bundle = Bundle {
                    token,
                    ..Bundle::default()
                };
                let minted = tokio::time::timeout_at(deadline, self.mint_api_key(&bundle.token.access_token))
                    .await
                    .unwrap_or_else(|_| {
                        Err(MintError::Other(
                            "meta auth: mint request failed: context deadline exceeded".into(),
                        ))
                    });
                match minted {
                    Err(e) => tracing::warn!("meta auth: could not mint api_key from dca_token: {}", e.redacted()),
                    Ok(minted) => {
                        if !minted.user_email.is_empty() {
                            bundle.email = minted.user_email.clone();
                        }
                        if !minted.user_full_name.is_empty() {
                            bundle.name = minted.user_full_name.clone();
                        }
                        bundle.minted = Some(minted);
                    }
                }
                return Ok(bundle);
            }
            // Go ignores decode errors here and keeps whatever fields matched.
            let (answer, _) = TokenData::decode(&body);
            match answer.error.as_str() {
                "authorization_pending" => {}
                "slow_down" => {
                    interval += 5;
                    tick = ticker(interval);
                }
                "access_denied" => return Err("meta auth: access was denied by user".into()),
                "expired_token" => return Err("meta auth: device code has expired".into()),
                "" => tracing::warn!("meta auth: unexpected response {status}"),
                other => {
                    return Err(format!(
                        "meta auth: error from authorization server: {other}: {}",
                        answer.error_description
                    ));
                }
            }
        }
    }

    /// MintAPIKey, single-flighted per mint endpoint and DCA token across the process
    /// (metaRefreshGroup). Callers that go away do not cancel it.
    pub async fn mint_api_key(&self, dca_token: &str) -> Result<MintedKey, MintError> {
        let dca_token = dca_token.trim().to_owned();
        if dca_token.is_empty() {
            return Err(MintError::Other("meta auth: missing dca token".into()));
        }
        type Flight = Shared<BoxFuture<'static, Result<MintedKey, MintError>>>;
        static FLIGHTS: LazyLock<Mutex<HashMap<String, Flight>>> = LazyLock::new(Mutex::default);
        let key = format!("{}\n{dca_token}", self.mint_url);
        let flight = {
            let mut flights = FLIGHTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(flight) = flights.get(&key) {
                flight.clone()
            } else {
                let auth = self.clone();
                let task_key = key.clone();
                let task = tokio::spawn(async move {
                    let result = auth.mint_once(&dca_token).await;
                    FLIGHTS
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&task_key);
                    result
                });
                let flight = async move {
                    task.await
                        .map_err(|_| MintError::Other("meta auth: mint request failed".into()))?
                }
                .boxed()
                .shared();
                flights.insert(key, flight.clone());
                flight
            }
        };
        flight.await
    }

    async fn mint_once(&self, dca_token: &str) -> Result<MintedKey, MintError> {
        let mut headers = GoHeaders::new();
        headers.set("Authorization", format!("Bearer {dca_token}"));
        headers.set("User-Agent", AUTH_USER_AGENT);
        headers.set("Content-Type", "application/json");
        headers.set("Accept", "application/json");
        let mut body = br#"{"dca_token":"#.to_vec();
        body.extend_from_slice(&gj::quote(dca_token));
        body.push(b'}');
        let (status, body) = self
            .post(&self.mint_url, headers, body)
            .await
            .map_err(|e| MintError::Other(format!("meta auth: mint request failed: {e}")))?;
        if !(200..300).contains(&status) {
            return Err(MintError::Status(status));
        }
        let minted =
            MintedKey::decode(&body).map_err(|e| MintError::Other(format!("meta auth: parse mint response: {e}")))?;
        if minted.api_key.trim().is_empty() {
            return Err(MintError::Other("meta auth: mint response missing api_key".into()));
        }
        Ok(minted)
    }
}

/// Persisted credential fields (MetaTokenStorage). `type` and `auth_kind` are forced on
/// write and have no field.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
pub struct TokenStorage {
    pub access_token: String,
    pub dca_token: String,
    pub api_key: String,
    pub token_type: String,
    pub expires_in: i64,
    pub expired: String,
    pub dca_expired: String,
    pub dca_expires_at: i64,
    pub last_refresh: String,
    pub base_url: String,
    pub email: String,
    pub name: String,
}

/// CreateTokenStorage. With a minted key the key is the access token and `expired` stays
/// empty; without one the DCA token is used and its expiry is tracked.
pub fn create_token_storage(bundle: &Bundle, now: DateTime<Utc>) -> TokenStorage {
    let dca_expired = if bundle.token.expires_at > 0 {
        DateTime::from_timestamp(bundle.token.expires_at, 0)
            .map(rfc3339_utc)
            .unwrap_or_default()
    } else {
        String::new()
    };
    let (mut api_key, mut base_url) = (String::new(), DEFAULT_API_BASE_URL.to_owned());
    let (mut email, mut name) = (bundle.email.clone(), bundle.name.clone());
    if let Some(minted) = &bundle.minted {
        api_key = minted.api_key.clone();
        if !minted.base_url.trim().is_empty() {
            base_url = minted.base_url.trim().to_owned();
        }
        if !minted.user_email.is_empty() {
            email = minted.user_email.clone();
        }
        if !minted.user_full_name.is_empty() {
            name = minted.user_full_name.clone();
        }
    }
    let (access_token, expired) = if api_key.is_empty() {
        (bundle.token.access_token.clone(), dca_expired.clone())
    } else {
        (api_key.clone(), String::new())
    };
    TokenStorage {
        access_token,
        dca_token: bundle.token.access_token.clone(),
        api_key,
        token_type: bundle.token.token_type.clone(),
        expires_in: bundle.token.expires_in,
        expired,
        dca_expired,
        dca_expires_at: bundle.token.expires_at,
        last_refresh: rfc3339_utc(now),
        base_url,
        email,
        name,
    }
}

/// CredentialFileName: a readable account name plus an identity hash, so distinct emails
/// that sanitize identically never share a file.
pub fn credential_file_name(email: &str, sub: &str) -> String {
    let hash = |s: &str| hex8(&Sha256::digest(s.as_bytes()));
    let clean = email.trim();
    if !clean.is_empty() {
        let mut sanitized: String = clean
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        sanitized.truncate(120);
        return format!("meta-{sanitized}-{}.json", hash(clean));
    }
    let sub = sub.trim();
    if !sub.is_empty() {
        return format!("meta-{}.json", hash(sub));
    }
    "meta-oauth.json".into()
}

fn hex8(digest: &[u8]) -> String {
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// Fields the custom writer owns. They are never restored from disk or metadata.
const CREDENTIAL_FIELDS: [&str; 11] = [
    "type",
    "auth_kind",
    "access_token",
    "token_type",
    "dca_token",
    "api_key",
    "expires_in",
    "expired",
    "dca_expired",
    "dca_expires_at",
    "last_refresh",
];

/// A Go `map[string]any` as the custom writer sees it.
pub(crate) type GoMap = BTreeMap<String, GoValue>;

/// Go `json.Unmarshal` into `map[string]any`: numbers become float64, so they re-encode
/// in Go's float form (`1.0` -> `1`, `1e21` -> `1e+21`). A number that overflows float64
/// becomes `null` and marks the decode as failed (Go keeps the partial map and returns an
/// error). `None` when Go leaves the map nil: invalid JSON or a non-object.
pub(crate) fn go_map_from_json(raw: &[u8]) -> Option<(GoMap, bool)> {
    check_valid(raw).ok()?;
    match GoValue::parse(raw)? {
        GoValue::Object(map) => {
            // Go keeps decoding after an overflow and keeps the error even when a later
            // duplicate key overwrites the value, so every number in the document counts.
            let mut clean = all_numbers_finite(&gj::parse(raw));
            let map = map.into_iter().map(|(k, v)| (k, as_float64(v, &mut clean))).collect();
            Some((map, clean))
        }
        _ => None,
    }
}

fn all_numbers_finite(value: &gj::Res<'_>) -> bool {
    match value.kind {
        gj::Kind::Number => go_float(&String::from_utf8_lossy(&value.raw)).is_some(),
        gj::Kind::Json => {
            let mut ok = true;
            value.each(|_, item| {
                ok = all_numbers_finite(&item);
                ok
            });
            ok
        }
        _ => true,
    }
}

fn as_float64(value: GoValue, clean: &mut bool) -> GoValue {
    match value {
        GoValue::Number(literal) => match go_float(&literal) {
            Some(number) => GoValue::Number(number),
            None => {
                *clean = false;
                GoValue::Null
            }
        },
        GoValue::Array(items) => GoValue::Array(items.into_iter().map(|v| as_float64(v, clean)).collect()),
        GoValue::Object(map) => GoValue::Object(map.into_iter().map(|(k, v)| (k, as_float64(v, clean))).collect()),
        other => other,
    }
}

/// Go's encoding of a float64 decoded from `literal`, or `None` when the literal
/// overflows float64 (Go: UnmarshalTypeError).
fn go_float(literal: &str) -> Option<String> {
    gj::json_float(literal.parse::<f64>().ok()?)
}

/// SaveTokenToFile's content. With a `snapshot` (managed saves) its non-credential keys
/// override storage; without one, non-credential keys from `disk` fill only missing keys.
pub(crate) fn encode_token_file(storage: &TokenStorage, snapshot: Option<&GoMap>, disk: Option<&[u8]>) -> String {
    let s = |v: &str| GoValue::String(v.to_owned());
    let mut data = GoMap::new();
    data.insert("type".into(), s("meta"));
    data.insert("auth_kind".into(), s("oauth"));
    data.insert("access_token".into(), s(&storage.access_token));
    let optional = [
        ("dca_token", &storage.dca_token),
        ("api_key", &storage.api_key),
        ("token_type", &storage.token_type),
        ("expired", &storage.expired),
        ("dca_expired", &storage.dca_expired),
        ("last_refresh", &storage.last_refresh),
        ("base_url", &storage.base_url),
        ("email", &storage.email),
        ("name", &storage.name),
    ];
    for (key, value) in optional {
        if !value.is_empty() {
            data.insert(key.into(), s(value));
        }
    }
    if storage.expires_in > 0 {
        data.insert("expires_in".into(), GoValue::Number(storage.expires_in.to_string()));
    }
    if storage.dca_expires_at > 0 {
        data.insert(
            "dca_expires_at".into(),
            GoValue::Number(storage.dca_expires_at.to_string()),
        );
    }
    // Go ignores the decode error and uses whatever the map holds.
    let from_disk = if snapshot.is_none() {
        disk.and_then(go_map_from_json).map(|(map, _)| map)
    } else {
        None
    };
    if let Some(metadata) = snapshot.or(from_disk.as_ref()) {
        for (key, value) in metadata {
            if CREDENTIAL_FIELDS.contains(&key.as_str()) {
                continue;
            }
            if snapshot.is_some() || !data.contains_key(key) {
                data.insert(key.clone(), value.clone());
            }
        }
    }
    String::from_utf8_lossy(&GoValue::Object(data).encode_indented()).into_owned()
}

/// SaveTokenToFile: indented JSON plus newline, atomically replaced through a private
/// temporary file in the same directory.
pub(crate) fn save_token_file(path: &Path, storage: &TokenStorage, snapshot: Option<&GoMap>) -> std::io::Result<()> {
    let disk = if snapshot.is_none() {
        std::fs::read(path).ok()
    } else {
        None
    };
    write_private(path, encode_token_file(storage, snapshot, disk.as_deref()).as_bytes())
}

/// `IsAuthTokenPayloadKey`: token fields a re-login always replaces.
fn is_token_payload_key(key: &str) -> bool {
    matches!(
        key.trim().to_ascii_lowercase().as_str(),
        "access_token"
            | "refresh_token"
            | "id_token"
            | "session_id"
            | "expired"
            | "last_refresh"
            | "expires_in"
            | "timestamp"
            | "token_type"
            | "user_code"
            | "verification_uri"
            | "verification_uri_complete"
    )
}

/// `CanonicalCredentialMetadataKey`.
fn canonical_key(key: &str) -> &str {
    match key {
        "api-key" => "api_key",
        "base-url" => "base_url",
        "disable-cooling" => "disable_cooling",
        "excluded-models" => "excluded_models",
        "fingerprint-profile" => "fingerprint_profile",
        "model-aliases" => "model_aliases",
        "proxy-url" => "proxy_url",
        "request-retry" => "request_retry",
        "request-scoped-errors" => "request_scoped_errors",
        "tool-prefix-disabled" => "tool_prefix_disabled",
        other => other,
    }
}

/// `MergeExistingAuthMetadata` for a login that lands on an existing file: user settings
/// survive, token payload (and Meta's key material) does not. Returns the disabled flag
/// the file carried, unless the new metadata sets one.
pub(crate) fn merge_existing(metadata: &mut GoMap, existing: &GoMap, provider: &str) -> Option<bool> {
    let disabled = match (metadata.contains_key("disabled"), existing.get("disabled")) {
        (false, Some(GoValue::Bool(b))) => Some(*b),
        _ => None,
    };
    let meta = provider.trim().eq_ignore_ascii_case("meta");
    for (key, value) in existing {
        if is_token_payload_key(key) {
            continue;
        }
        if meta
            && matches!(
                canonical_key(key),
                "api_key" | "dca_token" | "dca_expired" | "dca_expires_at"
            )
        {
            continue;
        }
        metadata.entry(key.clone()).or_insert_with(|| value.clone());
    }
    disabled
}

/// `NormalizeCredentialMetadata`: alias keys become canonical; an explicit canonical key wins.
pub(crate) fn normalize_metadata(metadata: &mut GoMap) {
    let aliases: Vec<String> = metadata
        .keys()
        .filter(|k| canonical_key(k) != k.as_str())
        .cloned()
        .collect();
    for alias in aliases {
        let canonical = canonical_key(&alias).to_owned();
        if let Some(value) = metadata.remove(&alias) {
            metadata.entry(canonical).or_insert(value);
        }
    }
}

/// The login record sdk/auth/meta.go builds before the token store merges and saves it.
pub(crate) fn login_metadata(storage: &TokenStorage, bundle: &Bundle) -> GoMap {
    let s = |v: &str| GoValue::String(v.to_owned());
    let n = |v: i64| GoValue::Number(v.to_string());
    let mut m = GoMap::new();
    m.insert("type".into(), s("meta"));
    m.insert("access_token".into(), s(&storage.access_token));
    m.insert("token_type".into(), s(&storage.token_type));
    m.insert("expires_in".into(), n(storage.expires_in));
    m.insert("expired".into(), s(&storage.expired));
    m.insert("last_refresh".into(), s(&storage.last_refresh));
    m.insert("base_url".into(), s(&storage.base_url));
    m.insert("auth_kind".into(), s("oauth"));
    if !storage.dca_expired.is_empty() {
        m.insert("dca_expired".into(), s(&storage.dca_expired));
    }
    if storage.dca_expires_at > 0 {
        m.insert("dca_expires_at".into(), n(storage.dca_expires_at));
    }
    for (key, value) in [
        ("api_key", &storage.api_key),
        ("dca_token", &storage.dca_token),
        ("email", &storage.email),
        ("name", &storage.name),
    ] {
        if !value.is_empty() {
            m.insert(key.into(), s(value));
        }
    }
    if let Some(minted) = &bundle.minted {
        m.insert("subs_tier_name".into(), s(&minted.subs_tier_name));
        m.insert("subs_tier_id".into(), s(&minted.subs_tier_id));
        m.insert("is_subs_active".into(), GoValue::Bool(minted.is_subs_active));
        m.insert("has_payment_method".into(), GoValue::Bool(minted.has_payment_method));
    }
    m
}

/// A saved login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginOutcome {
    pub path: PathBuf,
    pub label: String,
}

/// Builds and writes the credential for `bundle` the way Manager.Login and the file
/// token store do: merge an existing file's settings, normalize keys, validate the weight,
/// stamp `disabled`, then the custom writer.
pub fn save_login(auth_dir: &Path, bundle: &Bundle, now: DateTime<Utc>) -> Result<LoginOutcome, String> {
    let storage = create_token_storage(bundle, now);
    if storage.access_token.trim().is_empty() {
        return Err("meta token storage missing access token".into());
    }
    let file_name = credential_file_name(&storage.email, &storage.dca_token);
    let label = match storage.email.trim() {
        "" => "Meta".to_owned(),
        email => email.to_owned(),
    };
    let mut metadata = login_metadata(&storage, bundle);
    let path = auth_dir.join(&file_name);
    let mut disabled = false;
    // Manager.Login merges only a cleanly decoded, non-empty existing file.
    if let Ok(raw) = std::fs::read(&path)
        && let Some((existing, true)) = go_map_from_json(&raw)
        && !existing.is_empty()
        && let Some(flag) = merge_existing(&mut metadata, &existing, "meta")
    {
        disabled = flag;
    }
    normalize_metadata(&mut metadata);
    if let Some(weight) = metadata.get("weight") {
        let json: Value = serde_json::from_slice(&weight.marshal()).unwrap_or(Value::Null);
        cpa_core::config::credentials::parse_weight(&json)
            .map_err(|e| format!("auth filestore: invalid metadata weight: {e}"))?;
    }
    metadata.insert("disabled".into(), GoValue::Bool(disabled));
    save_token_file(&path, &storage, Some(&metadata)).map_err(|e| format!("meta token storage: {e}"))?;
    Ok(LoginOutcome { path, label })
}

/// `--meta-login`: device flow through `requests.proxy-url`, then the credential file in
/// `auth-dir`.
pub async fn login(cfg: &cpa_core::config::Config, no_browser: bool) -> Result<PathBuf, ExecError> {
    let proxy = cfg
        .document
        .get("requests")
        .and_then(|r| r.get("proxy-url"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let client = crate::proxy::GoClients::new(crate::proxy::Hooks::default()).get(&crate::proxy::Proxy::parse(proxy));
    let outcome = login_with(MetaAuth::new(client), &cfg.auth_dir, no_browser)
        .await
        .map_err(|e| {
            ExecError::local(
                500,
                FailureScope::Credential,
                format!("Meta authentication failed: {e}"),
            )
        })?;
    println!("Authentication saved to {}", outcome.path.display());
    println!("Authenticated as {}", outcome.label);
    println!("Meta authentication successful!");
    Ok(outcome.path)
}

pub(crate) async fn login_with(auth: MetaAuth, auth_dir: &Path, no_browser: bool) -> Result<LoginOutcome, String> {
    println!("Starting Meta (Muse) authentication...");
    let code = auth
        .start_device_flow()
        .await
        .map_err(|e| format!("meta: failed to start device flow: {e}"))?;
    let url = match code.verification_uri_complete.trim() {
        "" => code.verification_uri.trim(),
        complete => complete,
    };
    print!("\nTo authenticate, please visit:\n{url}\n\n");
    if !code.user_code.is_empty() {
        print!("Then enter this code: {}\n\n", code.user_code);
    }
    if !no_browser {
        if open_browser(url) {
            println!("Browser opened automatically.");
        } else {
            tracing::warn!("No browser available; please open the URL manually");
        }
    }
    println!("Waiting for authorization...");
    if code.expires_in > 0 {
        println!("(This will timeout in {} seconds if not authorized)", code.expires_in);
    }
    let bundle = auth
        .wait_for_authorization(&code)
        .await
        .map_err(|e| format!("meta: {e}"))?;
    println!("Meta authentication successful");
    let auth_dir = auth_dir.to_owned();
    let now = auth.now();
    tokio::task::spawn_blocking(move || save_login(&auth_dir, &bundle, now))
        .await
        .map_err(|_| "meta: credential publication failed".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_float_matches_strconv() {
        // Expected strings from Go: json.Marshal(float64(x)).
        for (literal, go) in [
            ("1", "1"),
            ("1.0", "1"),
            ("0.10", "0.1"),
            ("1e3", "1000"),
            ("1e21", "1e+21"),
            ("1.5e-7", "1.5e-7"),
            ("1e-10", "1e-10"),
            ("0.000001", "0.000001"),
            ("-2.50", "-2.5"),
            ("9007199254740993", "9007199254740992"),
            ("123456789012345678901", "123456789012345680000"),
        ] {
            assert_eq!(go_float(literal).as_deref(), Some(go), "{literal}");
        }
        assert_eq!(go_float("1e400"), None, "float64 overflow is a decode error");
    }

    #[test]
    fn merge_skips_token_payload_and_meta_key_material() {
        let mut metadata = GoMap::new();
        metadata.insert("email".into(), GoValue::String("new@x".into()));
        let (existing, clean) = go_map_from_json(
            br#"{"email":"old@x","EXPIRED":"x","api-key":"k","dca_token":"d","prefix":"p","disabled":true}"#,
        )
        .unwrap();
        assert!(clean);
        assert_eq!(merge_existing(&mut metadata, &existing, "meta"), Some(true));
        let keys: Vec<&str> = metadata.keys().map(String::as_str).collect();
        assert_eq!(keys, ["disabled", "email", "prefix"]);
        assert_eq!(metadata["email"], GoValue::String("new@x".into()));
        // Other providers keep the key material.
        let mut other = GoMap::new();
        merge_existing(&mut other, &existing, "kimi");
        assert!(other.contains_key("api-key") && other.contains_key("dca_token"));
    }
}
