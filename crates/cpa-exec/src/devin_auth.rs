//! Devin login and account status (internal/auth/devin, sdk/auth/devin.go,
//! internal/cmd/devin_login.go).
//!
//! Devin uses a browser PKCE flow on app.devin.ai that yields a permanent session token
//! (`devin-session-token$<jwt>`); there is no refresh token. Account details, plan and
//! quota come from the Connect-RPC `GetUserStatus` call on the Codeium server, used at
//! login and by the executor's refresh.
//!
//! The management login (Go `RequestDevinToken`) builds on [`DevinAuth`]: it uses
//! [`DevinAuth::build_authorization_url`] with `http://127.0.0.1:<port>/callback`,
//! receives the code on the main server's `/callback` route, then calls
//! [`DevinAuth::exchange_code`], [`DevinAuth::create_auth_record`] and [`save_record`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use chrono::{DateTime, TimeZone, Utc};
use cpa_common::json::{self as gj, GoValue};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::devin_wire::{device_fingerprint, pb};
use crate::meta_auth::{GoMap, go_map_from_json, merge_existing, normalize_metadata};
use crate::proxy::{GoHeaders, read_all, request};

/// `DefaultAppBaseURL`.
pub const DEFAULT_APP_BASE_URL: &str = "https://app.devin.ai";
/// `DefaultAPIBaseURL`.
pub const DEFAULT_API_BASE_URL: &str = "https://api.devin.ai";
/// `DefaultServerURL`.
pub const DEFAULT_SERVER_URL: &str = "https://server.codeium.com";
const TOKEN_PREFIX: &str = "devin-session-token$";
/// `DevinGetUserStatusPath`.
pub const USER_STATUS_PATH: &str = "/exa.seat_management_pb.SeatManagementService/GetUserStatus";
const BODY_LIMIT: usize = 1 << 20;
const STATUS_LIMIT: usize = 4 << 20;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(30);

/// `FormatSessionToken`: a bare JWT gets the `devin-session-token$` prefix.
pub fn format_session_token(raw: &str) -> String {
    let t = raw.trim();
    if !t.starts_with(TOKEN_PREFIX) && t.starts_with("eyJ") {
        return format!("{TOKEN_PREFIX}{t}");
    }
    t.to_owned()
}

/// `PKCECodes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// `GeneratePKCECodes`: a 64-byte random verifier and its S256 challenge.
pub fn generate_pkce() -> Result<Pkce, String> {
    let mut bytes = [0u8; 64];
    getrandom::fill(&mut bytes).map_err(|e| format!("failed to generate random bytes: {e}"))?;
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Ok(Pkce { verifier, challenge })
}

/// `misc.GenerateRandomState`: 16 random bytes, hex.
pub fn random_state() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| format!("failed to generate random bytes: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// `DevinUserStatus`: plan, quota and account metadata from `GetUserStatus`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserStatus {
    pub email: String,
    pub user_name: String,
    pub user_id: String,
    pub team_id: String,
    pub org_id: String,
    pub org_name: String,
    pub plan: String,
    pub daily_quota_remaining_percent: i64,
    pub weekly_quota_remaining_percent: i64,
    pub daily_quota_reset_at: Option<DateTime<Utc>>,
    pub weekly_quota_reset_at: Option<DateTime<Utc>>,
    pub plan_start: Option<DateTime<Utc>>,
    pub plan_end: Option<DateTime<Utc>>,
}

impl UserStatus {
    /// The quota observation signals Go writes on refresh and login.
    pub fn quota_signals(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if !self.plan.is_empty() {
            out.push(("plan", self.plan.clone()));
        }
        out.push((
            "daily_quota_remaining_percent",
            format!("{}%", self.daily_quota_remaining_percent),
        ));
        out.push((
            "weekly_quota_remaining_percent",
            format!("{}%", self.weekly_quota_remaining_percent),
        ));
        for (key, at) in [
            ("daily_quota_reset_at", self.daily_quota_reset_at),
            ("weekly_quota_reset_at", self.weekly_quota_reset_at),
            ("plan_start", self.plan_start),
            ("plan_end", self.plan_end),
        ] {
            if let Some(at) = at {
                out.push((key, rfc3339(at)));
            }
        }
        out
    }
}

