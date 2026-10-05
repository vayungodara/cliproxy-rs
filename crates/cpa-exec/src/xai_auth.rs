//! xAI RFC 8628 device login with OIDC discovery, token refresh and credential files
//! (internal/auth/xai, sdk/auth/xai.go, internal/runtime/executor/xai_executor_auth.go).
//!
//! Discovered endpoints must be HTTPS on `x.ai` or a subdomain, decided with Go's
//! `net/url` rules (Go `ValidateOAuthEndpoint`, see xai_url.rs). The discovered token
//! endpoint is saved with the credential and reused for refresh.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use base64::Engine;
use bytes::Bytes;
use chrono::{DateTime, SecondsFormat, Utc};
use cpa_common::gostr::{GoStr, quote};
use cpa_common::json::{self as gj, GoValue, Kind};
use cpa_core::config::Config;
use cpa_core::credential::{Credential, MetadataPatch};
use cpa_core::exec::{ExecError, FailureScope};
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use serde_json::{Value, json};

use crate::proxy::{GoClients, GoHeaders, Proxy};
use crate::xai_url;

pub const DEFAULT_API_BASE_URL: &str = "https://api.x.ai/v1";
pub const CLI_CHAT_PROXY_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";
pub const ISSUER: &str = "https://auth.x.ai";
pub const DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_POLL_DURATION: Duration = Duration::from_secs(30 * 60);
/// SDK refresh lead (`xaiauth.RefreshLead`).
pub const REFRESH_LEAD: Duration = Duration::from_secs(300);

/// A control-plane failure. Messages never carry tokens.
fn auth_error(status: u16, message: impl Into<String>) -> ExecError {
    ExecError::local(status, FailureScope::Credential, message)
}

/// Go `time.Time.UTC().Format(time.RFC3339)`.
fn rfc3339_utc(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// `ValidateOAuthEndpoint`: HTTPS on `x.ai` or a subdomain. Returns the trimmed input.
pub fn validate_endpoint(raw: &str, field: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(format!("xai discovery {field} is empty"));
    }
    let parsed = xai_url::parse(raw).map_err(|e| format!("xai discovery {field} is invalid: {e}"))?;
    if parsed.scheme != "https" {
        return Err(format!("xai discovery {field} must use https: {}", quote(raw)));
    }
    let host = parsed.hostname.trim().go_lower();
    if host != "x.ai" && !host.ends_with(".x.ai") {
        return Err(format!("xai discovery {field} host {} is not on x.ai", quote(&host)));
    }
    Ok(raw.to_owned())
}

/// Go `url.QueryEscape`.
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
fn form(pairs: &[(&str, &str)]) -> String {
    let mut pairs = pairs.to_vec();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", query_escape(k), query_escape(v)))
        .collect::<Vec<_>>()
        .join("&")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Str,
    Int,
}

/// The result of decoding a flat Go struct of string and int fields.
#[derive(Default)]
struct Decoded {
    strs: HashMap<&'static str, String>,
    ints: HashMap<&'static str, i64>,
}

impl Decoded {
    fn s(&self, key: &str) -> String {
        self.strs.get(key).cloned().unwrap_or_default()
    }

    fn i(&self, key: &str) -> i64 {
        self.ints.get(key).copied().unwrap_or_default()
    }
}

