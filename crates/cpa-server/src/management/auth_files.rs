//! Credential management, ported from Go's auth-file handlers
//! (internal/api/handlers/management/auth_files*.go, quota.go `ResetQuota`,
//! model_definitions.go). Response maps follow gin: `gin.H` keys are sorted.
//!
//! Credentials are the runtime store's; files are written atomically (0600) and the
//! store is re-synthesized from disk after every change, so attributes the scheduler
//! reads (priority, weight, headers, exclusions) are current immediately.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, SystemTime};

use axum::body::Bytes;
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use cpa_core::config::{ConfigDocument, credentials};
use cpa_core::credential::{Credential, MetadataPatch, Source};
use serde_json::{Map, Value, json};

use super::{Management, json as respond};
use crate::runtime::PatchError;

const MODEL_DEFINITIONS: &str = include_str!("model_definitions.json");

/// `url.ParseQuery` pairs in order: names and values decoded, malformed pairs skipped.
pub(super) struct Query(Vec<(String, String)>);

impl Query {
    pub(super) fn parse(raw: Option<String>) -> Self {
        let raw = raw.unwrap_or_default();
        let decode = |s: &str| -> Option<String> {
            let bytes = s.as_bytes();
            let mut out = Vec::with_capacity(bytes.len());
            let mut i = 0;
            while i < bytes.len() {
                match bytes[i] {
                    b'%' => {
                        let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
                        out.push(u8::from_str_radix(hex, 16).ok()?);
                        i += 3;
                    }
                    b'+' => {
                        out.push(b' ');
                        i += 1;
                    }
                    b => {
                        out.push(b);
                        i += 1;
                    }
                }
            }
            Some(String::from_utf8_lossy(&out).into_owned())
        };
        Self(
            raw.split('&')
                .filter(|p| !p.is_empty() && !p.contains(';'))
                .filter_map(|pair| {
                    let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                    Some((decode(k)?, decode(v)?))
                })
                .collect(),
        )
    }

    pub(super) fn get(&self, key: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// gin `c.Query`: the first value, or empty.
    pub(super) fn first(&self, key: &str) -> &str {
        self.get(key).unwrap_or_default()
    }

    fn all(&self, key: &str) -> Vec<&str> {
        self.0
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    }
}

fn h(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    let sorted: BTreeMap<&str, Value> = pairs.into_iter().collect();
    Value::Object(sorted.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

fn reply(status: StatusCode, pairs: impl IntoIterator<Item = (&'static str, Value)>) -> Response {
    respond(status, &h(pairs))
}

fn fail(status: StatusCode, message: impl Into<String>) -> Response {
    reply(status, [("error", Value::from(message.into()))])
}

/// Go `time.Time` JSON: RFC 3339 with trailing fractional zeros removed.
pub(super) fn go_time<Tz: chrono::TimeZone>(t: chrono::DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let text = t.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let Some(dot) = text.find('.') else { return text };
    let end = text[dot..]
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .map_or(text.len(), |i| dot + i);
    let fraction = text[dot + 1..end].trim_end_matches('0');
    if fraction.is_empty() {
        format!("{}{}", &text[..dot], &text[end..])
    } else {
        format!("{}.{fraction}{}", &text[..dot], &text[end..])
    }
}

fn local(t: SystemTime) -> String {
    go_time(chrono::DateTime::<chrono::Local>::from(t))
}

fn file_name(c: &Credential) -> Option<String> {
    match &c.source {
        Source::File(p) => p.file_name().map(|n| n.to_string_lossy().into_owned()),
        Source::Config { .. } => None,
    }
}

fn path_of(c: &Credential) -> Option<&Path> {
    match &c.source {
        Source::File(p) => Some(p),
        Source::Config { .. } => None,
    }
}

/// Go `lookupAuthFile`: by ID, then file name; with an index, the first match of both.
fn lookup(state: &Management, name: &str, auth_index: &str) -> Option<Arc<Credential>> {
    let (name, auth_index) = (name.trim(), auth_index.trim());
    if name.is_empty() {
        return None;
    }
    let all = state.rt.store().snapshot();
    let matches_name = |c: &Credential| c.id.trim() == name || file_name(c).as_deref() == Some(name);
    if auth_index.is_empty() {
        return all
            .iter()
            .find(|c| c.id == name)
            .or_else(|| all.iter().find(|c| file_name(c).as_deref() == Some(name)))
            .cloned();
    }
    all.into_iter()
        .find(|c| matches_name(c) && credentials::auth_index(c) == auth_index)
}

fn unsafe_name(name: &str) -> bool {
    name.trim().is_empty() || name.contains(['/', '\\'])
}

fn ends_json(name: &str) -> bool {
    name.to_lowercase().ends_with(".json")
}

// ---------------------------------------------------------------------------
// Listing

fn meta_str<'a>(c: &'a Credential, key: &str) -> Option<&'a str> {
    c.metadata.get(key).and_then(Value::as_str)
}

fn jwt_claims(token: &str) -> Option<Value> {
    use base64::Engine;
    let parts: Vec<&str> = token.trim().split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = parts[1].trim_end_matches('=');
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Go `parseTimeValue`.
fn parse_time(v: &Value) -> Option<SystemTime> {
    let unix = |n: i64| -> SystemTime {
        // normaliseUnix: millisecond values are scaled down.
        let secs = if n > 1_000_000_000_000 { n / 1000 } else { n };
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64)
    };
    match v {
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
                return Some(t.into());
            }
            for layout in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
                if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, layout) {
                    return Some(t.and_utc().into());
                }
            }
            s.parse::<i64>().ok().map(unix)
        }
        Value::Number(n) => n.as_f64().map(|f| unix(f as i64)),
        _ => None,
    }
}

/// Go `Auth.AccessTokenExpirationTime`: JWT `exp`, then the expiry metadata keys.
fn access_token_expiry(c: &Credential) -> Option<SystemTime> {
    let token = meta_str(c, "access_token")
        .or_else(|| meta_str(c, "accessToken"))
        .or_else(|| c.metadata.get("token")?.get("access_token")?.as_str())
        .filter(|t| !t.trim().is_empty())?;
    if let Some(exp) = jwt_claims(token).and_then(|claims| claims.get("exp").and_then(parse_time)) {
        return Some(exp);
    }
    expiry_from(&c.metadata)
}