/// Go `time.Time.Format(time.RFC3339)` for a UTC time.
fn rfc3339(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn unix(secs: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(secs, 0).single()
}

/// `BuildGetUserStatusRequest`.
pub(crate) fn user_status_request(session_token: &str, fingerprint: &str) -> Vec<u8> {
    let fingerprint = if fingerprint.is_empty() {
        device_fingerprint(session_token)
    } else {
        fingerprint.to_owned()
    };
    let mut meta = Vec::with_capacity(1024);
    pb::bytes(&mut meta, 1, b"chisel");
    pb::bytes(&mut meta, 2, b"3000.10.21");
    pb::bytes(&mut meta, 3, session_token.as_bytes());
    pb::bytes(&mut meta, 4, b"en");
    pb::bytes(&mut meta, 5, crate::kimi_http::go_os().as_bytes());
    pb::bytes(&mut meta, 7, b"3000.10.21");
    pb::bytes(&mut meta, 12, b"chisel");
    pb::bytes(&mut meta, 31, fingerprint.as_bytes());
    let mut out = Vec::with_capacity(meta.len() + 4);
    pb::bytes(&mut out, 1, &meta);
    out
}

/// Visits each field of a protobuf message; stops quietly at the first parse error.
/// `f` receives the field number and either its bytes or its varint.
fn fields(data: &[u8], mut f: impl FnMut(i64, Field<'_>)) -> Result<(), pb::Error> {
    let mut pos = 0;
    while pos < data.len() {
        let (num, wire, n) = pb::consume_tag(&data[pos..])?;
        pos += n;
        match wire {
            pb::BYTES => {
                let (b, n) = pb::consume_bytes(&data[pos..])?;
                pos += n;
                f(num, Field::Bytes(b));
            }
            pb::VARINT => {
                let (v, n) = pb::consume_varint(&data[pos..])?;
                pos += n;
                f(num, Field::Varint(v));
            }
            _ => pos += pb::consume_field_value(num, wire, &data[pos..])?,
        }
    }
    Ok(())
}

enum Field<'a> {
    Bytes(&'a [u8]),
    Varint(u64),
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `parseSecondsSubfield`: the varint field 1, 0 when absent or malformed.
fn seconds(data: &[u8]) -> i64 {
    let mut pos = 0;
    while pos < data.len() {
        let Ok((num, wire, n)) = pb::consume_tag(&data[pos..]) else {
            return 0;
        };
        pos += n;
        if wire == pb::VARINT {
            let Ok((v, n)) = pb::consume_varint(&data[pos..]) else {
                return 0;
            };
            pos += n;
            if num == 1 {
                return v as i64;
            }
        } else {
            let Ok(n) = pb::consume_field_value(num, wire, &data[pos..]) else {
                return 0;
            };
            pos += n;
        }
    }
    0
}

/// `ParseGetUserStatusResponse`.
pub fn parse_user_status(data: &[u8]) -> Result<UserStatus, String> {
    if data.is_empty() {
        return Err("empty response data".into());
    }
    let mut status = UserStatus::default();
    let mut pos = 0;
    while pos < data.len() {
        let (num, wire, n) = pb::consume_tag(&data[pos..]).map_err(|e| e.to_string())?;
        pos += n;
        if num == 1 && wire == pb::BYTES {
            let (user, n) = pb::consume_bytes(&data[pos..]).map_err(|e| e.to_string())?;
            pos += n;
            parse_user(user, &mut status);
        } else {
            pos += pb::consume_field_value(num, wire, &data[pos..]).map_err(|e| e.to_string())?;
        }
    }
    Ok(status)
}

fn parse_user(data: &[u8], status: &mut UserStatus) {
    let _ = fields(data, |num, field| {
        if let Field::Bytes(b) = field {
            match num {
                3 => status.user_name = text(b),
                5 => status.team_id = text(b),
                7 => status.email = text(b),
                13 => parse_plan_status(b, status),
                36 => status.user_id = text(b),
                _ => {}
            }
        }
    });
}

fn parse_plan_status(data: &[u8], status: &mut UserStatus) {
    let _ = fields(data, |num, field| match field {
        Field::Bytes(b) => match num {
            1 => parse_plan_info(b, status),
            2 => {
                let sec = seconds(b);
                if sec > 0 {
                    status.plan_start = unix(sec);
                }
            }
            3 => {
                let sec = seconds(b);
                if sec > 0 {
                    status.plan_end = unix(sec);
                }
            }
            _ => {}
        },
        Field::Varint(v) => match num {
            14 => status.daily_quota_remaining_percent = v as i64,
            15 => status.weekly_quota_remaining_percent = v as i64,
            17 if v > 0 => status.daily_quota_reset_at = unix(v as i64),
            18 if v > 0 => status.weekly_quota_reset_at = unix(v as i64),
            _ => {}
        },
    });
}

fn parse_plan_info(data: &[u8], status: &mut UserStatus) {
    let _ = fields(data, |num, field| {
        if let Field::Bytes(b) = field {
            match num {
                2 => status.plan = text(b),
                33 => {
                    let _ = fields(b, |num, field| {
                        if let Field::Bytes(b) = field {
                            match num {
                                4 => status.org_id = text(b),
                                8 => status.org_name = text(b),
                                _ => {}
                            }
                        }
                    });
                }
                _ => {}
            }
        }
    });
}

/// `DevinAuthService`: PKCE URL, code exchange, profile and seat status.
#[derive(Clone)]
pub struct DevinAuth {
    client: wreq::Client,
    app_base: String,
    api_base: String,
    server_base: String,
    /// The executor's Devin transport (`NewDevinHTTPClient`): no automatic gzip.
    devin_transport: bool,
}

impl DevinAuth {
    pub fn new(client: wreq::Client) -> Self {
        Self {
            client,
            app_base: DEFAULT_APP_BASE_URL.into(),
            api_base: DEFAULT_API_BASE_URL.into(),
            server_base: DEFAULT_SERVER_URL.into(),
            devin_transport: false,
        }
    }

    fn base(url: &str) -> Option<String> {
        let url = url.trim();
        (!url.is_empty()).then(|| url.trim_end_matches('/').to_owned())
    }

    /// `SetAppBaseURL`.
    pub fn with_app_base(mut self, url: &str) -> Self {
        if let Some(b) = Self::base(url) {
            self.app_base = b;
        }
        self
    }

    /// `SetAPIBaseURL`.
    pub fn with_api_base(mut self, url: &str) -> Self {
        if let Some(b) = Self::base(url) {
            self.api_base = b;
        }
        self
    }

    /// `SetServerBaseURL`.
    pub fn with_server_base(mut self, url: &str) -> Self {
        if let Some(b) = Self::base(url) {
            self.server_base = b;
        }
        self
    }

    /// Sends through the executor's Devin transport (no automatic gzip).
    pub(crate) fn with_devin_transport(mut self) -> Self {
        self.devin_transport = true;
        self
    }

    /// `BuildAuthorizationURL`: query parameters in the Devin CLI's order; without a
    /// redirect URI it is the headless manual-code URL (`cli_pkce_marker=1`).
    pub fn build_authorization_url(&self, redirect_uri: &str, code_challenge: &str, state: &str) -> String {
        let redirect = redirect_uri.trim();
        let mut parts = Vec::new();
        if !redirect.is_empty() {
            parts.push(format!("redirect_uri={}", query_escape(redirect)));
        }
        if !state.is_empty() {
            parts.push(format!("state={}", query_escape(state)));
        }
        parts.push("prompt=select_account".into());
        parts.push(format!("code_challenge={}", query_escape(code_challenge)));
        parts.push("code_challenge_method=S256".into());
        if redirect.is_empty() {
            parts.push("cli_pkce_marker=1".into());
        }
        format!(
            "{}/auth/cli/continue?{}",
            self.app_base.trim_end_matches('/'),
            parts.join("&")
        )
    }

    fn headers(&self) -> GoHeaders {
        let mut h = GoHeaders::new();
        if self.devin_transport {
            h.disable_compression();
        }
        h
    }

    /// `ExchangeCodeForToken`: the session token for an authorization code.
    pub async fn exchange_code(&self, code: &str, code_verifier: &str) -> Result<String, String> {
        let mut body = br#"{"code":"#.to_vec();
        gj::marshal_str(&mut body, code.trim().as_bytes(), true);
        body.extend_from_slice(br#","code_verifier":"#);
        gj::marshal_str(&mut body, code_verifier.trim().as_bytes(), true);
        body.push(b'}');
        let mut h = self.headers();
        h.set("Content-Type", "application/json");
        h.set("Accept", "application/json");
        let url = format!("{}/auth/cli/token", self.api_base.trim_end_matches('/'));
        let upstream = request(
            &self.client,
            wreq::Method::POST,
            &url,
            h,
            Some(Bytes::from(body)),
            Some(LOGIN_TIMEOUT),
        )
        .await
        .map_err(|e| format!("devin token exchange failed: {}", lossy(&e.body)))?;
        let status = upstream.status;
        let data = read_all(upstream.body, BODY_LIMIT, false)
            .await
            .map_err(|e| format!("read token exchange response: {}", lossy(&e.body)))?;
        // Go appends the response body to both errors; it can carry the session token and
        // the message is logged, so it is withheld.
        if !(200..300).contains(&status) {
            return Err(format!("token exchange failed with status {status}"));
        }
        let token = gj::get(&data, "token").str().trim().to_owned();
        if token.is_empty() {
            return Err("response did not contain a valid token".into());
        }
        Ok(token)
    }

    /// `FetchSelfProfile`: user name, user ID and org ID from `/v3/self` (empty unless
    /// the answer is 200).
    pub async fn fetch_self_profile(&self, session_token: &str) -> Result<(String, String, String), String> {
        let mut h = self.headers();
        h.set("Authorization", format!("Bearer {session_token}"));
        h.set("Accept", "application/json");
        let url = format!("{}/v3/self", self.api_base.trim_end_matches('/'));
        let upstream = request(&self.client, wreq::Method::GET, &url, h, None, Some(LOGIN_TIMEOUT))
            .await
            .map_err(|e| lossy(&e.body))?;
        let data = read_all(upstream.body, BODY_LIMIT, false)
            .await
            .map_err(|e| lossy(&e.body))?;
        if upstream.status != 200 {
            return Ok(Default::default());
        }
        let field = |p: &str| gj::get(&data, p).str().into_owned();
        Ok((field("user_name"), field("user_id"), field("org_id")))
    }

    /// `FetchUserStatus`: `GetUserStatus` over Connect-RPC unary protobuf.
    pub async fn fetch_user_status(&self, session_token: &str, device_seed: &str) -> Result<UserStatus, String> {
        let token = session_token.trim();
        if token.is_empty() {
            return Err("devin auth service: session token is required".into());
        }
        let body = user_status_request(token, &device_fingerprint(device_seed));
        let mut h = self.headers();
        h.set("Authorization", format!("Basic {token}-{token}"));
        h.set("Connect-Protocol-Version", "1");
        h.set("Content-Type", "application/proto");
        h.set("Accept", "*/*");
        h.set("User-Agent", "");
        let url = format!("{}{USER_STATUS_PATH}", self.server_base.trim_end_matches('/'));
        let upstream = request(
            &self.client,
            wreq::Method::POST,
            &url,
            h,
            Some(Bytes::from(body)),
            Some(LOGIN_TIMEOUT),
        )
        .await
        .map_err(|e| lossy(&e.body))?;
        let data = read_all(upstream.body, STATUS_LIMIT, false)
            .await
            .map_err(|e| lossy(&e.body))?;
        if upstream.status != 200 {
            // Go appends the response body; it is withheld from logs and clients.
            return Err(format!("devin seat management error (status {})", upstream.status));
        }
        parse_user_status(&data)
    }

    /// `CreateAuthRecord`: the credential for a session token. Profile and status are
    /// best-effort; the file name never takes path components from upstream values.
    pub async fn create_auth_record(&self, token: &str) -> Result<AuthRecord, String> {
        let session_token = format_session_token(token);
        if session_token.is_empty() {
            return Err("devin session token is required".into());
        }
        let (mut user_name, mut user_id, mut org_id) = match self.fetch_self_profile(&session_token).await {
            Ok(profile) => profile,
            Err(_) => {
                tracing::warn!("failed to fetch devin user profile");
                Default::default()
            }
        };
        let status = match self.fetch_user_status(&session_token, "").await {
            Ok(status) => Some(status),
            Err(_) => {
                tracing::warn!("failed to fetch devin user status and quota");
                None
            }
        };
        let (mut email, mut plan) = (String::new(), String::new());
        if let Some(s) = &status {
            if user_name.is_empty() {
                user_name.clone_from(&s.user_name);
            }
            if user_id.is_empty() {
                user_id.clone_from(&s.user_id);
            }
            if org_id.is_empty() {
                org_id.clone_from(&s.org_id);
            }
            email.clone_from(&s.email);
            plan.clone_from(&s.plan);
        }
        let mut identifier = if user_name.is_empty() {
            user_id.clone()
        } else {
            user_name.clone()
        };
        if identifier.is_empty() {
            identifier = format!("user-{}", hex8(session_token.as_bytes()));
        }
        let safe: String = identifier
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let file_identifier = if safe != identifier || safe.len() > 160 {
            format!("user-{}", hex8(identifier.as_bytes()))
        } else {
            safe
        };
        let file_name = format!("devin-{file_identifier}.json");
        let label = if email.is_empty() {
            format!("Devin ({identifier})")
        } else {
            format!("Devin ({identifier} - {email})")
        };
        let mut attributes = BTreeMap::new();
        let mut metadata = Map::new();
        metadata.insert("type".into(), "devin".into());
        for (key, value) in [
            ("api_key", &session_token),
            ("session_token", &session_token),
            ("user_name", &user_name),
            ("user_id", &user_id),
            ("org_id", &org_id),
        ] {
            attributes.insert(key.to_owned(), value.clone());
            metadata.insert(key.into(), value.clone().into());
        }
        attributes.insert("base_url".into(), DEFAULT_SERVER_URL.into());
        attributes.insert("auth_kind".into(), "oauth".into());
        metadata.insert("auth_kind".into(), "oauth".into());
        for (key, value) in [("email", &email), ("plan", &plan)] {
            if !value.is_empty() {
                attributes.insert(key.into(), value.clone());
                metadata.insert(key.into(), value.clone().into());
            }
        }
        let quota_signals = status
            .as_ref()
            .map(|s| s.quota_signals().into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
            .unwrap_or_else(|| {
                let mut only_plan = BTreeMap::new();
                if !plan.is_empty() {
                    only_plan.insert("plan".to_owned(), plan.clone());
                }
                only_plan
            });
        Ok(AuthRecord {
            file_name,
            label,
            attributes,
            metadata,
            quota_signals,
        })
    }
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `fmt.Sprintf("%x", sha256(b)[:8])`.
fn hex8(b: &[u8]) -> String {
    Sha256::digest(b)[..8].iter().map(|x| format!("{x:02x}")).collect()
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

/// The credential a Devin login produces (Go's `*coreauth.Auth` from `CreateAuthRecord`).
#[derive(Debug, Clone, PartialEq)]
pub struct AuthRecord {
    /// `devin-<identifier>.json`; also the credential ID.
    pub file_name: String,
    pub label: String,
    pub attributes: BTreeMap<String, String>,
    /// What the file store persists.
    pub metadata: Map<String, Value>,
    /// Initial quota observation (plan, daily and weekly remaining, resets).
    pub quota_signals: BTreeMap<String, String>,
}

/// `Manager.Login` persistence for a Devin record (`FileTokenStore.Save`'s metadata
/// branch): an existing file's settings are merged in (never its token payload), keys
/// normalized, the weight validated, `disabled` stamped, then Go's compact JSON. An
/// unchanged file is left alone. Returns the path.
pub fn save_record(auth_dir: &Path, record: &AuthRecord) -> Result<PathBuf, String> {
    let path = auth_dir.join(&record.file_name);
    let mut metadata: GoMap = record
        .metadata
        .iter()
        .map(|(k, v)| (k.clone(), GoValue::from_json(v)))
        .collect();
    let mut disabled = false;
    let existing = std::fs::read(&path).ok();
    if let Some(raw) = existing.as_deref().filter(|r| !r.is_empty())
        && let Some((map, true)) = go_map_from_json(raw)
        && !map.is_empty()
        && let Some(flag) = merge_existing(&mut metadata, &map, "devin")
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
    let raw = GoValue::Object(metadata).marshal();
    // jsonEqual: an existing file with the same content is not rewritten.
    if let Some(old) = existing
        && serde_json::from_slice::<Value>(&old).ok() == serde_json::from_slice::<Value>(&raw).ok()
        && serde_json::from_slice::<Value>(&old).is_ok()
    {
        return Ok(path);
    }
    crate::kimi_auth::write_private(&path, &raw).map_err(|e| format!("auth filestore: write file failed: {e}"))?;
    Ok(path)
}

/// A pasted value that is neither a code, a callback URL nor a token
/// (`errDevinUnrecognizedPaste`).
pub const UNRECOGNIZED_PASTE: &str = "unrecognized devin authorization code or token format";

/// `parseDevinManualPaste`: `(code, token)` from a pasted authorization code, callback
/// URL or session token. Empty input gives two empty values.
pub fn parse_manual_paste(input: &str, expected_state: &str) -> Result<(String, String), String> {
    let trimmed = input.trim().trim_matches(['"', '\'']).trim();
    if trimmed.is_empty() {
        return Ok(Default::default());
    }
    if trimmed.starts_with(TOKEN_PREFIX) || trimmed.starts_with("eyJ") {
        return Ok((String::new(), trimmed.to_owned()));
    }
    if let Ok(Some(parsed)) = crate::claude_login::parse_callback_input(trimmed) {
        let mut error = parsed.error.trim().to_owned();
        if !error.is_empty() {
            let description = parsed.description.trim();
            if !description.is_empty() {
                error = format!("{error}: {description}");
            }
            return Err(format!("devin oauth error: {error}"));
        }
        if !parsed.code.is_empty() {
            if !expected_state.is_empty() && !parsed.state.is_empty() && parsed.state != expected_state {
                return Err("devin oauth state mismatch (possible CSRF)".into());
            }
            return Ok((parsed.code, String::new()));
        }
    }
    if !trimmed.contains([' ', '\t', '\r', '\n', '/', '?', '#', '=']) {
        return Ok((trimmed.to_owned(), String::new()));
    }
    Err(UNRECOGNIZED_PASTE.into())
}

const LOGIN_SUCCESS_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <title>Authentication Successful - Devin</title>
    <style>
        body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; display: flex; justify-content: center; align-items: center; height: 100vh; margin: 0; background: #0f172a; color: #f8fafc; }
        .card { background: #1e293b; padding: 2.5rem; border-radius: 12px; box-shadow: 0 8px 30px rgba(0,0,0,0.4); text-align: center; max-width: 420px; }
        h2 { margin-top: 0; color: #38bdf8; }
        p { color: #94a3b8; font-size: 15px; }
    </style>
</head>
<body>
    <div class="card">
        <h2>Authentication Complete</h2>
        <p>You have successfully logged in to Devin via CLIProxyAPI.</p>
        <p>You may safely close this window and return to your terminal.</p>
    </div>
</body>
</html>"#;

const LOGIN_FAILURE_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <title>Authentication Failed - Devin</title>
    <style>
        body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; display: flex; justify-content: center; align-items: center; height: 100vh; margin: 0; background: #0f172a; color: #f8fafc; }
        .card { background: #1e293b; padding: 2.5rem; border-radius: 12px; box-shadow: 0 8px 30px rgba(0,0,0,0.4); text-align: center; max-width: 420px; }
        h2 { margin-top: 0; color: #f87171; }
        p { color: #94a3b8; font-size: 15px; }
    </style>
</head>
<body>
    <div class="card">
        <h2>Authentication Failed</h2>
        <p>Devin authentication encountered an error: %s</p>
        <p>Please check your terminal and try again.</p>
    </div>
</body>
</html>"#;

/// Go `html.EscapeString`.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&#39;"),
            '"' => out.push_str("&#34;"),
            c => out.push(c),
        }
    }
    out
}

/// What the loopback `/callback` received (`OAuthResult`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallbackResult {
    pub code: String,
    pub state: String,
    pub error: String,
}

/// `OAuthServer.handleCallback`: the HTTP status, page and result for a callback query.
pub(crate) fn callback_response(query: &str) -> (u16, String, CallbackResult) {
    let get = |name: &str| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.trim().to_owned())
            .unwrap_or_default()
    };
    let (code, state, error, description) = (get("code"), get("state"), get("error"), get("error_description"));
    if !error.is_empty() || code.is_empty() {
        let mut message = error.clone();
        if !description.is_empty() {
            message = format!("{error}: {description}");
        }
        if message.is_empty() {
            message = "missing authorization code".into();
        }
        let page = LOGIN_FAILURE_HTML.replacen("%s", &html_escape(&message), 1);
        return (
            400,
            page,
            CallbackResult {
                error: message,
                ..Default::default()
            },
        );
    }
    (
        200,
        LOGIN_SUCCESS_HTML.to_owned(),
        CallbackResult {
            code,
            state,
            error: String::new(),
        },
    )
}

/// Go's `OAuthServer` on `127.0.0.1:<port>`: answers `/callback`, 404 elsewhere, and
/// sends the first result to `results`.
async fn serve_callbacks(listener: tokio::net::TcpListener, results: tokio::sync::mpsc::Sender<CallbackResult>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    loop {
        let Ok((socket, _)) = listener.accept().await else {
            continue;
        };
        let results = results.clone();
        tokio::spawn(async move {
            let work = async move {
                let (read, mut write) = socket.into_split();
                let mut reader = BufReader::new(read);
                let mut line = String::new();
                reader.read_line(&mut line).await.ok()?;
                let target = line.split_whitespace().nth(1)?.to_owned();
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).await.ok()? == 0 || header.trim().is_empty() {
                        break;
                    }
                }
                let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                let (status, page) = if path == "/callback" {
                    let (status, page, result) = callback_response(query);
                    let _ = results.try_send(result);
                    (status, page)
                } else {
                    (404, "404 page not found\n".to_owned())
                };
                let reason = if status == 200 {
                    "OK"
                } else if status == 400 {
                    "Bad Request"
                } else {
                    "Not Found"
                };
                let content_type = if status == 404 {
                    "text/plain; charset=utf-8"
                } else {
                    "text/html; charset=utf-8"
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                    page.len()
                );
                write.write_all(response.as_bytes()).await.ok()
            };
            let _ = tokio::time::timeout(Duration::from_secs(10), work).await;
        });
    }
}

/// One prompt read on a detached thread (`misc.AsyncPrompt`).
async fn prompt(message: &'static str) -> std::io::Result<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        use std::io::Write;
        print!("{message}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let read = std::io::stdin().read_line(&mut line).map(|_| line);
        let _ = tx.send(read);
    });
    rx.await
        .unwrap_or_else(|_| Err(std::io::Error::other("prompt thread ended")))
}

/// `DevinAuthenticator.Login`: the session token from a browser callback, a pasted
/// callback URL or code, or a pasted token; then the auth record.
pub(crate) async fn login_with(auth: &DevinAuth, no_browser: bool, callback_port: u16) -> Result<AuthRecord, String> {
    let pkce = generate_pkce().map_err(|e| format!("devin pkce generation failed: {e}"))?;
    let state = random_state().map_err(|e| format!("devin state generation failed: {e}"))?;
    let (code, token) = if no_browser {
        let url = auth.build_authorization_url("", &pkce.challenge, &state);
        print!("Visit the following URL to continue Devin authentication:\n{url}\n\n");
        let input = prompt("Paste the Devin authorization code or session token directly: ")
            .await
            .map_err(|e| format!("failed to read devin input: {e}"))?;
        let (code, token) = parse_manual_paste(&input, &state)?;
        if code.is_empty() && token.is_empty() {
            return Err("devin authentication canceled: empty input received".into());
        }
        (code, token)
    } else {
        browser_callback(auth, &pkce, &state, callback_port).await?
    };
    let session_token = if !token.is_empty() {
        format_session_token(&token)
    } else if !code.is_empty() {
        let token = auth
            .exchange_code(&code, &pkce.verifier)
            .await
            .map_err(|e| format!("failed to exchange devin authorization code: {e}"))?;
        format_session_token(&token)
    } else {
        return Err("no authorization code or token received".into());
    };
    auth.create_auth_record(&session_token).await
}

/// The browser half of `Login`: a loopback callback, with a manual paste offered after
/// five seconds and a five-minute limit.
async fn browser_callback(auth: &DevinAuth, pkce: &Pkce, state: &str, port: u16) -> Result<(String, String), String> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
        format!(
            "failed to start devin oauth callback server: failed to bind local OAuth server to 127.0.0.1:{port}: {e}"
        )
    })?;
    let actual = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let server = tokio::spawn(serve_callbacks(listener, tx));
    let _stop = AbortOnDrop(server);
    let redirect = format!("http://127.0.0.1:{actual}/callback");
    let url = auth.build_authorization_url(&redirect, &pkce.challenge, state);
    println!("Opening browser for Devin authentication...");
    if !crate::kimi_auth::open_browser(&url) {
        tracing::warn!("No browser available; please open the URL manually");
        crate::claude_login::print_ssh_tunnel_instructions(actual);
        println!("Visit the following URL to continue authentication:\n{url}");
    }
    println!("Waiting for Devin authentication callback...");
    let deadline = tokio::time::sleep(Duration::from_secs(5 * 60));
    tokio::pin!(deadline);
    let manual_delay = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(manual_delay);
    let mut delay_done = false;
    let mut manual: Option<std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<String>> + Send>>> = None;
    let accept = |result: CallbackResult| -> Result<(String, String), String> {
        if !result.error.is_empty() {
            return Err(format!("devin oauth error: {}", result.error));
        }
        if !state.is_empty() && result.state != state {
            return Err("devin oauth state mismatch (possible CSRF)".into());
        }
        Ok((result.code, String::new()))
    };
    loop {
        tokio::select! {
            Some(result) = rx.recv() => return accept(result),
            () = &mut deadline => {
                return Err("devin oauth callback failed: devin authentication timed out".into());
            }
            () = &mut manual_delay, if !delay_done => {
                delay_done = true;
                if let Ok(result) = rx.try_recv() {
                    return accept(result);
                }
                manual = Some(Box::pin(prompt(
                    "Paste the Devin callback URL, authorization code, or session token directly (or press Enter to keep waiting): ",
                )));
            }
            input = async { manual.as_mut().expect("guarded").await }, if manual.is_some() => {
                manual = None;
                let Ok(input) = input else {
                    continue;
                };
                match parse_manual_paste(&input, state) {
                    Err(e) if e == UNRECOGNIZED_PASTE => continue,
                    Err(e) => return Err(e),
                    Ok((code, token)) if !token.is_empty() || !code.is_empty() => return Ok((code, token)),
                    Ok(_) => {}
                }
            }
        }
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// `--devin-login` (`DoDevinLogin`): browser or headless login through
/// `requests.proxy-url`, then the credential in `auth-dir`. Failures are logged and the
/// command still exits normally, as in Go.
pub async fn login(cfg: &cpa_core::config::Config, no_browser: bool, callback_port: u16) -> Option<PathBuf> {
    let proxy = cfg
        .document
        .get("requests")
        .and_then(|r| r.get("proxy-url"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let client = crate::proxy::GoClients::new(crate::proxy::Hooks::default()).get(&crate::proxy::Proxy::parse(proxy));
    let auth = DevinAuth::new(client);
    let record = match login_with(&auth, no_browser, callback_port).await {
        Ok(record) => record,
        Err(e) => {
            tracing::error!("Devin authentication failed: {e}");
            return None;
        }
    };
    let auth_dir = cfg.auth_dir.clone();
    let saved = {
        let record = record.clone();
        tokio::task::spawn_blocking(move || save_record(&auth_dir, &record))
            .await
            .map_err(|_| "credential publication failed".to_owned())
            .and_then(|r| r)
    };
    let path = match saved {
        Ok(path) => path,
        Err(e) => {
            tracing::error!("Devin authentication failed: {e}");
            return None;
        }
    };
    println!("Authentication saved to {}", path.display());
    if !record.label.is_empty() {
        println!("Authenticated as {}", record.label);
    }
    println!("Devin authentication successful!");
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_s256_of_the_verifier() {
        let pkce = generate_pkce().unwrap();
        assert_eq!(pkce.verifier.len(), 86, "64 bytes, base64url without padding");
        assert_eq!(
            pkce.challenge,
            URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()))
        );
    }

    #[test]
    fn callback_page_escapes_the_error() {
        let (status, page, result) = callback_response("error=a%3Cb&error_description=x%26y");
        assert_eq!(status, 400);
        assert_eq!(result.error, "a<b: x&y");
        assert!(page.contains("encountered an error: a&lt;b: x&amp;y</p>"));
        let (status, _, result) = callback_response("state=s");
        assert_eq!((status, result.error.as_str()), (400, "missing authorization code"));
        let (status, page, result) = callback_response("code=%20c%20&state=s");
        assert_eq!((status, result.code.as_str(), result.state.as_str()), (200, "c", "s"));
        assert_eq!(page, LOGIN_SUCCESS_HTML);
    }
}