/// `json.Unmarshal` into a struct whose fields are strings and ints: keys match the
/// field name exactly or case-insensitively, the last occurrence wins, null leaves a
/// field unset, and the first type mismatch is the error (Go keeps decoding, then
/// returns it). `go_type` is the struct as Go prints it; `field_prefix` its name in
/// field errors (empty for anonymous structs).
// ponytail: syntax errors report a fixed text instead of Go's scanner message (for
// example "invalid character 'o' in literal null"); only CLI login output and refresh
// error messages differ.
fn go_unmarshal(
    body: &[u8],
    fields: &[(&'static str, Field)],
    go_type: &str,
    field_prefix: &str,
) -> Result<Decoded, String> {
    if !gj::valid(body) {
        return Err("invalid JSON input".into());
    }
    let root = gj::parse(body);
    let kind_name = |r: &gj::Res<'_>| match r.kind {
        Kind::String => "string",
        Kind::Number => "number",
        Kind::True | Kind::False => "bool",
        Kind::Json if r.is_array() => "array",
        Kind::Json => "object",
        Kind::Null => "null",
    };
    match root.kind {
        Kind::Null => return Ok(Decoded::default()),
        Kind::Json if root.is_object() => {}
        _ => {
            return Err(format!(
                "json: cannot unmarshal {} into Go value of type {go_type}",
                kind_name(&root)
            ));
        }
    }
    let mut out = Decoded::default();
    let mut first_error: Option<String> = None;
    root.each(|key, value| {
        let key = key.str();
        let field = fields
            .iter()
            .find(|(name, _)| *name == key)
            .or_else(|| fields.iter().find(|(name, _)| name.go_eq_fold(&key)));
        let Some(&(name, kind)) = field else { return true };
        let mismatch = |what: &str| {
            let ty = if kind == Field::Str { "string" } else { "int" };
            format!("json: cannot unmarshal {what} into Go struct field {field_prefix}.{name} of type {ty}")
        };
        match (kind, value.kind) {
            (_, Kind::Null) => {}
            (Field::Str, Kind::String) => {
                out.strs.insert(name, gj::go_unquote(value.raw()).unwrap_or_default());
            }
            (Field::Int, Kind::Number) => match std::str::from_utf8(value.raw())
                .ok()
                .and_then(|n| n.parse::<i64>().ok())
            {
                Some(n) => {
                    out.ints.insert(name, n);
                }
                // ponytail: Go quotes the number ("number 1.5 into ..."); the value is
                // response data that reaches logs and returned errors, so the message
                // names only its kind.
                None => {
                    first_error.get_or_insert_with(|| mismatch("number"));
                }
            },
            _ => {
                first_error.get_or_insert_with(|| mismatch(kind_name(&value)));
            }
        }
        true
    });
    match first_error {
        Some(error) => Err(error),
        None => Ok(out),
    }
}

const DISCOVERY_TYPE: &str = r#"struct { DeviceAuthorizationEndpoint string "json:\"device_authorization_endpoint\""; TokenEndpoint string "json:\"token_endpoint\"" }"#;
const TOKEN_TYPE: &str = r#"struct { Error string "json:\"error\""; ErrorDescription string "json:\"error_description\""; AccessToken string "json:\"access_token\""; RefreshToken string "json:\"refresh_token\""; IDToken string "json:\"id_token\""; TokenType string "json:\"token_type\""; ExpiresIn int "json:\"expires_in\"" }"#;
const REFRESH_TYPE: &str = r#"struct { AccessToken string "json:\"access_token\""; RefreshToken string "json:\"refresh_token\""; IDToken string "json:\"id_token\""; TokenType string "json:\"token_type\""; ExpiresIn int "json:\"expires_in\"" }"#;
const TOKEN_FIELDS: [(&str, Field); 7] = [
    ("error", Field::Str),
    ("error_description", Field::Str),
    ("access_token", Field::Str),
    ("refresh_token", Field::Str),
    ("id_token", Field::Str),
    ("token_type", Field::Str),
    ("expires_in", Field::Int),
];

/// Discovered OAuth endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    pub device_authorization_endpoint: String,
    pub token_endpoint: String,
}

/// RFC 8628 device authorization response, plus the token endpoint to poll.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub expires_in: i64,
    pub interval: i64,
    pub token_endpoint: String,
}

/// Go `TokenData`. `expire` is RFC 3339 UTC, empty when the server sent no lifetime.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenData {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: String,
    pub token_type: String,
    pub expires_in: i64,
    pub expire: String,
    pub email: String,
    pub subject: String,
}

/// `parseJWTIdentity`: email and subject claims from an ID token, unverified.
pub fn parse_jwt_identity(token: &str) -> (String, String) {
    use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
    // base64.URLEncoding: padded, discarded bits may be non-zero, CR and LF are skipped.
    const GO_URL_ENCODING: GeneralPurpose = GeneralPurpose::new(
        &base64::alphabet::URL_SAFE,
        GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(DecodePaddingMode::RequireCanonical),
    );
    let mut parts = token.split('.');
    let (Some(_), Some(payload)) = (parts.next(), parts.next()) else {
        return Default::default();
    };
    let padded = format!("{payload}{}", "=".repeat((4 - payload.len() % 4) % 4));
    let padded: String = padded.chars().filter(|c| !matches!(c, '\r' | '\n')).collect();
    let Ok(raw) = GO_URL_ENCODING.decode(padded) else {
        return Default::default();
    };
    let Some(GoValue::Object(claims)) = GoValue::parse_f64(&raw) else {
        return Default::default();
    };
    let claim = |k: &str| match claims.get(k) {
        Some(GoValue::String(s)) => s.trim().to_owned(),
        _ => String::new(),
    };
    (claim("email"), claim("sub"))
}