fn expiry_from(meta: &Map<String, Value>) -> Option<SystemTime> {
    for key in ["expired", "expire", "expires_at", "expiresAt", "expiry", "expires"] {
        if let Some(t) = meta.get(key).and_then(parse_time) {
            return Some(t);
        }
    }
    let seconds = ["expires_in", "expiresIn"]
        .iter()
        .find_map(|k| meta.get(*k)?.as_i64().filter(|s| *s > 0));
    let issued = ["timestamp", "issued_at", "issuedAt"]
        .iter()
        .find_map(|k| meta.get(*k).and_then(parse_time));
    if let (Some(seconds), Some(issued)) = (seconds, issued) {
        return Some(issued + Duration::from_secs(seconds as u64));
    }
    ["token", "Token"]
        .iter()
        .find_map(|k| meta.get(*k)?.as_object().and_then(expiry_from))
}

/// Go `newCooldownView` over the scheduler's cooldown state.
fn cooldown_view(cd: &crate::scheduler::CooldownState) -> Value {
    let retry_at = SystemTime::now() + cd.remaining;
    let seconds = cd.remaining.as_secs() + u64::from(cd.remaining.subsec_nanos() > 0);
    let credential_quota = cd.model.is_empty() && cd.quota;
    let reason = if credential_quota {
        "credential_quota"
    } else if cd.quota {
        "quota"
    } else {
        match cd.status {
            401 => "unauthorized",
            402 | 403 => "payment_required",
            404 => "not_found",
            429 => "quota",
            408 | 500 | 502 | 503 | 504 | 520..=526 => "transient_error",
            _ => "unknown",
        }
    };
    let mut view = Map::new();
    view.insert(
        "scope".into(),
        (if cd.model.is_empty() { "credential" } else { "model" }).into(),
    );
    if !cd.model.is_empty() {
        view.insert("model_key".into(), cd.model.clone().into());
    }
    view.insert("reason".into(), reason.into());
    view.insert(
        "retry_at".into(),
        go_time(chrono::DateTime::<chrono::Utc>::from(retry_at)).into(),
    );
    view.insert("remaining_seconds".into(), seconds.into());
    if reason == "quota" {
        view.insert("backoff_level".into(), cd.level.into());
    }
    if !credential_quota && (400..=599).contains(&cd.status) && reason != "unknown" {
        view.insert("http_status".into(), cd.status.into());
    }
    Value::Object(view)
}

fn recent_requests(activity: &crate::runtime::CredentialActivity) -> Value {
    use chrono::TimeZone;
    let span = crate::runtime::RECENT_BUCKET_SECONDS;
    let now = chrono::Utc::now().timestamp() / span;
    Value::Array(
        (0..20)
            .rev()
            .map(|back| {
                let bucket = now - back;
                let start = chrono::Local.timestamp_opt(bucket * span, 0).single();
                let end = chrono::Local.timestamp_opt((bucket + 1) * span, 0).single();
                let label = match (start, end) {
                    (Some(s), Some(e)) => format!("{}-{}", s.format("%H:%M"), e.format("%H:%M")),
                    _ => String::new(),
                };
                let (ok, failed) = activity
                    .recent
                    .iter()
                    .find(|(b, _, _)| *b == bucket)
                    .map_or((0, 0), |(_, s, f)| (*s, *f));
                json!({"time": label, "success": ok, "failed": failed})
            })
            .collect(),
    )
}

fn int_value(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Go `buildAuthFileEntryLocked`. `None` hides the credential (config API keys and
/// disabled credentials whose file is gone).
fn entry(state: &Management, c: &Credential) -> Option<BTreeMap<&'static str, Value>> {
    let path = path_of(c)?;
    let store = state.rt.store();
    let now = SystemTime::now();
    let cooldowns = store.cooldowns(&c.id);
    let expired = access_token_expiry(c).is_some_and(|t| t <= now);
    let credential_cooldown = cooldowns.iter().find(|cd| cd.model.is_empty());
    let (status, unavailable) = if c.disabled {
        ("disabled", false)
    } else if expired || credential_cooldown.is_some() {
        ("error", true)
    } else {
        ("active", false)
    };
    let mut e: BTreeMap<&'static str, Value> = BTreeMap::new();
    let name = file_name(c).unwrap_or_else(|| c.id.clone());
    for (k, v) in [
        ("id", Value::from(c.id.clone())),
        ("auth_index", credentials::auth_index(c).into()),
        ("name", name.into()),
        ("type", c.provider.trim().into()),
        ("provider", c.provider.trim().into()),
        ("label", c.label.clone().into()),
        ("status", status.into()),
        ("status_message", "".into()),
        ("disabled", c.disabled.into()),
        ("unavailable", unavailable.into()),
        ("runtime_only", false.into()),
        ("source", "memory".into()),
        ("size", 0.into()),
    ] {
        e.insert(k, v);
    }
    let activity = store.activity(&c.id);
    e.insert("success", activity.success.into());
    e.insert("failed", activity.failed.into());
    e.insert("recent_requests", recent_requests(&activity));
    e.insert("quota", json!({"signals": {}}));
    if let Some(probe) = c.metadata.get("quota_probe").filter(|v| !v.is_null()) {
        e.insert("supports_quota", true.into());
        e.insert("quota_probe", probe.clone());
    }
    let email = meta_str(c, "email").map(str::trim).unwrap_or_default();
    if !email.is_empty() {
        e.insert("email", email.into());
    }
    if let Some(project) = meta_str(c, "project_id").map(str::trim).filter(|p| !p.is_empty()) {
        e.insert("project_id", project.into());
    }
    if c.attributes.get("auth_kind").map(String::as_str) == Some("oauth") {
        e.insert("account_type", "oauth".into());
        if !email.is_empty() {
            e.insert("account", email.into());
        }
    }
    let meta = std::fs::metadata(path);
    let modified = meta.as_ref().ok().and_then(|m| m.modified().ok());
    // ponytail: Go stamps created/updated at synthesis; the store keeps no
    // timestamps, so both report the file's modification time.
    if let Some(t) = modified {
        e.insert("created_at", local(t).into());
        e.insert("updated_at", local(t).into());
    }
    for key in ["last_refresh", "lastRefresh", "last_refreshed_at", "lastRefreshedAt"] {
        if let Some(t) = c.metadata.get(key).and_then(parse_time) {
            e.insert("last_refresh", go_time(chrono::DateTime::<chrono::Utc>::from(t)).into());
            break;
        }
    }
    if let Some(cd) = credential_cooldown.filter(|_| !c.disabled) {
        let at = SystemTime::now() + cd.remaining;
        e.insert("next_retry_after", local(at).into());
    }
    e.insert("path", path.display().to_string().into());
    match meta {
        Ok(m) => {
            e.insert("source", "file".into());
            e.insert("size", m.len().into());
            if let Some(t) = modified {
                e.insert("modtime", local(t).into());
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if c.disabled {
                return None;
            }
        }
        Err(_) => {}
    }
    if c.provider.eq_ignore_ascii_case("codex")
        && let Some(claims) = meta_str(c, "id_token").and_then(jwt_claims)
        && let Some(auth) = claims.get("https://api.openai.com/auth")
    {
        let mut out = Map::new();
        for (from, to) in [
            ("chatgpt_account_id", "chatgpt_account_id"),
            ("chatgpt_plan_type", "plan_type"),
        ] {
            if let Some(v) = auth
                .get(from)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
            {
                out.insert(to.into(), v.into());
            }
        }
        for key in ["chatgpt_subscription_active_start", "chatgpt_subscription_active_until"] {
            if let Some(v) = auth.get(key).filter(|v| !v.is_null()) {
                out.insert(key.into(), v.clone());
            }
        }
        if !out.is_empty() {
            e.insert("id_token", Value::Object(out));
        }
    }
    let priority = c
        .attributes
        .get("priority")
        .filter(|p| !p.trim().is_empty())
        .map(|p| p.trim().parse::<i64>().ok())
        .unwrap_or_else(|| c.metadata.get("priority").and_then(int_value));
    if let Some(p) = priority {
        e.insert("priority", p.into());
    }
    let note = c
        .attributes
        .get("note")
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
        .or_else(|| {
            meta_str(c, "note")
                .map(|n| n.trim().to_owned())
                .filter(|n| !n.is_empty())
        });
    if let Some(note) = note {
        e.insert("note", note.into());
    }
    let weight = match c.attributes.get("weight").filter(|w| !w.trim().is_empty()) {
        Some(w) => credentials::parse_weight(&Value::from(w.trim())).ok(),
        None => c
            .metadata
            .get("weight")
            .filter(|w| !w.is_null())
            .and_then(|w| credentials::parse_weight(w).ok()),
    };
    if let Some(w) = weight {
        e.insert("weight", w.into());
    }
    let bool_of = |v: &Value| match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim() {
            "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
            "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
            _ => None,
        },
        _ => None,
    };
    let websockets = c
        .attributes
        .get("websockets")
        .and_then(|w| bool_of(&Value::from(w.as_str())))
        .or_else(|| c.metadata.get("websockets").and_then(bool_of));
    if let Some(w) = websockets {
        e.insert("websockets", w.into());
    }
    if let Some(retry) = c.metadata.get("request_retry").and_then(int_value).filter(|r| *r >= 0) {
        e.insert("request_retry", retry.into());
    }
    e.insert(
        "cooldowns",
        Value::Array(store.cooldowns(&c.id).iter().map(cooldown_view).collect()),
    );
    Some(e)
}

fn entry_value(e: BTreeMap<&'static str, Value>) -> Value {
    Value::Object(e.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

pub(super) async fn list(State(state): State<Arc<Management>>, RawQuery(raw): RawQuery) -> Response {
    let q = Query::parse(raw);
    let positive = |name: &'static str| -> Result<Option<usize>, String> {
        match q.get(name) {
            None => Ok(None),
            Some(v) => match v.trim().parse::<i64>() {
                Ok(n) if n > 0 => Ok(Some(n as usize)),
                _ => Err(format!("{name} must be a positive integer")),
            },
        }
    };
    let (page, page_size) = match (positive("page"), positive("page_size")) {
        (Ok(page), Ok(size)) => (page, size),
        (Err(msg), _) | (_, Err(msg)) => return fail(StatusCode::BAD_REQUEST, msg),
    };
    let paginated = page.is_some() || page_size.is_some();
    let (page, page_size) = (page.unwrap_or(1), page_size.unwrap_or(50));
    let (name, index) = (
        q.first("name").trim().to_owned(),
        q.first("auth_index").trim().to_owned(),
    );
    let observed = go_time(chrono::Utc::now());
    let mut matching: Vec<Arc<Credential>> = state
        .rt
        .store()
        .snapshot()
        .into_iter()
        .filter(|c| name.is_empty() || c.id.trim() == name || file_name(c).as_deref() == Some(name.as_str()))
        .filter(|c| index.is_empty() || credentials::auth_index(c) == index)
        .collect();
    if !paginated {
        let mut files: Vec<(String, Value)> = matching
            .iter()
            .filter_map(|c| entry(&state, c).map(|e| (file_name(c).unwrap_or_else(|| c.id.clone()), entry_value(e))))
            .collect();
        files.sort_by_key(|(n, _)| n.to_lowercase());
        return reply(
            StatusCode::OK,
            [
                ("observed_at", observed.into()),
                ("files", Value::Array(files.into_iter().map(|(_, e)| e).collect())),
            ],
        );
    }
    // Paginated listings count only listable credentials (Go `isAuthFileListable`).
    matching.retain(|c| path_of(c).is_some_and(|p| p.exists() || !c.disabled));
    matching.sort_by(|a, b| {
        let (na, nb) = (file_name(a).unwrap_or_default(), file_name(b).unwrap_or_default());
        na.to_lowercase()
            .cmp(&nb.to_lowercase())
            .then_with(|| na.cmp(&nb))
            .then_with(|| a.id.cmp(&b.id))
    });
    let total = matching.len();
    let start = ((page - 1).saturating_mul(page_size)).min(total);
    let end = (start + page_size).min(total);
    let files: Vec<Value> = matching[start..end]
        .iter()
        .filter_map(|c| entry(&state, c).map(entry_value))
        .collect();
    reply(
        StatusCode::OK,
        [
            ("observed_at", observed.into()),
            ("files", Value::Array(files)),
            ("total", total.into()),
            ("page", page.into()),
            ("page_size", page_size.into()),
            ("has_more", (end < total).into()),
        ],
    )
}

// ---------------------------------------------------------------------------
// Files

pub(super) async fn download(State(state): State<Arc<Management>>, RawQuery(raw): RawQuery) -> Response {
    let q = Query::parse(raw);
    let name = q.first("name").trim().to_owned();
    if unsafe_name(&name) {
        return fail(StatusCode::BAD_REQUEST, "invalid name");
    }
    if !ends_json(&name) {
        return fail(StatusCode::BAD_REQUEST, "name must end with .json");
    }
    let full = state.rt.config().auth_dir.join(&name);
    match tokio::fs::read(&full).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, HeaderValue::from_static("application/json")),
                (
                    header::CONTENT_DISPOSITION,
                    HeaderValue::from_str(&format!("attachment; filename=\"{name}\""))
                        .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
                ),
            ],
            bytes,
        )
            .into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => fail(StatusCode::NOT_FOUND, "file not found"),
        Err(e) => fail(StatusCode::INTERNAL_SERVER_ERROR, format!("failed to read file: {e}")),
    }
}