/// `time.Duration(seconds) * time.Second` in nanoseconds, wrapping like Go's int64.
fn go_seconds(seconds: i64) -> i64 {
    seconds.wrapping_mul(1_000_000_000)
}

/// `buildTokenData`. Identity comes from the untrimmed ID token, as in Go.
fn token_data(decoded: &Decoded, now: DateTime<Utc>) -> TokenData {
    let (email, subject) = parse_jwt_identity(&decoded.s("id_token"));
    let expires_in = decoded.i("expires_in");
    let expire = (expires_in > 0).then(|| {
        let at = now
            .checked_add_signed(chrono::Duration::nanoseconds(go_seconds(expires_in)))
            .unwrap_or(now);
        rfc3339_utc(at)
    });
    TokenData {
        access_token: decoded.s("access_token").trim().to_owned(),
        refresh_token: decoded.s("refresh_token").trim().to_owned(),
        id_token: decoded.s("id_token").trim().to_owned(),
        token_type: decoded.s("token_type").trim().to_owned(),
        expires_in,
        expire: expire.unwrap_or_default(),
        email,
        subject,
    }
}

/// The poll interval Go starts with (`PollForToken`).
fn initial_interval(device_interval: i64, min_poll: Option<Duration>) -> Duration {
    let min = min_poll.unwrap_or(DEFAULT_POLL_INTERVAL);
    let interval = go_seconds(device_interval);
    if (min_poll.is_some() && device_interval <= 0) || interval < min.as_nanos() as i64 {
        min
    } else {
        Duration::from_nanos(interval as u64)
    }
}

/// When polling stops, in nanoseconds after the first poll: 30 minutes, or the device
/// code lifetime when that is earlier (Go's wrapped duration can lie in the past).
fn poll_deadline(expires_in: i64) -> i64 {
    let max = MAX_POLL_DURATION.as_nanos() as i64;
    if expires_in > 0 {
        go_seconds(expires_in).min(max)
    } else {
        max
    }
}

/// One xAI OAuth client.
#[derive(Clone)]
pub struct XaiAuth {
    client: wreq::Client,
    discovery_url: String,
    min_poll_interval: Option<Duration>,
    /// Test seam: requests to `https://auth.x.ai` go to this origin instead, after
    /// validation (the Go goldens use the same rewrite in their transport).
    issuer_origin: Option<String>,
}

impl XaiAuth {
    pub fn new(client: wreq::Client) -> Self {
        Self {
            client,
            discovery_url: DISCOVERY_URL.into(),
            min_poll_interval: None,
            issuer_origin: None,
        }
    }

    pub fn with_issuer_origin(mut self, origin: &str) -> Self {
        self.issuer_origin = Some(origin.into());
        self
    }

    /// Where a request for `url` goes. Fails closed when the HTTP client would contact a
    /// different host than Go's `net/url` reads from the same string.
    // ponytail: the request target is the WHATWG form of the URL, so dot segments and
    // backslashes in an accepted path are normalized where Go sends them as given.
    fn target(&self, url: &str) -> Result<String, ()> {
        if !crate::xai_url::same_authority(url) {
            return Err(());
        }
        Ok(match (&self.issuer_origin, url.strip_prefix(ISSUER)) {
            (Some(origin), Some(rest)) => format!("{origin}{rest}"),
            _ => url.to_owned(),
        })
    }

    /// Go's `minPollInterval` test knob.
    pub fn with_min_poll_interval(mut self, interval: Duration) -> Self {
        self.min_poll_interval = Some(interval);
        self
    }