/// Exclusive 0600 temp file, then rename: a crash never leaves a partial credential.
fn write_file(dst: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = dst.parent().unwrap_or(Path::new("."));
    let name = dst.file_name().unwrap_or_default().to_string_lossy();
    let mut n = 0u32;
    let (tmp, mut file) = loop {
        let tmp = dir.join(format!(".{name}.{}.{n}.upload", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
        {
            Ok(f) => break (tmp, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e),
        }
    };
    let result = file
        .write_all(data)
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::rename(&tmp, dst));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Re-synthesizes the store from disk and the current config (callers hold `disk`).
fn resync(state: &Management) {
    state.publish((*state.rt.config()).clone(), None);
}

/// Go `writeAuthFile`: validate as Go's `buildAuthFromFileData` does, then persist.
fn store_file(state: &Management, name: &str, data: &[u8]) -> Result<(), String> {
    let cfg = state.rt.config();
    let base = Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dst = std::path::absolute(cfg.auth_dir.join(&base)).unwrap_or_else(|_| cfg.auth_dir.join(&base));
    let meta = match serde_json::from_slice::<Map<String, Value>>(data) {
        Ok(m) => m,
        Err(e) => return Err(format!("invalid auth file: {e}")),
    };
    let synthesized = credentials::from_file(&cfg, &cfg.auth_dir, &dst, data)
        .map_err(|e| format!("invalid auth file: invalid weight in {base}: {e}"))?;
    // Go writes the upload as sent, then its file store rewrites it unless it is
    // already JSON-equal to the canonical form: canonical metadata keys plus the
    // auth's disabled flag, in `json.Marshal` key order with no trailing newline.
    let uploaded: BTreeMap<String, Value> = meta.into_iter().collect();
    let mut persisted = uploaded.clone();
    for (alias, canon) in ALIASES {
        if let Some(v) = persisted.remove(*alias) {
            persisted.entry((*canon).to_owned()).or_insert(v);
        }
    }
    let disabled = synthesized.as_ref().map_or_else(
        || persisted.get("disabled").and_then(Value::as_bool).unwrap_or(false),
        |c| c.disabled,
    );
    persisted.insert("disabled".into(), disabled.into());
    let bytes = if persisted == uploaded {
        data.to_vec()
    } else {
        serde_json::to_vec(&persisted).map_err(|e| e.to_string())?
    };
    let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
    write_file(&dst, &bytes).map_err(|e| format!("failed to write file: {e}"))?;
    resync(state);
    Ok(())
}

struct Part {
    field: String,
    filename: Option<String>,
    data: Vec<u8>,
}

/// Minimal `multipart/form-data` reader (RFC 7578): enough for credential uploads.
fn multipart(content_type: &str, body: &[u8]) -> Result<Vec<Part>, String> {
    let boundary = content_type
        .split(';')
        .filter_map(|p| p.trim().strip_prefix("boundary="))
        .next()
        .map(|b| b.trim_matches('"').to_owned())
        .filter(|b| !b.is_empty())
        .ok_or("no multipart boundary param in Content-Type")?;
    let delimiter = format!("--{boundary}");
    let find = |hay: &[u8], needle: &[u8], from: usize| -> Option<usize> {
        hay.get(from..)?
            .windows(needle.len())
            .position(|w| w == needle)
            .map(|i| i + from)
    };
    let mut pos = find(body, delimiter.as_bytes(), 0).ok_or("multipart: NextPart: EOF")?;
    let mut parts = Vec::new();
    loop {
        pos += delimiter.len();
        if body.get(pos..pos + 2) == Some(b"--") {
            return Ok(parts);
        }
        let headers_start = find(body, b"\r\n", pos).ok_or("multipart: NextPart: EOF")? + 2;
        let headers_end = find(body, b"\r\n\r\n", headers_start - 2).ok_or("malformed MIME header")?;
        let headers = String::from_utf8_lossy(&body[headers_start..headers_end]).into_owned();
        let data_start = headers_end + 4;
        let next = find(body, format!("\r\n{delimiter}").as_bytes(), data_start).ok_or("multipart: NextPart: EOF")?;
        let disposition = headers
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("content-disposition:"))
            .unwrap_or_default();
        let param = |key: &str| -> Option<String> {
            disposition.split(';').find_map(|p| {
                let (k, v) = p.trim().split_once('=')?;
                (k.trim().eq_ignore_ascii_case(key)).then(|| v.trim().trim_matches('"').to_owned())
            })
        };
        parts.push(Part {
            field: param("name").unwrap_or_default(),
            filename: param("filename"),
            data: body[data_start..next].to_vec(),
        });
        pos = next + 2;
    }
}

pub(super) async fn upload(
    State(state): State<Arc<Management>>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let is_multipart = content_type.split(';').next().unwrap_or_default().trim() == "multipart/form-data";
    if is_multipart {
        let mut files = match multipart(&content_type, &body) {
            Ok(parts) => parts.into_iter().filter(|p| p.filename.is_some()).collect::<Vec<_>>(),
            Err(e) => return fail(StatusCode::BAD_REQUEST, format!("invalid multipart form: {e}")),
        };
        // gin groups files by sorted form field name.
        files.sort_by(|a, b| a.field.cmp(&b.field));
        let store_part = |part: &Part| -> Result<String, (bool, String)> {
            let raw_name = part.filename.clone().unwrap_or_default();
            let name = Path::new(raw_name.trim())
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !ends_json(&name) {
                return Err((true, "file must be .json".into()));
            }
            store_file(&state, &name, &part.data)
                .map(|()| name)
                .map_err(|e| (false, e))
        };
        return match files.len() {
            0 => fail(StatusCode::BAD_REQUEST, "no files uploaded"),
            1 => match store_part(&files[0]) {
                Ok(_) => reply(StatusCode::OK, [("status", "ok".into())]),
                Err((true, msg)) => fail(StatusCode::BAD_REQUEST, msg),
                Err((false, msg)) => fail(StatusCode::INTERNAL_SERVER_ERROR, msg),
            },
            _ => {
                let mut uploaded = Vec::new();
                let mut failed = Vec::new();
                for part in &files {
                    match store_part(part) {
                        Ok(name) => uploaded.push(Value::from(name)),
                        Err((_, msg)) => {
                            let name = Path::new(part.filename.as_deref().unwrap_or_default())
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            failed.push(h([("name", name.into()), ("error", msg.into())]));
                        }
                    }
                }
                let count = uploaded.len();
                if failed.is_empty() {
                    reply(
                        StatusCode::OK,
                        [
                            ("status", "ok".into()),
                            ("uploaded", count.into()),
                            ("files", uploaded.into()),
                        ],
                    )
                } else {
                    reply(
                        StatusCode::MULTI_STATUS,
                        [
                            ("status", "partial".into()),
                            ("uploaded", count.into()),
                            ("files", uploaded.into()),
                            ("failed", failed.into()),
                        ],
                    )
                }
            }
        };
    }
    let q = Query::parse(raw);
    let name = q.first("name").trim().to_owned();
    if unsafe_name(&name) {
        return fail(StatusCode::BAD_REQUEST, "invalid name");
    }
    if !ends_json(&name) {
        return fail(StatusCode::BAD_REQUEST, "name must end with .json");
    }
    let result = tokio::task::spawn_blocking(move || store_file(&state, &name, &body)).await;
    match result {
        Ok(Ok(())) => reply(StatusCode::OK, [("status", "ok".into())]),
        Ok(Err(msg)) => fail(StatusCode::INTERNAL_SERVER_ERROR, msg),
        Err(_) => fail(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    }
}

fn delete_names(q: &Query, body: &[u8]) -> Result<Vec<String>, &'static str> {
    let unique = |names: Vec<String>| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for n in names {
            let n = n.trim().to_owned();
            if !n.is_empty() && !out.contains(&n) {
                out.push(n);
            }
        }
        out
    };
    let from_query = unique(q.all("name").into_iter().map(str::to_owned).collect());
    if !from_query.is_empty() {
        return Ok(from_query);
    }
    let body = body.trim_ascii();
    if body.is_empty() {
        return Ok(Vec::new());
    }
    if body[0] == b'[' {
        let names: Vec<String> = serde_json::from_slice(body).map_err(|_| "invalid request body")?;
        return Ok(unique(names));
    }
    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct Names {
        name: String,
        names: Vec<String>,
    }
    let parsed: Names = serde_json::from_slice(body).map_err(|_| "invalid request body")?;
    let mut out = Vec::new();
    if !parsed.name.trim().is_empty() {
        out.push(parsed.name);
    }
    out.extend(parsed.names);
    Ok(unique(out))
}

/// Go `deleteAuthFileByName`. Callers hold `disk` and resync afterwards.
fn delete_one(state: &Management, name: &str) -> Result<String, (StatusCode, String)> {
    let name = name.trim();
    if unsafe_name(name) {
        return Err((StatusCode::BAD_REQUEST, "invalid name".into()));
    }
    let base = Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cfg = state.rt.config();
    let all = state.rt.store().snapshot();
    let found = all
        .iter()
        .find(|c| c.id == name)
        .or_else(|| all.iter().find(|c| file_name(c).as_deref() == Some(name)));
    let target: PathBuf = found
        .and_then(|c| path_of(c).map(Path::to_path_buf))
        .unwrap_or_else(|| cfg.auth_dir.join(&base));
    match std::fs::remove_file(&target) {
        Ok(()) => Ok(base),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err((StatusCode::NOT_FOUND, "auth file not found".into()))
        }
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("failed to remove file: {e}"))),
    }
}

pub(super) async fn delete(State(state): State<Arc<Management>>, RawQuery(raw): RawQuery, body: Bytes) -> Response {
    let q = Query::parse(raw);
    tokio::task::spawn_blocking(move || {
        let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
        let response = delete_sync(&state, &q, &body);
        resync(&state);
        response
    })
    .await
    .unwrap_or_else(|_| fail(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))
}