    /// `http.Client.Do` of a form POST: status and the whole body.
    async fn post_form(&self, url: &str, body: String) -> Result<(u16, Bytes), ()> {
        let mut headers = GoHeaders::new();
        headers.set("User-Agent", "Go-http-client/1.1");
        headers.set("Content-Type", "application/x-www-form-urlencoded");
        headers.set("Accept", "application/json");
        let upstream = crate::proxy::send(&self.client, &self.target(url)?, headers, body, Some(HTTP_TIMEOUT))
            .await
            .map_err(|_| ())?;
        let status = upstream.status;
        let body = crate::proxy::read_all(upstream.body, crate::proxy::MAX_ERROR_BODY, false)
            .await
            .map_err(|_| ())?;
        Ok((status, body))
    }

    /// `http.Client.Do` of the discovery GET, following redirects as Go does.
    async fn get(&self, url: &str) -> Result<(u16, Bytes), ()> {
        let mut headers = GoHeaders::new();
        headers.set("User-Agent", "Go-http-client/1.1");
        headers.set("Accept", "application/json");
        let upstream = crate::proxy::request(
            &self.client,
            wreq::Method::GET,
            &self.target(url)?,
            headers,
            None,
            Some(HTTP_TIMEOUT),
        )
        .await
        .map_err(|_| ())?;
        let status = upstream.status;
        let body = crate::proxy::read_all(upstream.body, crate::proxy::MAX_ERROR_BODY, false)
            .await
            .map_err(|_| ())?;
        Ok((status, body))
    }

    /// `Discover`.
    pub async fn discover(&self) -> Result<Discovery, ExecError> {
        let fail = |m: String| auth_error(502, m);
        let (status, body) = self
            .get(&self.discovery_url)
            .await
            .map_err(|_| fail("xai discovery: request failed".into()))?;
        if status != 200 {
            return Err(fail(format!(
                "xai discovery failed with status {status}: {}",
                String::from_utf8_lossy(&body).trim()
            )));
        }
        let wire = go_unmarshal(
            &body,
            &[
                ("device_authorization_endpoint", Field::Str),
                ("token_endpoint", Field::Str),
            ],
            DISCOVERY_TYPE,
            "",
        )
        .map_err(|e| fail(format!("xai discovery: parse response: {e}")))?;
        let device = validate_endpoint(
            &wire.s("device_authorization_endpoint"),
            "device_authorization_endpoint",
        )
        .map_err(fail)?;
        let token = validate_endpoint(&wire.s("token_endpoint"), "token_endpoint").map_err(fail)?;
        Ok(Discovery {
            device_authorization_endpoint: device,
            token_endpoint: token,
        })
    }

    /// `StartDeviceFlow`.
    pub async fn start_device_flow(&self) -> Result<DeviceCode, ExecError> {
        let discovery = self.discover().await?;
        self.request_device_code(&discovery.device_authorization_endpoint, &discovery.token_endpoint)
            .await
    }

    /// `RequestDeviceCode`.
    pub async fn request_device_code(
        &self,
        device_endpoint: &str,
        token_endpoint: &str,
    ) -> Result<DeviceCode, ExecError> {
        let fail = |m: String| auth_error(502, m);
        let device_endpoint = device_endpoint.trim();
        if device_endpoint.is_empty() {
            return Err(fail(
                "xai device code: device authorization endpoint is required".into(),
            ));
        }
        let (status, body) = self
            .post_form(device_endpoint, form(&[("client_id", CLIENT_ID), ("scope", SCOPE)]))
            .await
            .map_err(|_| fail("xai device code request failed".into()))?;
        if status != 200 {
            return Err(fail(format!(
                "xai device code request failed with status {status}: {}",
                String::from_utf8_lossy(&body).trim()
            )));
        }
        let wire = go_unmarshal(
            &body,
            &[
                ("device_code", Field::Str),
                ("user_code", Field::Str),
                ("verification_uri", Field::Str),
                ("verification_uri_complete", Field::Str),
                ("expires_in", Field::Int),
                ("interval", Field::Int),
            ],
            "xai.DeviceCodeResponse",
            "DeviceCodeResponse",
        )
        .map_err(|e| fail(format!("xai device code: parse response: {e}")))?;
        let code = DeviceCode {
            device_code: wire.s("device_code"),
            user_code: wire.s("user_code"),
            verification_uri: wire.s("verification_uri"),
            verification_uri_complete: wire.s("verification_uri_complete"),
            expires_in: wire.i("expires_in"),
            interval: wire.i("interval"),
            token_endpoint: token_endpoint.trim().to_owned(),
        };
        if code.device_code.trim().is_empty() {
            return Err(fail("xai device code: response missing device_code".into()));
        }
        if code.user_code.trim().is_empty() {
            return Err(fail("xai device code: response missing user_code".into()));
        }
        if code.verification_uri.trim().is_empty() && code.verification_uri_complete.trim().is_empty() {
            return Err(fail("xai device code: response missing verification URI".into()));
        }
        Ok(code)
    }

    /// One `exchangeDeviceCode`: `Ok(Some)` tokens, `Ok(None)` keep polling after
    /// `interval`, `Err` stop.
    async fn exchange(
        &self,
        token_endpoint: &str,
        device_code: &str,
        interval: &mut Duration,
    ) -> Result<Option<TokenData>, ExecError> {
        let fail = |m: String| auth_error(400, m);
        let body = form(&[
            ("client_id", CLIENT_ID),
            ("device_code", device_code.trim()),
            ("grant_type", DEVICE_GRANT),
        ]);
        let (status, body) = self
            .post_form(token_endpoint.trim(), body)
            .await
            .map_err(|_| auth_error(502, "xai device token request failed"))?;
        let wire = go_unmarshal(&body, &TOKEN_FIELDS, TOKEN_TYPE, "")
            .map_err(|e| auth_error(502, format!("xai device token: parse response: {e}")))?;
        match wire.s("error").as_str() {
            "" => {}
            "authorization_pending" => return Ok(None),
            "slow_down" => {
                *interval = interval.saturating_add(self.min_poll_interval.unwrap_or(DEFAULT_POLL_INTERVAL));
                return Ok(None);
            }
            "expired_token" => return Err(fail("xai device code expired".into())),
            "access_denied" => return Err(auth_error(403, "xai device authorization denied")),
            other => {
                let description = wire.s("error_description");
                let description = description.trim();
                return Err(fail(if description.is_empty() {
                    format!("xai device token error: {other}")
                } else {
                    format!("xai device token error: {other}: {description}")
                }));
            }
        }
        if status != 200 {
            return Err(auth_error(
                502,
                format!(
                    "xai device token request failed with status {status}: {}",
                    String::from_utf8_lossy(&body).trim()
                ),
            ));
        }
        if wire.s("access_token").trim().is_empty() {
            return Err(auth_error(502, "xai device token response missing access_token"));
        }
        Ok(Some(token_data(&wire, Utc::now())))
    }

    /// `PollForToken`: the first poll is immediate, then at the device interval (at least
    /// 5s), slower on `slow_down`, until the code or 30 minutes expire.
    pub async fn poll(&self, code: &DeviceCode) -> Result<TokenData, ExecError> {
        let mut token_endpoint = code.token_endpoint.trim().to_owned();
        if token_endpoint.is_empty() {
            token_endpoint = self.discover().await?.token_endpoint;
        }
        let mut interval = initial_interval(code.interval, self.min_poll_interval);
        let start = tokio::time::Instant::now();
        let deadline = poll_deadline(code.expires_in);
        let mut first = true;
        loop {
            if !first {
                tokio::time::sleep(interval).await;
                if start.elapsed().as_nanos() as i64 > deadline {
                    return Err(auth_error(400, "xai device code expired"));
                }
            }
            first = false;
            if let Some(tokens) = self.exchange(&token_endpoint, &code.device_code, &mut interval).await? {
                return Ok(tokens);
            }
        }
    }

    /// `RefreshTokens`, single-flighted per refresh token across the process
    /// (xaiRefreshGroup). Callers that go away do not cancel it.
    pub async fn refresh(&self, refresh_token: &str, token_endpoint: &str) -> Result<TokenData, ExecError> {
        if refresh_token.trim().is_empty() {
            return Err(auth_error(400, "xai token refresh: refresh token is required"));
        }
        let refresh_token = refresh_token.trim().to_owned();
        let mut token_endpoint = token_endpoint.trim().to_owned();
        if token_endpoint.is_empty() {
            token_endpoint = self.discover().await?.token_endpoint;
        }
        type Flight = Shared<BoxFuture<'static, Result<TokenData, ExecError>>>;
        static FLIGHTS: LazyLock<Mutex<HashMap<String, Flight>>> = LazyLock::new(Mutex::default);
        let flight = {
            let mut flights = FLIGHTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            match flights.get(&refresh_token) {
                Some(flight) => flight.clone(),
                None => {
                    let auth = self.clone();
                    let key = refresh_token.clone();
                    let task = tokio::spawn(async move {
                        let result = auth.refresh_once(&key, &token_endpoint).await;
                        FLIGHTS
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(&key);
                        result
                    });
                    let flight = async move {
                        task.await
                            .map_err(|_| auth_error(502, "xai token refresh task failed"))?
                    }
                    .boxed()
                    .shared();
                    flights.insert(refresh_token, flight.clone());
                    flight
                }
            }
        };
        flight.await
    }