fn delete_sync(state: &Management, q: &Query, body: &[u8]) -> Response {
    if matches!(q.first("all"), "true" | "1" | "*") {
        let dir = state.rt.config().auth_dir.clone();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                return fail(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to read auth dir: {e}"),
                );
            }
        };
        let mut deleted = 0;
        for entry in entries.flatten() {
            let is_json = entry.file_name().to_string_lossy().to_lowercase().ends_with(".json");
            if entry.file_type().is_ok_and(|t| !t.is_dir()) && is_json && std::fs::remove_file(entry.path()).is_ok() {
                deleted += 1;
            }
        }
        return reply(StatusCode::OK, [("status", "ok".into()), ("deleted", deleted.into())]);
    }
    let names = match delete_names(q, body) {
        Ok(n) => n,
        Err(msg) => return fail(StatusCode::BAD_REQUEST, msg),
    };
    match names.as_slice() {
        [] => fail(StatusCode::BAD_REQUEST, "invalid name"),
        [one] => match delete_one(state, one) {
            Ok(_) => reply(StatusCode::OK, [("status", "ok".into())]),
            Err((status, msg)) => fail(status, msg),
        },
        many => {
            let (mut files, mut failed) = (Vec::new(), Vec::new());
            for name in many {
                match delete_one(state, name) {
                    Ok(base) => files.push(Value::from(base)),
                    Err((_, msg)) => failed.push(h([("name", name.clone().into()), ("error", msg.into())])),
                }
            }
            let count = files.len();
            if failed.is_empty() {
                reply(
                    StatusCode::OK,
                    [
                        ("status", "ok".into()),
                        ("deleted", count.into()),
                        ("files", files.into()),
                    ],
                )
            } else {
                reply(
                    StatusCode::MULTI_STATUS,
                    [
                        ("status", "partial".into()),
                        ("deleted", count.into()),
                        ("files", files.into()),
                        ("failed", failed.into()),
                    ],
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Edits

fn patch_error(e: Option<PatchError>) -> Response {
    match e {
        Some(PatchError::NotFound) => fail(StatusCode::NOT_FOUND, "auth file not found"),
        Some(PatchError::Stale { .. }) => fail(StatusCode::CONFLICT, "stale credential"),
        Some(other) => fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to update auth: {other:?}"),
        ),
        None => fail(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    }
}

/// Go `toggleConfigAPIKeyExcludedAll`: a config API key is disabled by adding `*` to
/// its excluded models. The key's own list is written (group inheritance resolved).
fn toggle_config_key(state: &Management, id: &str, disabled: bool) -> Result<bool, String> {
    let cfg = state.rt.config();
    let Some(credentials::KeyLocation {
        family,
        group: g,
        key: k,
    }) = credentials::config_key_location(&cfg, id)
    else {
        return Ok(false);
    };
    let original = std::fs::read_to_string(&state.path).map_err(|e| e.to_string())?;
    let mut doc = ConfigDocument::parse(&original).map_err(|e| e.to_string())?;
    let basis = doc.migrated_text(&original).unwrap_or_else(|| original.clone());
    let group = doc.get(&["api-keys", family]).and_then(|v| v.get(g)).cloned();
    let key_value = group.as_ref().and_then(|grp| grp.get("keys")?.get(k)).cloned();
    let pick = |v: Option<&serde_yaml_ng::Value>| -> Option<Vec<String>> {
        v.filter(|v| !v.is_null())?
            .as_sequence()
            .map(|s| s.iter().filter_map(|m| m.as_str().map(str::to_owned)).collect())
    };
    let current = pick(key_value.as_ref().and_then(|kv| kv.get("excluded-models")))
        .or_else(|| pick(group.as_ref().and_then(|grp| grp.get("excluded-models"))))
        .unwrap_or_default();
    let mut next: Vec<String> = current.into_iter().filter(|m| m.trim() != "*").collect();
    if disabled {
        next.push("*".into());
    }
    let next = credentials::normalize_excluded(&next);
    let mut keys = group
        .and_then(|grp| grp.get("keys").cloned())
        .and_then(|v| v.as_sequence().cloned())
        .ok_or("config api key entry not found")?;
    let entry = keys
        .get_mut(k)
        .and_then(serde_yaml_ng::Value::as_mapping_mut)
        .ok_or("config api key entry not found")?;
    entry.insert(
        "excluded-models".into(),
        serde_yaml_ng::Value::Sequence(next.into_iter().map(Into::into).collect()),
    );
    // Sequence elements are replaced through their parent list.
    let mut groups = doc
        .get(&["api-keys", family])
        .and_then(|v| v.as_sequence().cloned())
        .ok_or("config api key entry not found")?;
    if let Some(grp) = groups.get_mut(g).and_then(serde_yaml_ng::Value::as_mapping_mut) {
        grp.insert("keys".into(), serde_yaml_ng::Value::Sequence(keys));
    }
    doc.update(&["api-keys", family], serde_yaml_ng::Value::Sequence(groups), false)
        .map_err(|e| e.to_string())?;
    let text = doc.render_preserving(&basis).map_err(|e| e.to_string())?;
    let next_cfg = cpa_core::config::Config::parse(&text).map_err(|e| e.to_string())?;
    ConfigDocument::write(&state.path, &text).map_err(|e| e.to_string())?;
    state.publish(next_cfg, None);
    Ok(true)
}

pub(super) async fn status(State(state): State<Arc<Management>>, body: Bytes) -> Response {
    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct Req {
        name: String,
        auth_index: String,
        disabled: Option<bool>,
    }
    let Ok(req) = serde_json::from_slice::<Req>(&body) else {
        return fail(StatusCode::BAD_REQUEST, "invalid request body");
    };
    let name = req.name.trim().to_owned();
    if name.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "name is required");
    }
    let Some(disabled) = req.disabled else {
        return fail(StatusCode::BAD_REQUEST, "disabled is required");
    };
    let Some(target) = lookup(&state, &name, &req.auth_index) else {
        return fail(StatusCode::NOT_FOUND, "auth file not found");
    };
    tokio::task::spawn_blocking(move || {
        let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(target.source, Source::Config { .. }) {
            return match toggle_config_key(&state, &target.id, disabled) {
                Ok(true) => reply(
                    StatusCode::OK,
                    [
                        ("status", "ok".into()),
                        ("disabled", disabled.into()),
                        ("via", "config:excluded-models".into()),
                        ("excluded_pattern", "*".into()),
                    ],
                ),
                Ok(false) => fail(StatusCode::NOT_FOUND, "config api key entry not found"),
                Err(e) => fail(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to update config api key: {e}"),
                ),
            };
        }
        let patch = MetadataPatch {
            set: Map::from_iter([("disabled".into(), disabled.into())]),
            remove: vec![],
        };
        match state.rt.store().apply_patch(&target.id, target.revision, &patch) {
            Ok(_) => {
                resync(&state);
                reply(StatusCode::OK, [("status", "ok".into()), ("disabled", disabled.into())])
            }
            Err(e) => patch_error(Some(e)),
        }
    })
    .await
    .unwrap_or_else(|_| patch_error(None))
}

const ALIASES: &[(&str, &str)] = &[
    ("api-key", "api_key"),
    ("base-url", "base_url"),
    ("disable-cooling", "disable_cooling"),
    ("excluded-models", "excluded_models"),
    ("fingerprint-profile", "fingerprint_profile"),
    ("model-aliases", "model_aliases"),
    ("proxy-url", "proxy_url"),
    ("request-retry", "request_retry"),
    ("request-scoped-errors", "request_scoped_errors"),
    ("tool-prefix-disabled", "tool_prefix_disabled"),
];

fn canonical(key: &str) -> &str {
    ALIASES.iter().find(|(a, _)| *a == key).map_or(key, |(_, c)| *c)
}

/// Go `PatchAuthFileFields`: dotted metadata paths, `weight`, `request_retry` and a
/// merging `headers` field. The canonical spelling wins over a config-style alias.
pub(super) async fn fields(State(state): State<Arc<Management>>, body: Bytes) -> Response {
    let Ok(req) = serde_json::from_slice::<Map<String, Value>>(&body) else {
        return fail(StatusCode::BAD_REQUEST, "invalid request body");
    };
    let name = req
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    if name.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "name is required");
    }
    let mut fields: Vec<(String, Value)> = Vec::new();
    let mut canonical_flag: Vec<bool> = Vec::new();
    for (key, value) in req.iter().filter(|(k, _)| k.as_str() != "name") {
        let mut parts: Vec<String> = key.trim().split('.').map(|p| p.trim().to_owned()).collect();
        let original_root = parts[0].clone();
        parts[0] = canonical(&original_root).to_owned();
        let path = parts.join(".");
        let is_canonical = original_root == parts[0];
        if let Some(i) = fields.iter().position(|(p, _)| *p == path) {
            if canonical_flag[i] != is_canonical {
                if is_canonical {
                    fields[i].1 = value.clone();
                    canonical_flag[i] = true;
                }
                continue;
            }
            return fail(
                StatusCode::BAD_REQUEST,
                format!("auth file fields {:?} and {key:?} refer to the same field", fields[i].0),
            );
        }
        fields.push((path, value.clone()));
        canonical_flag.push(is_canonical);
    }
    let root = |p: &str| p.split('.').next().unwrap_or_default().trim().to_owned();
    let mut retry: Option<Option<i64>> = None;
    for (path, value) in &fields {
        if root(path) == "request_retry" && path != "request_retry" {
            return fail(StatusCode::BAD_REQUEST, "request_retry does not support nested fields");
        }
        if path == "request_retry" {
            retry = Some(match value {
                Value::Null => None,
                Value::Number(n) => match n.as_i64() {
                    Some(v) if v < 0 => None,
                    Some(v) => Some(v),
                    None => return fail(StatusCode::BAD_REQUEST, "request_retry must be an integer or null"),
                },
                _ => return fail(StatusCode::BAD_REQUEST, "request_retry must be an integer or null"),
            });
        }
    }
    fields.retain(|(p, _)| p != "request_retry");
    let all = state.rt.store().snapshot();
    let Some(target) = all
        .iter()
        .find(|c| c.id == name)
        .or_else(|| all.iter().find(|c| file_name(c).as_deref() == Some(name.as_str())))
        .cloned()
    else {
        return fail(StatusCode::NOT_FOUND, "auth file not found");
    };
    // Start from Go's normalized metadata so aliases persist under canonical names.
    let mut meta = target.metadata.clone();
    for (alias, canon) in ALIASES {
        if let Some(v) = meta.remove(*alias) {
            meta.entry(*canon).or_insert(v);
        }
    }
    let mut changed = false;
    for (path, value) in &fields {
        if path.is_empty() {
            return fail(StatusCode::BAD_REQUEST, "field name is required");
        }
        if path == "weight" {
            if value.is_null() {
                meta.remove("weight");
            } else if !value.is_number() {
                return fail(StatusCode::BAD_REQUEST, "weight must be an integer");
            } else {
                match credentials::parse_weight(value) {
                    Ok(w) => {
                        meta.insert("weight".into(), w.into());
                    }
                    Err(e) => return fail(StatusCode::BAD_REQUEST, e),
                }
            }
        } else if root(path) == "weight" {
            return fail(StatusCode::BAD_REQUEST, "weight does not support nested fields");
        } else if path == "headers" {
            merge_headers(&mut meta, value);
        } else {
            let parts: Vec<&str> = path.split('.').map(str::trim).collect();
            if parts.iter().any(|p| p.is_empty()) {
                return fail(StatusCode::BAD_REQUEST, format!("invalid field path: {path}"));
            }
            set_path(&mut meta, &parts, value.clone());
        }
        changed = true;
    }
    if let Some(retry) = retry {
        match retry {
            None => meta.remove("request_retry"),
            Some(v) => meta.insert("request_retry".into(), v.into()),
        };
        changed = true;
    }
    if !changed {
        return fail(StatusCode::BAD_REQUEST, "no fields to update");
    }
    // Go's file store writes the auth's disabled flag on every save; a patched
    // `disabled` field updates that flag when it parses as a bool.
    let disabled = match meta.get("disabled") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => match s.trim() {
            "1" | "t" | "T" | "TRUE" | "true" | "True" => true,
            "0" | "f" | "F" | "FALSE" | "false" | "False" => false,
            _ => target.disabled,
        },
        _ => target.disabled,
    };
    meta.insert("disabled".into(), disabled.into());
    let mut patch = MetadataPatch::default();
    for key in target.metadata.keys() {
        if !meta.contains_key(key) {
            patch.remove.push(key.clone());
        }
    }
    for (key, value) in meta {
        if target.metadata.get(&key) != Some(&value) {
            patch.set.insert(key, value);
        }
    }
    tokio::task::spawn_blocking(move || {
        let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
        match state.rt.store().apply_patch(&target.id, target.revision, &patch) {
            Ok(_) => {
                resync(&state);
                reply(StatusCode::OK, [("status", "ok".into())])
            }
            Err(e) => patch_error(Some(e)),
        }
    })
    .await
    .unwrap_or_else(|_| patch_error(None))
}