    /// `refreshTokensSingleFlight` + `postTokenForm`.
    async fn refresh_once(&self, refresh_token: &str, token_endpoint: &str) -> Result<TokenData, ExecError> {
        let body = form(&[
            ("client_id", CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ]);
        let (status, body) = self
            .post_form(token_endpoint, body)
            .await
            .map_err(|_| auth_error(502, "xai token request failed"))?;
        if status != 200 {
            // ponytail: Go appends the response body; it is withheld because token
            // endpoints may echo credentials and the message reaches management clients
            // (same choice as the Kimi refresh).
            return Err(auth_error(
                status,
                format!("xai token request failed with status {status}"),
            ));
        }
        let wire = go_unmarshal(&body, &TOKEN_FIELDS[2..], REFRESH_TYPE, "")
            .map_err(|e| auth_error(502, format!("xai token response: parse body: {e}")))?;
        if wire.s("access_token").trim().is_empty() {
            return Err(auth_error(502, "xai token response missing access_token"));
        }
        Ok(token_data(&wire, Utc::now()))
    }
}

/// `sanitizeFileSegment`.
fn sanitize_segment(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}

/// `CredentialFileName`.
pub fn credential_file_name(email: &str, subject: &str, now_ms: i64) -> String {
    let email = sanitize_segment(email);
    if !email.is_empty() {
        return format!("xai-{email}.json");
    }
    let subject = sanitize_segment(subject);
    if !subject.is_empty() {
        return format!("xai-{subject}.json");
    }
    format!("xai-{now_ms}.json")
}

/// A finished login: file name, the saved document and label.
pub struct LoginRecord {
    pub file_name: String,
    pub metadata: BTreeMap<String, GoValue>,
    pub label: String,
}

fn go_str(s: &str) -> GoValue {
    GoValue::String(s.to_owned())
}

/// The credential Go's login saves before any existing file is merged: `TokenStorage`
/// fields (omitempty) overlaid by the login metadata (sdk/auth/xai.go).
pub fn login_record(tokens: &TokenData, last_refresh: &str, token_endpoint: &str, now_ms: i64) -> LoginRecord {
    let email = tokens.email.trim();
    let mut out = BTreeMap::new();
    // TokenStorage with omitempty.
    for (k, v) in [
        ("id_token", tokens.id_token.as_str()),
        ("token_type", &tokens.token_type),
        ("expired", &tokens.expire),
        ("last_refresh", last_refresh),
        ("email", email),
        ("sub", &tokens.subject),
        ("base_url", DEFAULT_API_BASE_URL),
        ("token_endpoint", token_endpoint),
    ] {
        if !v.is_empty() {
            out.insert(k.to_owned(), go_str(v));
        }
    }
    if tokens.expires_in != 0 {
        out.insert("expires_in".into(), GoValue::Number(tokens.expires_in.to_string()));
    }
    // Login metadata, always present.
    for (k, v) in [
        ("type", "xai"),
        ("access_token", tokens.access_token.as_str()),
        ("refresh_token", &tokens.refresh_token),
        ("id_token", &tokens.id_token),
        ("token_type", &tokens.token_type),
        ("expired", &tokens.expire),
        ("last_refresh", last_refresh),
        ("base_url", DEFAULT_API_BASE_URL),
        ("token_endpoint", token_endpoint),
        ("auth_kind", "oauth"),
    ] {
        out.insert(k.to_owned(), go_str(v));
    }
    out.insert("expires_in".into(), GoValue::Number(tokens.expires_in.to_string()));
    if !email.is_empty() {
        out.insert("email".into(), go_str(email));
    }
    if !tokens.subject.is_empty() {
        out.insert("sub".into(), go_str(&tokens.subject));
    }
    LoginRecord {
        file_name: credential_file_name(email, &tokens.subject, now_ms),
        metadata: out,
        label: if email.is_empty() { "xAI".into() } else { email.into() },
    }
}

/// `IsAuthTokenPayloadKey`.
fn is_token_key(key: &str) -> bool {
    matches!(
        key.trim().go_lower().as_str(),
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

/// The file `Manager.Login` + `FileTokenStore.Save` write: non-token fields of an
/// existing file are kept (`MergeExistingAuthMetadata`), legacy keys are renamed
/// (`NormalizeCredentialMetadata`), `disabled` is the existing boolean or false, and a
/// merged `weight` must be valid (`ValidateAuthWeight`) or nothing is written.
pub fn saved_document(
    mut record: BTreeMap<String, GoValue>,
    existing: Option<&[u8]>,
) -> Result<BTreeMap<String, GoValue>, String> {
    let existing = existing.and_then(GoValue::parse_f64).and_then(|v| match v {
        GoValue::Object(map) if !map.is_empty() => Some(map),
        _ => None,
    });
    let mut disabled = false;
    if let Some(existing) = existing {
        if let Some(GoValue::Bool(b)) = existing.get("disabled") {
            disabled = *b;
        }
        for (k, v) in existing {
            if !is_token_key(&k) {
                record.entry(k).or_insert(v);
            }
        }
    }
    let legacy: Vec<String> = record
        .keys()
        .filter(|k| canonical_key(k) != k.as_str())
        .cloned()
        .collect();
    for key in legacy {
        let value = record.remove(&key).expect("present");
        record.entry(canonical_key(&key).to_owned()).or_insert(value);
    }
    if let Some(weight) = record.get("weight") {
        let json = match weight {
            GoValue::Number(n) => serde_json::from_str(n).unwrap_or(Value::Null),
            GoValue::String(s) => Value::String(s.clone()),
            _ => Value::Null,
        };
        cpa_core::config::credentials::parse_weight(&json)
            .map_err(|e| format!("auth filestore: invalid metadata weight: {e}"))?;
    }
    record.insert("disabled".into(), GoValue::Bool(disabled));
    Ok(record)
}

/// `xaiMetadataString`: a metadata value as `fmt.Sprint` prints it (JSON numbers are
/// float64 in Go's decoded auth files), trimmed; missing and null are empty.
pub(crate) fn metadata_string(credential: &Credential, key: &str) -> String {
    match credential.metadata.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(value) => go_sprint(value).trim().to_owned(),
    }
}

/// `fmt.Sprint` of a value decoded by `encoding/json` into `any`.
fn go_sprint(value: &Value) -> String {
    match value {
        Value::Null => "<nil>".into(),
        Value::Bool(b) => b.to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => go_float_v(n.as_f64().unwrap_or(f64::NAN)),
        Value::Array(items) => format!("[{}]", items.iter().map(go_sprint).collect::<Vec<_>>().join(" ")),
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let items: Vec<String> = keys.iter().map(|k| format!("{k}:{}", go_sprint(&map[*k]))).collect();
            format!("map[{}]", items.join(" "))
        }
    }
}

/// `%v` of a float64: `strconv.FormatFloat(f, 'g', -1, 64)`, the shortest digits in
/// exponent form when the decimal exponent is below -4 or at least 6.
fn go_float_v(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "+Inf".into() } else { "-Inf".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').expect("exponent form");
    let exp: i32 = exp.parse().expect("exponent");
    if !(-4..6).contains(&exp) {
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exp.abs());
    }
    format!("{f}")
}