fn set_path(meta: &mut Map<String, Value>, parts: &[&str], value: Value) {
    let (last, parents) = parts.split_last().expect("non-empty path");
    let mut current = meta;
    for part in parents {
        let slot = current.entry(*part).or_insert_with(|| Value::Object(Map::new()));
        if !slot.is_object() {
            *slot = Value::Object(Map::new());
        }
        current = slot.as_object_mut().expect("object");
    }
    current.insert((*last).to_owned(), value);
}

/// Go `applyAuthFileHeadersPatch`: string maps merge (empty values delete); anything
/// else replaces `headers` verbatim.
fn merge_headers(meta: &mut Map<String, Value>, value: &Value) {
    let Some(patch) = value.as_object().filter(|m| m.values().all(Value::is_string)) else {
        meta.insert("headers".into(), value.clone());
        return;
    };
    let mut next: BTreeMap<String, String> = BTreeMap::new();
    if let Some(Value::Object(existing)) = meta.get("headers") {
        for (k, v) in existing {
            if let (k, Some(v)) = (k.trim(), v.as_str().map(str::trim))
                && !k.is_empty()
                && !v.is_empty()
            {
                next.insert(k.to_owned(), v.to_owned());
            }
        }
    }
    for (k, v) in patch {
        let (k, v) = (k.trim(), v.as_str().unwrap_or_default().trim());
        if k.is_empty() {
            continue;
        }
        if v.is_empty() {
            next.remove(k);
        } else {
            next.insert(k.to_owned(), v.to_owned());
        }
    }
    if next.is_empty() {
        meta.remove("headers");
    } else {
        meta.insert(
            "headers".into(),
            Value::Object(next.into_iter().map(|(k, v)| (k, v.into())).collect()),
        );
    }
}

// ---------------------------------------------------------------------------
// Models, cooldowns, refresh

fn definitions() -> &'static Map<String, Value> {
    static DEFS: std::sync::LazyLock<Map<String, Value>> =
        std::sync::LazyLock::new(|| serde_json::from_str(MODEL_DEFINITIONS).expect("embedded model definitions"));
    &DEFS
}

/// Go `GetStaticModelDefinitionsByChannel`.
fn channel_models(channel: &str) -> Option<&'static Vec<Value>> {
    let key = match channel.trim().to_lowercase().as_str() {
        "kimi-ai" | "kimi.ai" | "kimi.com" => "kimi".to_owned(),
        "x-ai" | "grok" => "xai".to_owned(),
        "muse" => "meta".to_owned(),
        other => other.to_owned(),
    };
    definitions().get(&key)?.as_array()
}

pub(super) async fn model_definitions(UrlPath(channel): UrlPath<String>) -> Response {
    let channel = channel.trim().to_owned();
    if channel.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "channel is required");
    }
    match channel_models(&channel) {
        Some(models) => reply(
            StatusCode::OK,
            [
                ("channel", channel.to_lowercase().into()),
                ("models", Value::Array(models.clone())),
            ],
        ),
        None => reply(
            StatusCode::BAD_REQUEST,
            [("error", "unknown channel".into()), ("channel", channel.into())],
        ),
    }
}

fn wildcard(pattern: &str, model: &str) -> bool {
    let re = format!("^{}$", regex::escape(pattern).replace("\\*", ".*"));
    regex::Regex::new(&re).is_ok_and(|re| re.is_match(model))
}