/// Metadata change for a successful refresh (`XAIExecutor.Refresh`).
pub fn refresh_patch(
    credential: &Credential,
    tokens: &TokenData,
    token_endpoint: &str,
    now: DateTime<Utc>,
) -> MetadataPatch {
    let mut patch = MetadataPatch::default();
    let mut set = |k: &str, v: Value| {
        patch.set.insert(k.into(), v);
    };
    set("type", json!("xai"));
    set("auth_kind", json!("oauth"));
    set("access_token", json!(tokens.access_token));
    for (key, value) in [
        ("refresh_token", &tokens.refresh_token),
        ("id_token", &tokens.id_token),
        ("token_type", &tokens.token_type),
        ("expired", &tokens.expire),
        ("email", &tokens.email),
        ("sub", &tokens.subject),
    ] {
        if !value.is_empty() {
            set(key, json!(value));
        }
    }
    if tokens.expires_in > 0 {
        set("expires_in", json!(tokens.expires_in));
    }
    if !token_endpoint.is_empty() {
        set("token_endpoint", json!(token_endpoint));
    }
    if metadata_string(credential, "base_url").is_empty() {
        set("base_url", json!(DEFAULT_API_BASE_URL));
    }
    set("last_refresh", json!(rfc3339_utc(now)));
    patch
}