/// Models a credential serves, approximating Go's per-auth registry registration:
/// configured model aliases, else the provider's static channel, minus exclusions.
/// ponytail: Go reads its dynamic registry (OAuth aliases, remote catalogs); the
/// registry overlay belongs to the routes/registry stream.
fn models_for(c: &Credential) -> Vec<Value> {
    let configured: Vec<Value> = c
        .metadata
        .get("model_aliases")
        .and_then(Value::as_array)
        .filter(|_| matches!(c.source, Source::Config { .. }))
        .map(|aliases| {
            aliases
                .iter()
                .filter_map(|a| a.get("alias").and_then(Value::as_str))
                .map(|id| json!({"id": id}))
                .collect()
        })
        .unwrap_or_default();
    if !configured.is_empty() {
        return configured;
    }
    let channel = match c.provider.as_str() {
        "codex" => match c.attributes.get("plan_type").map(|p| p.to_lowercase()).as_deref() {
            Some("free") => "codex-free",
            Some("team") => "codex-team",
            Some("plus") => "codex-plus",
            _ => "codex-pro",
        },
        "gemini-interactions" => "gemini",
        other => other,
    };
    let excluded: Vec<String> = c
        .attributes
        .get("excluded_models")
        .map(|s| s.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    cpa_core::registry::pinned()
        .channel(channel)
        .iter()
        .filter(|m| !excluded.iter().any(|p| wildcard(p, &m.id.to_lowercase())))
        .map(|m| {
            let mut out = Map::new();
            out.insert("id".into(), m.id.clone().into());
            for (from, to) in [
                ("display_name", "display_name"),
                ("type", "type"),
                ("owned_by", "owned_by"),
            ] {
                if let Some(v) = m.raw.get(from).and_then(Value::as_str).filter(|v| !v.is_empty()) {
                    out.insert(to.into(), v.into());
                }
            }
            Value::Object(out)
        })
        .collect()
}

pub(super) async fn models(State(state): State<Arc<Management>>, RawQuery(raw): RawQuery) -> Response {
    let q = Query::parse(raw);
    let name = q.first("name").to_owned();
    if name.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "name is required");
    }
    let all = state.rt.store().snapshot();
    let models = all
        .iter()
        .find(|c| file_name(c).as_deref() == Some(name.as_str()) || c.id == name)
        .map(|c| models_for(c))
        .unwrap_or_default();
    reply(StatusCode::OK, [("models", Value::Array(models))])
}

pub(super) async fn cooldown_reset(State(state): State<Arc<Management>>, body: Bytes) -> Response {
    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct Req {
        auth_index: String,
    }
    let Ok(req) = serde_json::from_slice::<Req>(&body) else {
        return fail(StatusCode::BAD_REQUEST, "invalid request body");
    };
    let index = req.auth_index.trim().to_owned();
    if index.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "auth_index is required");
    }
    let Some(target) = state
        .rt
        .store()
        .snapshot()
        .into_iter()
        .find(|c| credentials::auth_index(c) == index)
    else {
        return fail(StatusCode::NOT_FOUND, "auth not found");
    };
    let mut models = state.rt.store().reset_cooldowns(&target.id);
    if models.is_empty() {
        // Go falls back to the models registered for the credential.
        models = models_for(&target)
            .iter()
            .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect();
    }
    reply(
        StatusCode::OK,
        [
            ("status", "ok".into()),
            ("auth_index", index.into()),
            ("models", models.into()),
        ],
    )
}

/// Go `Auth` JSON, limited to the fields the store holds.
fn auth_json(c: &Credential) -> Value {
    json!({
        "id": c.id,
        "provider": c.provider,
        "label": c.label,
        "status": if c.disabled { "disabled" } else { "active" },
        "disabled": c.disabled,
        "unavailable": false,
        "attributes": c.attributes,
        "metadata": c.metadata,
    })
}

pub(super) async fn refresh(State(state): State<Arc<Management>>, RawQuery(raw): RawQuery, body: Bytes) -> Response {
    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct Req {
        name: String,
        auth_index: String,
        all: bool,
    }
    let mut req = if body.is_empty() {
        Req::default()
    } else {
        match serde_json::from_slice::<Req>(&body) {
            Ok(r) => r,
            Err(e) => return fail(StatusCode::BAD_REQUEST, format!("invalid request body: {e}")),
        }
    };
    let q = Query::parse(raw);
    if q.first("all") == "true" {
        req.all = true;
    }
    if req.name.is_empty() {
        req.name = q.first("name").trim().to_owned();
    }
    if req.auth_index.is_empty() {
        req.auth_index = q.first("auth_index").trim().to_owned();
    }
    if req.all {
        let targets: Vec<Arc<Credential>> = state
            .rt
            .store()
            .snapshot()
            .into_iter()
            .filter(|c| !c.disabled && meta_str(c, "refresh_token").is_some_and(|t| !t.trim().is_empty()))
            .collect();
        let mut results = Vec::with_capacity(targets.len());
        for c in targets {
            let outcome = state.rt.refresh_credential(&c.id).await;
            let mut r = Map::new();
            r.insert("id".into(), c.id.clone().into());
            r.insert("success".into(), outcome.is_ok().into());
            if let Err(e) = outcome {
                r.insert("error".into(), String::from_utf8_lossy(&e.body).into_owned().into());
            }
            results.push(Value::Object(r));
        }
        return reply(StatusCode::OK, [("ok", true.into()), ("results", results.into())]);
    }
    let name = req.name.trim().to_owned();
    if name.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "name or all=true is required");
    }
    let Some(target) = lookup(&state, &name, &req.auth_index) else {
        return fail(StatusCode::NOT_FOUND, "auth file not found");
    };
    match state.rt.refresh_credential(&target.id).await {
        Ok(c) => reply(StatusCode::OK, [("ok", true.into()), ("auth", auth_json(&c))]),
        Err(e) => fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            String::from_utf8_lossy(&e.body).into_owned(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_time_trims_fraction_like_rfc3339nano() {
        use chrono::TimeZone;
        let t = chrono::Utc.timestamp_opt(1_700_000_000, 120_000_000).unwrap();
        assert_eq!(go_time(t), "2023-11-14T22:13:20.12Z");
        let t = chrono::Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        assert_eq!(go_time(t), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn multipart_reads_files_and_fields() {
        let body = b"--b\r\nContent-Disposition: form-data; name=\"x\"; filename=\"a.json\"\r\n\r\n{}\r\n--b\r\nContent-Disposition: form-data; name=\"f\"\r\n\r\nv\r\n--b--\r\n";
        let parts = multipart("multipart/form-data; boundary=b", body).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(
            (parts[0].field.as_str(), parts[0].filename.as_deref()),
            ("x", Some("a.json"))
        );
        assert_eq!(parts[0].data, b"{}");
        assert!(parts[1].filename.is_none());
        assert!(multipart("multipart/form-data", body).is_err());
    }
}