/// Whether the background loop should refresh: a refresh token and expiry within the
/// SDK lead (`RefreshSoon`; requests keep the current token).
pub fn needs_refresh(credential: &Credential, now: DateTime<Utc>) -> bool {
    !metadata_string(credential, "refresh_token").is_empty()
        && crate::kimi_http::refresh_due(
            credential,
            Some(chrono::Duration::from_std(REFRESH_LEAD).expect("lead fits")),
            now,
        )
}

/// `XAIExecutor.Refresh`: nothing to do without a refresh token.
pub async fn refresh(auth: &XaiAuth, credential: &Credential) -> Result<MetadataPatch, ExecError> {
    let refresh_token = metadata_string(credential, "refresh_token");
    if refresh_token.is_empty() {
        return Ok(MetadataPatch::default());
    }
    let token_endpoint = metadata_string(credential, "token_endpoint");
    let tokens = auth.refresh(&refresh_token, &token_endpoint).await?;
    Ok(refresh_patch(credential, &tokens, &token_endpoint, Utc::now()))
}

/// `--xai-login`: device flow through `requests.proxy-url`, then the credential file.
pub async fn login(cfg: &Config, no_browser: bool) -> Result<PathBuf, ExecError> {
    let proxy = cfg
        .document
        .get("requests")
        .and_then(|r| r.get("proxy-url"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let client = GoClients::new(Default::default()).get(&Proxy::parse(proxy));
    login_with(XaiAuth::new(client), &cfg.auth_dir, no_browser).await
}

pub(crate) async fn login_with(auth: XaiAuth, auth_dir: &Path, no_browser: bool) -> Result<PathBuf, ExecError> {
    println!("Starting xAI authentication...");
    let code = auth.start_device_flow().await.map_err(|e| {
        auth_error(
            e.status,
            format!("xai: failed to start device flow: {}", String::from_utf8_lossy(&e.body)),
        )
    })?;
    let url = if code.verification_uri_complete.trim().is_empty() {
        code.verification_uri.trim()
    } else {
        code.verification_uri_complete.trim()
    };
    print!("\nTo authenticate, please visit:\n{url}\n\n");
    if !code.user_code.is_empty() {
        print!("Then enter this code: {}\n\n", code.user_code);
    }
    if !no_browser && crate::kimi_auth::open_browser(url) {
        println!("Browser opened automatically.");
    }
    println!("Waiting for authorization...");
    if code.expires_in > 0 {
        println!("(This will timeout in {} seconds if not authorized)", code.expires_in);
    }
    let tokens = auth
        .poll(&code)
        .await
        .map_err(|e| auth_error(e.status, format!("xai: {}", String::from_utf8_lossy(&e.body))))?;
    if tokens.access_token.trim().is_empty() {
        return Err(auth_error(502, "xai token storage missing access token"));
    }
    println!("xAI authentication successful");
    let record = login_record(
        &tokens,
        &rfc3339_utc(Utc::now()),
        &code.token_endpoint,
        Utc::now().timestamp_millis(),
    );
    let path = auth_dir.join(&record.file_name);
    let target = path.clone();
    let metadata = record.metadata;
    tokio::task::spawn_blocking(move || {
        let existing = std::fs::read(&target).ok().filter(|raw| !raw.is_empty());
        let document = GoValue::Object(saved_document(metadata, existing.as_deref()).map_err(|e| auth_error(500, e))?);
        crate::kimi_auth::write_private(&target, &document.encode_indented())
            .map_err(|_| auth_error(500, "xai: cannot write credential file"))
    })
    .await
    .map_err(|_| auth_error(500, "xai: credential publication failed"))??;
    println!("Authentication saved to {}", path.display());
    println!("Authenticated as {}", record.label);
    println!("xAI authentication successful!");
    Ok(path)
}

#[cfg(test)]
#[path = "xai_auth_tests.rs"]
mod tests;
