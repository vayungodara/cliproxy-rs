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
use axum::extract::rejection::BytesRejection;
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use cpa_common::gostr::GoStr;
use cpa_common::json::GoValue;
use cpa_core::config::{ConfigDocument, credentials};
use cpa_core::credential::{Credential, MetadataPatch, Source};
use serde_json::{Map, Value, json};

use super::{Management, json as respond, multipart};
use crate::body_limit::FirstJson;
use crate::runtime::{Commit, PatchError};

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

    pub(super) fn all(&self, key: &str) -> Vec<&str> {
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

pub(super) fn reply(status: StatusCode, pairs: impl IntoIterator<Item = (&'static str, Value)>) -> Response {
    respond(status, &h(pairs))
}

/// [`reply`] for bodies holding Go structs (see [`super::json_ordered`]).
pub(super) fn reply_ordered(status: StatusCode, pairs: impl IntoIterator<Item = (&'static str, Value)>) -> Response {
    super::json_ordered(status, &h(pairs))
}

pub(super) fn fail(status: StatusCode, message: impl Into<String>) -> Response {
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

/// Go `quotaObservationPayloadForProvider`: the latest passive quota snapshot
/// (`observed_at`, `signals`) for providers Go observes; empty signals otherwise.
fn quota_observation(state: &Management, c: &Credential) -> Value {
    let snapshot = match c.provider.trim().go_lower().as_str() {
        "codex" => state.rt.executors.codex.quota().snapshot(&c.id),
        "claude" => state.rt.executors.claude.quota().snapshot(&c.id),
        // Devin's credential snapshot comes from its status refresh.
        cpa_exec::devin::PROVIDER => state.rt.executors.devices.devin.quota(&c.id),
        _ => None,
    };
    match snapshot {
        Some(s) => observation(&s),
        None => json!({"signals": {}}),
    }
}

/// Go `modelQuotaObservationPayload`: the latest snapshot per (canonical) model;
/// empty for providers without observations.
fn model_quota_observations(state: &Management, c: &Credential) -> Map<String, Value> {
    match c.provider.trim().go_lower().as_str() {
        "codex" => state
            .rt
            .executors
            .codex
            .quota()
            .model_snapshots(&c.id)
            .iter()
            .map(|(model, s)| (model.clone(), observation(s)))
            .collect(),
        "claude" => state
            .rt
            .executors
            .claude
            .quota()
            .model_snapshots(&c.id)
            .iter()
            .map(|(model, s)| (model.clone(), observation(s)))
            .collect(),
        _ => Map::new(),
    }
}

fn observation(s: &cpa_exec::codex_quota::Snapshot) -> Value {
    json!({"observed_at": local(s.observed_at), "signals": s.signals})
}

pub(super) fn file_name(c: &Credential) -> Option<String> {
    match &c.source {
        Source::File(p) => p.file_name().map(|n| n.to_string_lossy().into_owned()),
        Source::Config { .. } | Source::Runtime => None,
    }
}

/// Go `isRuntimeOnlyAuth`: an AI Studio browser on the `/v1/ws` relay, with no file.
pub(super) fn runtime_only(c: &Credential) -> bool {
    matches!(c.source, Source::Runtime)
}

/// Go `errPluginVirtualAuth`.
const PLUGIN_VIRTUAL: &str = "plugin virtual auth cannot be modified directly; edit or delete the source auth file";

/// Go `IsPluginVirtualAuth`: one of several credentials a plugin parsed from one file.
fn plugin_virtual(c: &Credential) -> bool {
    c.attributes
        .get("plugin_virtual")
        .is_some_and(|v| v.trim().go_eq_fold("true"))
}

/// The file a plugin-virtual credential was parsed from (Go `virtual_source`, else
/// `path`).
fn virtual_source(c: &Credential) -> &str {
    let attr = |key: &str| c.attributes.get(key).map_or("", |v| v.trim());
    Some(attr("virtual_source"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| attr("path"))
}

/// Go `isPluginVirtualSourceDelete`: a plugin-virtual credential is changed or deleted
/// only through its source file's name.
fn names_virtual_source(name: &str, c: &Credential) -> bool {
    if !plugin_virtual(c) {
        return true;
    }
    let base = |p: &str| Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned());
    match (base(name.trim()), base(virtual_source(c))) {
        (Some(name), Some(source)) => name.go_eq_fold(&source),
        _ => false,
    }
}

/// Go `authFileListName`: the file name, else the credential ID.
fn list_name(c: &Credential) -> String {
    file_name(c).unwrap_or_else(|| c.id.clone())
}

/// When management first saw a runtime-only credential and its current revision.
pub(crate) struct RuntimeSeen {
    revision: u64,
    created: SystemTime,
    updated: SystemTime,
}

/// Go's `CreatedAt`/`UpdatedAt` for a runtime-only credential.
///
/// ponytail: the store keeps no timestamps, so these are when management first saw the
/// credential and when it first saw its current revision (management edits change
/// it); Go stamps the connect time and also moves `UpdatedAt` on every request
/// result. A full fix records both in the store.
fn runtime_times(state: &Management, c: &Credential) -> (SystemTime, SystemTime) {
    let now = SystemTime::now();
    let mut seen = state.runtime_seen.lock().unwrap_or_else(PoisonError::into_inner);
    let entry = seen.entry(c.id.clone()).or_insert(RuntimeSeen {
        revision: c.revision,
        created: now,
        updated: now,
    });
    if entry.revision != c.revision {
        entry.revision = c.revision;
        entry.updated = now;
    }
    (entry.created, entry.updated)
}

pub(super) fn path_of(c: &Credential) -> Option<&Path> {
    match &c.source {
        Source::File(p) => Some(p),
        Source::Config { .. } | Source::Runtime => None,
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
    name.go_lower().ends_with(".json")
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
pub(super) fn parse_time(v: &Value) -> Option<SystemTime> {
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

/// Go `Auth.SIWCExpirationTime` over the `siwc` section: its expiry fields, then its
/// access token's JWT `exp`.
pub(super) fn siwc_expiry(section: &Map<String, Value>) -> Option<SystemTime> {
    expiry_from(section).or_else(|| {
        let token = section.get("access_token")?.as_str().filter(|t| !t.trim().is_empty())?;
        jwt_claims(token)?.get("exp").and_then(parse_time)
    })
}

/// Go `strconv.ParseBool`.
pub(super) fn go_parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
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
    // cliproxy-rs only: the upstream's stated reset when `max-trusted-cooldown` cut it;
    // `retry_at` is then the next check, which may already be due.
    if let Some(recover) = cd.recover_in {
        view.insert(
            "recover_at".into(),
            go_time(chrono::DateTime::<chrono::Utc>::from(SystemTime::now() + recover)).into(),
        );
    }
    if reason == "quota" {
        view.insert("backoff_level".into(), cd.level.into());
    }
    if !credential_quota && (400..=599).contains(&cd.status) && reason != "unknown" {
        view.insert("http_status".into(), cd.status.into());
    }
    Value::Object(view)
}

pub(super) fn recent_requests(activity: &crate::runtime::CredentialActivity) -> Value {
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

/// Go `authFileBoolValue`: a bool, or a string `strconv.ParseBool` accepts.
fn go_bool(v: Option<&Value>) -> Option<bool> {
    match v? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim() {
            "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
            "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Go's `StatusMessage` for a credential the management API disabled.
pub(super) fn status_message(state: &Management, c: &Credential) -> &'static str {
    let noted = state.disabled_via_api.lock().unwrap_or_else(PoisonError::into_inner);
    if c.disabled && noted.contains(&c.id) {
        "disabled via management API"
    } else {
        ""
    }
}

/// Records a disable/enable through the status or fields endpoints (`None`: the
/// patched value was not a boolean, so Go leaves the state alone).
fn note_disabled(state: &Management, id: &str, disabled: Option<bool>) {
    let mut noted = state.disabled_via_api.lock().unwrap_or_else(PoisonError::into_inner);
    match disabled {
        Some(true) => {
            noted.insert(id.to_owned());
        }
        Some(false) => {
            noted.remove(id);
        }
        None => {}
    }
}

/// Go `Auth.AuthKind`: `oauth`, `apikey` or unknown.
pub(super) fn auth_kind(c: &Credential) -> Option<&'static str> {
    cpa_core::registry::dynamic::auth_kind(c)
}

fn int_value(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Go `authFileQuotaContext`: the quota plugin selected per provider key.
type QuotaPlugins = std::collections::HashMap<String, String>;

/// Go `buildAuthFileEntryLocked`. `None` hides the credential (config API keys and
/// disabled credentials whose file is gone).
fn entry(state: &Management, c: &Credential, v8: bool, quota: &QuotaPlugins) -> Option<BTreeMap<&'static str, Value>> {
    let runtime = runtime_only(c);
    // Go `buildAuthFileEntryLocked`: runtime-only auths are listed from memory and
    // hidden while disabled; other credentials need a file path.
    if runtime && c.disabled {
        return None;
    }
    let path = path_of(c);
    if path.is_none() && !runtime {
        return None;
    }
    let store = state.rt.store();
    let now = SystemTime::now();
    let cooldowns = store.cooldowns(&c.id);
    let expired = access_token_expiry(c).is_some_and(|t| t <= now);
    // An ended bounded window (next check due) does not block the credential.
    let credential_cooldown = cooldowns
        .iter()
        .find(|cd| cd.model.is_empty() && !cd.remaining.is_zero());
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
        ("status_message", status_message(state, c).into()),
        ("disabled", c.disabled.into()),
        ("unavailable", unavailable.into()),
        ("runtime_only", runtime.into()),
        ("source", "memory".into()),
        ("size", 0.into()),
    ] {
        e.insert(k, v);
    }
    let activity = store.activity(&c.id);
    e.insert("success", activity.success.into());
    e.insert("failed", activity.failed.into());
    e.insert("recent_requests", recent_requests(&activity));
    e.insert("quota", quota_observation(state, c));
    let model_quotas = model_quota_observations(state, c);
    if !model_quotas.is_empty() {
        e.insert("model_quotas", Value::Object(model_quotas));
    }
    if let Some(plugin_id) = quota
        .get(&cpa_plugin::host::normalize_provider(&c.provider))
        .filter(|id| !id.is_empty())
    {
        e.insert("supports_quota", true.into());
        e.insert("quota_provider", c.provider.clone().into());
        if v8 {
            e.insert("quota_plugin_id", plugin_id.clone().into());
        }
    }
    // Go 0c708401: only the v0 quota route runs a declarative `quota_probe`.
    if !v8 && let Some(probe) = c.metadata.get("quota_probe").filter(|v| !v.is_null()) {
        e.insert("supports_quota", true.into());
        e.insert("quota_probe", super::sorted_keys(probe));
    }
    let email = meta_str(c, "email").map(str::trim).unwrap_or_default();
    if !email.is_empty() {
        e.insert("email", email.into());
    }
    if let Some(project) = meta_str(c, "project_id").map(str::trim).filter(|p| !p.is_empty()) {
        e.insert("project_id", project.into());
    }
    let (account_type, account) = cpa_core::registry::dynamic::account_info(c);
    if !account_type.is_empty() {
        e.insert("account_type", account_type.into());
    }
    if !account.is_empty() {
        e.insert("account", account.into());
    }
    let meta = path.map(std::fs::metadata);
    let modified = meta
        .as_ref()
        .and_then(|m| m.as_ref().ok())
        .and_then(|m| m.modified().ok());
    // ponytail: Go stamps created/updated at synthesis; the store keeps no
    // timestamps, so both report the file's modification time.
    if let Some(t) = modified {
        e.insert("created_at", local(t).into());
        e.insert("updated_at", local(t).into());
    }
    if runtime {
        let (created, updated) = runtime_times(state, c);
        e.insert("created_at", local(created).into());
        e.insert("updated_at", local(updated).into());
        e.insert("modtime", local(updated).into());
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
    if let Some(path) = path {
        e.insert("path", path.display().to_string().into());
    }
    match meta {
        None => {}
        Some(Ok(m)) => {
            e.insert("source", "file".into());
            e.insert("size", m.len().into());
            if let Some(t) = modified {
                e.insert("modtime", local(t).into());
            }
        }
        Some(Err(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            if c.disabled {
                return None;
            }
        }
        Some(Err(_)) => {}
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
            e.insert("id_token", super::sorted_keys(&Value::Object(out)));
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
        Value::String(s) => go_parse_bool(s.trim()),
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
    if v8 {
        super::siwc::summary(c, &mut e);
    }
    Some(e)
}

fn entry_value(e: BTreeMap<&'static str, Value>) -> Value {
    Value::Object(e.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

pub(crate) async fn list(State(state): State<Arc<Management>>, uri: Uri, RawQuery(raw): RawQuery) -> Response {
    let v8 = uri.path().starts_with("/v8/");
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
    let quota = state.rt.plugins().quota_provider_plugin_ids().await;
    let observed = go_time(chrono::Utc::now());
    let all = state.rt.store().snapshot();
    // Forget runtime-only credentials whose relay session ended.
    state
        .runtime_seen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .retain(|id, _| all.iter().any(|c| runtime_only(c) && c.id == *id));
    let mut matching: Vec<Arc<Credential>> = all
        .into_iter()
        .filter(|c| name.is_empty() || c.id.trim() == name || file_name(c).as_deref() == Some(name.as_str()))
        .filter(|c| index.is_empty() || credentials::auth_index(c) == index)
        .collect();
    if !paginated {
        let mut files: Vec<(String, Value)> = matching
            .iter()
            .filter_map(|c| entry(&state, c, v8, &quota).map(|e| (list_name(c), entry_value(e))))
            .collect();
        files.sort_by_key(|(n, _)| n.go_lower());
        return reply_ordered(
            StatusCode::OK,
            [
                ("observed_at", observed.into()),
                ("files", Value::Array(files.into_iter().map(|(_, e)| e).collect())),
            ],
        );
    }
    // Paginated listings count only listable credentials (Go `isAuthFileListable`).
    matching.retain(|c| path_of(c).is_some_and(|p| p.exists() || !c.disabled) || (runtime_only(c) && !c.disabled));
    matching.sort_by(|a, b| {
        let (na, nb) = (list_name(a), list_name(b));
        na.go_lower()
            .cmp(&nb.go_lower())
            .then_with(|| na.cmp(&nb))
            .then_with(|| a.id.cmp(&b.id))
    });
    let total = matching.len();
    let start = ((page - 1).saturating_mul(page_size)).min(total);
    let end = (start + page_size).min(total);
    let files: Vec<Value> = matching[start..end]
        .iter()
        .filter_map(|c| entry(&state, c, v8, &quota).map(entry_value))
        .collect();
    reply_ordered(
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

pub(crate) async fn download(State(state): State<Arc<Management>>, RawQuery(raw): RawQuery) -> Response {
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
pub(super) fn write_file(dst: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = dst.parent().unwrap_or(Path::new("."));
    let name = dst.file_name().unwrap_or_default().to_string_lossy();
    let mut n = 0u32;
    let (tmp, mut file) = loop {
        let tmp = dir.join(format!(".{name}.{}.{n}.upload", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        match options.open(&tmp) {
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

/// An upload ready to write: its path, bytes and credential, and whether it is a Codex
/// record.
type Upload = (PathBuf, Vec<u8>, Option<Credential>, bool);

/// Go `buildAuthFromFileData`'s checks on an upload, and the bytes Go's store writes.
/// An invalid file is the client's error (`Err((true, _))`, Go `errInvalidAuthFile`,
/// 400).
fn prepare_upload(state: &Management, name: &str, data: &[u8]) -> Result<Upload, (bool, String)> {
    let cfg = state.rt.config();
    let base = Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dst = std::path::absolute(cfg.auth_dir.join(&base)).unwrap_or_else(|_| cfg.auth_dir.join(&base));
    let meta = match serde_json::from_slice::<Map<String, Value>>(data) {
        Ok(m) => m,
        Err(e) => return Err((true, format!("invalid auth file: {e}"))),
    };
    // Loading tolerates invalid exclusion rules, so they are rejected here (Go 1a28c61e),
    // after Go's `NormalizeCredentialMetadata`, so an error names the canonical key.
    let mut canonical = meta.clone();
    for (alias, canon) in ALIASES {
        if let Some(v) = canonical.remove(*alias) {
            canonical.entry(*canon).or_insert(v);
        }
    }
    if let (_, Some(e)) = credentials::excluded_models_from_metadata(&canonical) {
        return Err((true, format!("invalid auth file: {e}")));
    }
    let synthesized = credentials::from_file(&cfg, &cfg.auth_dir, &dst, data)
        .map_err(|e| (true, format!("invalid auth file: invalid weight in {base}: {e}")))?;
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
    // Go's fallback auth (no synthesizer claims the file) is registered enabled.
    let disabled = synthesized.as_ref().is_some_and(|c| c.disabled);
    persisted.insert("disabled".into(), disabled.into());
    let bytes = if persisted == uploaded {
        data.to_vec()
    } else {
        serde_json::to_vec(&persisted).map_err(|e| (false, e.to_string()))?
    };
    let provider = synthesized.as_ref().map_or("", |c| c.provider.as_str());
    let codex = codex_record(provider, &canonical);
    Ok((dst, bytes, synthesized, codex))
}

/// [`store_file`] off the async workers, waiting for a Codex file's lock in between.
async fn store(
    state: &Arc<Management>,
    job: impl FnMut(&Management, &mut CodexLock) -> Result<Result<(), (bool, String)>, PathBuf> + Send + 'static,
) -> Result<(), (bool, String)> {
    with_codex_lock(state, job)
        .await
        .unwrap_or_else(|e| Err((false, format!("failed to write file: {e}"))))
}

/// Go `writeAuthFile`: validate ([`prepare_upload`]), then persist. A Codex record is
/// written under its file's credential lock, as Go's store saves it; the outer `Err`
/// names a file whose lock another holder has ([`with_codex_lock`]).
fn store_file(
    state: &Management,
    name: &str,
    data: &[u8],
    v8: bool,
    lock: &mut CodexLock,
) -> Result<Result<(), (bool, String)>, PathBuf> {
    let (dst, bytes, synthesized, codex) = match prepare_upload(state, name, data) {
        Ok(prepared) => prepared,
        Err(e) => return Ok(Err(e)),
    };
    if !codex {
        let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
        if v8 {
            state.oauth.invalidate_siwc(&dst);
        }
        if let Err(e) = write_file(&dst, &bytes) {
            return Ok(Err((false, format!("failed to write file: {e}"))));
        }
        publish_stored(state, dst, &bytes, synthesized);
        return Ok(Ok(()));
    }
    // Go writes the upload as sent, then saves its record under the file's credential
    // lock (`PersistCodexAuthFile`), which re-reads the file: a refresh holding the lock
    // meanwhile patches its new tokens onto the upload, and the save keeps them. A rerun
    // holds the lock its first attempt waited for, after that write.
    if !lock.holds(&dst) {
        let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
        if v8 {
            state.oauth.invalidate_siwc(&dst);
        }
        if let Err(e) = write_file(&dst, data) {
            return Ok(Err((false, format!("failed to write file: {e}"))));
        }
    }
    match lock.hold(&dst) {
        Ok(()) => {}
        Err(PatchError::LockBusy(path)) => return Err(path),
        Err(e) => return Ok(Err((false, format!("failed to write file: {e:?}")))),
    }
    let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
    if v8 {
        state.oauth.invalidate_siwc(&dst);
    }
    // Go's metadata branch: the upload as decoded, the disk's credentials merged in, the
    // record's flag, then `json.Marshal`, written unless the file holds those bytes.
    let disabled = synthesized.as_ref().is_some_and(|c| c.disabled);
    let object = |b: &[u8]| GoValue::parse_f64(b).and_then(|v| v.into_object().ok());
    let mut doc = object(data).unwrap_or_default();
    let existing = std::fs::read(&dst).ok();
    if let Some(disk) = existing.as_deref().and_then(object).filter(|d| !d.is_empty()) {
        merge_codex_from_disk(&disk, &mut doc);
    }
    doc.insert("disabled".into(), GoValue::Bool(disabled));
    normalize_go(&mut doc);
    let bytes = GoValue::Object(doc).marshal();
    // Go's store does not recreate a disabled record's file that is gone.
    let write = existing.as_ref().map_or(!disabled, |old| *old != bytes);
    if write && let Err(e) = write_file(&dst, &bytes) {
        return Ok(Err((false, format!("failed to write file: {e}"))));
    }
    let cfg = state.rt.config();
    let synthesized = match credentials::from_file(&cfg, &cfg.auth_dir, &dst, &bytes) {
        Ok(merged) => merged,
        Err(_) => synthesized,
    };
    publish_stored(state, dst, &bytes, synthesized);
    Ok(Ok(()))
}

/// Publishes an upload stored as `bytes` at `dst`, and records whether a synthesizer
/// claimed it.
fn publish_stored(state: &Management, dst: PathBuf, bytes: &[u8], synthesized: Option<Credential>) {
    {
        let mut fallbacks = state.fallbacks.lock().unwrap_or_else(PoisonError::into_inner);
        if synthesized.is_none() {
            fallbacks.insert(dst);
        } else {
            fallbacks.remove(&dst);
        }
    }
    let written: [u8; 32] = <sha2::Sha256 as sha2::Digest>::digest(bytes).into();
    state.publish_upload((*state.rt.config()).clone(), None, synthesized.map(|c| (c, written)));
}

pub(crate) async fn upload(
    State(state): State<Arc<Management>>,
    uri: Uri,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    read: crate::body_limit::Read,
) -> Response {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    // Go `UploadAuthFileV8`: v8 writes fail a pending SIWC binding of the file.
    let v8 = uri.path().starts_with("/v8/");
    let body = if multipart::is_form(&content_type) {
        match multipart::body(read, &content_type) {
            Ok(body) => body,
            Err(multipart::Failed::Form(status, e)) => {
                let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
                return fail(status, format!("invalid multipart form: {e}"));
            }
            Err(multipart::Failed::Read(rejection)) => {
                let reason = super::read_reason(&rejection);
                return fail(StatusCode::BAD_REQUEST, format!("invalid multipart form: {reason}"));
            }
        }
    } else {
        match read.body {
            Ok(body) => body,
            Err(rejection) => return fail(super::read_status(&rejection), "failed to read body"),
        }
    };
    if multipart::is_form(&content_type) {
        let files = match multipart::form_files(&content_type, &body) {
            Ok(files) => files,
            Err(e) => return fail(StatusCode::BAD_REQUEST, format!("invalid multipart form: {e}")),
        };
        let store_part = async |part: &multipart::FilePart| -> Result<String, (bool, String)> {
            let name = multipart::go_base(part.filename.trim()).to_owned();
            if !ends_json(&name) {
                return Err((true, "file must be .json".into()));
            }
            let (file, data) = (name.clone(), part.data.to_vec());
            store(&state, move |state, lock| store_file(state, &file, &data, v8, lock))
                .await
                .map(|()| name)
        };
        return match files.len() {
            0 => fail(StatusCode::BAD_REQUEST, "no files uploaded"),
            1 => match store_part(&files[0]).await {
                Ok(_) => reply(StatusCode::OK, [("status", "ok".into())]),
                Err((true, msg)) => fail(StatusCode::BAD_REQUEST, msg),
                Err((false, msg)) => fail(StatusCode::INTERNAL_SERVER_ERROR, msg),
            },
            _ => {
                let mut uploaded = Vec::new();
                let mut failed = Vec::new();
                for part in &files {
                    match store_part(part).await {
                        Ok(name) => uploaded.push(Value::from(name)),
                        Err((_, msg)) => {
                            let name = multipart::go_base(&part.filename);
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
    match store(&state, move |state, lock| store_file(state, &name, &body, v8, lock)).await {
        Ok(()) => reply(StatusCode::OK, [("status", "ok".into())]),
        Err((true, msg)) => fail(StatusCode::BAD_REQUEST, msg),
        Err((false, msg)) => fail(StatusCode::INTERNAL_SERVER_ERROR, msg),
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
fn delete_one(state: &Management, name: &str, v8: bool) -> Result<String, (StatusCode, String)> {
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
    // A runtime-only credential has no file. Go falls back to `auth-dir/<name>` and
    // would remove a same-named file there; report the credential as not found instead.
    if found.is_some_and(|c| runtime_only(c)) {
        return Err((StatusCode::NOT_FOUND, "auth file not found".into()));
    }
    if found.is_some_and(|c| !names_virtual_source(name, c)) {
        return Err((StatusCode::CONFLICT, PLUGIN_VIRTUAL.into()));
    }
    let target: PathBuf = found
        .and_then(|c| path_of(c).map(Path::to_path_buf))
        .unwrap_or_else(|| cfg.auth_dir.join(&base));
    if v8 {
        state.oauth.invalidate_siwc(&target);
    }
    match std::fs::remove_file(&target) {
        Ok(()) => {
            // Go `deleteTokenRecord`: remote stores record the removal explicitly.
            state
                .store_delete(&target)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
            Ok(base)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err((StatusCode::NOT_FOUND, "auth file not found".into()))
        }
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("failed to remove file: {e}"))),
    }
}

pub(crate) async fn delete(
    State(state): State<Arc<Management>>,
    uri: Uri,
    RawQuery(raw): RawQuery,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let q = Query::parse(raw);
    // Go reads names from the body only without `all` or `name` in the query.
    let body = match body {
        Ok(body) => body,
        Err(_) if matches!(q.first("all"), "true" | "1" | "*") || !q.first("name").is_empty() => Bytes::new(),
        Err(rejection) if crate::body_limit::is_too_large(&rejection) => {
            return fail(StatusCode::PAYLOAD_TOO_LARGE, crate::body_limit::MESSAGE);
        }
        Err(_) => return fail(StatusCode::BAD_REQUEST, "failed to read body"),
    };
    // Go `DeleteAuthFileV8`.
    let v8 = uri.path().starts_with("/v8/");
    tokio::task::spawn_blocking(move || {
        let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
        let response = delete_sync(&state, &q, &body, v8);
        resync(&state);
        response
    })
    .await
    .unwrap_or_else(|_| fail(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))
}

pub(super) fn delete_sync(state: &Management, q: &Query, body: &[u8], v8: bool) -> Response {
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
            let is_json = entry.file_name().to_string_lossy().go_lower().ends_with(".json");
            if entry.file_type().is_ok_and(|t| !t.is_dir()) && is_json && {
                if v8 {
                    state.oauth.invalidate_siwc(&entry.path());
                }
                std::fs::remove_file(entry.path()).is_ok()
            } {
                if let Err(error) = state.store_delete(&entry.path()) {
                    return fail(StatusCode::INTERNAL_SERVER_ERROR, error);
                }
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
        [one] => match delete_one(state, one, v8) {
            Ok(_) => reply(StatusCode::OK, [("status", "ok".into())]),
            Err((status, msg)) => fail(status, msg),
        },
        many => {
            let (mut files, mut failed) = (Vec::new(), Vec::new());
            for name in many {
                match delete_one(state, name, v8) {
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

/// Go `syncAuthFileMetadataFields`: attributes that mirror the patched metadata roots.
/// Used for runtime-only credentials, which are not re-synthesized after a patch.
fn sync_patched_attributes(c: &mut Credential, roots: &[String]) {
    let touched = |r: &str| roots.iter().any(|x| x == r);
    let text = |c: &Credential, key: &str| c.metadata.get(key).and_then(Value::as_str).map(|s| s.trim().to_owned());
    let flag = go_bool;
    for key in ["prefix", "proxy_url"] {
        if touched(key)
            && let Some(v) = text(c, key)
        {
            c.attributes.insert(key.into(), v);
        }
    }
    if touched("excluded_models") {
        // Go `ApplyAuthExcludedModelsMeta`: only the credential's own rules.
        let (rules, _) = credentials::excluded_models_from_metadata(&c.metadata);
        let mut rules: Vec<String> = rules.iter().map(|r| r.go_lower()).collect();
        rules.sort();
        rules.dedup();
        if rules.is_empty() {
            c.attributes.remove("excluded_models");
        } else {
            c.attributes.insert("excluded_models".into(), rules.join(","));
        }
    }
    if touched("headers") {
        c.attributes.retain(|k, _| !k.starts_with("header:"));
        if let Some(Value::Object(headers)) = c.metadata.get("headers") {
            let pairs: Vec<(String, String)> = headers
                .iter()
                .filter_map(|(k, v)| {
                    let (k, v) = (k.trim(), v.as_str()?.trim());
                    (!k.is_empty() && !v.is_empty()).then(|| (format!("header:{k}"), v.to_owned()))
                })
                .collect();
            c.attributes.extend(pairs);
        }
    }
    if touched("priority") {
        match c.metadata.get("priority").and_then(int_value) {
            Some(0) => {
                c.attributes.remove("priority");
            }
            Some(p) => {
                c.attributes.insert("priority".into(), p.to_string());
            }
            None => {
                c.attributes.remove("priority");
                c.attributes.remove("file_priority");
            }
        }
    }
    if touched("weight") {
        match c.metadata.get("weight").map(credentials::parse_weight) {
            Some(Ok(w)) => {
                c.attributes.insert("weight".into(), w.to_string());
            }
            _ => {
                c.attributes.remove("weight");
            }
        }
    }
    if touched("note") {
        match text(c, "note").filter(|n| !n.is_empty()) {
            Some(note) => {
                c.attributes.insert("note".into(), note);
            }
            None => {
                c.attributes.remove("note");
            }
        }
    }
    if touched("websockets") {
        match flag(c.metadata.get("websockets")) {
            Some(w) => {
                c.attributes.insert("websockets".into(), w.to_string());
            }
            None => {
                c.attributes.remove("websockets");
            }
        }
    }
    if touched("disabled")
        && let Some(d) = flag(c.metadata.get("disabled"))
    {
        c.disabled = d;
    }
}

/// Go `toggleConfigAPIKeyExcludedAll`: a config API key is disabled by adding `*` to
/// its excluded models. The key's own list is written (group inheritance resolved).
fn toggle_config_key(state: &Management, id: &str, disabled: bool) -> Result<bool, String> {
    // Locate the key in the same snapshot that is edited: the file on disk may be
    // newer than the published config (an external edit the watcher has not seen).
    let original = std::fs::read_to_string(&state.path).map_err(|e| e.to_string())?;
    let file_cfg = cpa_core::config::Config::parse(&original).map_err(|e| e.to_string())?;
    let Some(credentials::KeyLocation {
        family,
        group: g,
        key: k,
    }) = credentials::config_key_location(&file_cfg, id)
    else {
        return Ok(false);
    };
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

pub(crate) async fn status(State(state): State<Arc<Management>>, body: Result<FirstJson, BytesRejection>) -> Response {
    let body = match body {
        Ok(FirstJson(body)) => body,
        Err(rejection) => return fail(super::read_status(&rejection), "invalid request body"),
    };
    // gin `ShouldBindJSON` into `struct{ Name, AuthIndex string; Disabled *bool }`:
    // every member in order; any type mismatch fails the bind, even one a later
    // duplicate would fix.
    let Some(members) = super::api_call::bind_struct(&body, &["name", "auth_index", "disabled"]) else {
        return fail(StatusCode::BAD_REQUEST, "invalid request body");
    };
    let (mut name, mut auth_index, mut disabled) = (String::new(), String::new(), None);
    for (field, value) in members {
        let slot = match field {
            0 => &mut name,
            1 => &mut auth_index,
            _ => {
                match value {
                    Value::Bool(b) => disabled = Some(b),
                    Value::Null => disabled = None,
                    _ => return fail(StatusCode::BAD_REQUEST, "invalid request body"),
                }
                continue;
            }
        };
        match value {
            Value::String(s) => *slot = s,
            Value::Null => {}
            _ => return fail(StatusCode::BAD_REQUEST, "invalid request body"),
        }
    }
    let name = name.trim().to_owned();
    if name.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "name is required");
    }
    let Some(disabled) = disabled else {
        return fail(StatusCode::BAD_REQUEST, "disabled is required");
    };
    let Some(target) = lookup(&state, &name, &auth_index) else {
        return fail(StatusCode::NOT_FOUND, "auth file not found");
    };
    if runtime_only(&target) {
        // Go `applyAuthDisabledState` then `Manager.Update`, which never persists a
        // runtime-only auth: the change lives in memory until the relay session ends.
        let mut next = Credential::clone(&target);
        next.disabled = disabled;
        next.metadata.insert("disabled".into(), disabled.into());
        return match state.rt.store().replace_config_backed(next, target.revision) {
            Ok(_) => {
                note_disabled(&state, &target.id, Some(disabled));
                reply(StatusCode::OK, [("status", "ok".into()), ("disabled", disabled.into())])
            }
            Err(e) => patch_error(Some(e)),
        };
    }
    let saved = with_codex_lock(&state, move |state, lock| {
        let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
        if plugin_virtual(&target) {
            // Go: only through the source file's name, which toggles the file and every
            // credential parsed from it; a parsed credential alone cannot be changed.
            if !names_virtual_source(&name, &target) {
                return Ok(fail(StatusCode::CONFLICT, PLUGIN_VIRTUAL));
            }
            return Ok(virtual_source_status(state, &target, disabled));
        }
        if matches!(target.source, Source::Config { .. }) {
            return Ok(match toggle_config_key(state, &target.id, disabled) {
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
            });
        }
        // Go v8.0.35 `Manager.Modify`: the flag lands on the record as it is now, so a
        // token refresh between the lookup and the write does not fail the toggle.
        let mut current = target.clone();
        for _ in 0..16 {
            let mut metadata = current.metadata.clone();
            metadata.insert("disabled".into(), disabled.into());
            match save_file_backed(state, &current, &metadata, &[], lock) {
                Ok(()) => {
                    note_disabled(state, &current.id, Some(disabled));
                    return Ok(reply(
                        StatusCode::OK,
                        [("status", "ok".into()), ("disabled", disabled.into())],
                    ));
                }
                Err(PatchError::LockBusy(path)) => return Err(path),
                Err(PatchError::Stale { .. }) => match state.rt.store().get(&current.id) {
                    Some(c) => current = c,
                    None => return Ok(patch_error(Some(PatchError::NotFound))),
                },
                Err(e) => return Ok(patch_error(Some(e))),
            }
        }
        Ok(patch_error(Some(PatchError::Stale {
            current: current.revision,
        })))
    })
    .await;
    saved.unwrap_or_else(|e| patch_error(Some(PatchError::Io(e))))
}

/// Go `patchPluginVirtualSourceStatus`: writes `disabled` into the source file (Go's
/// `setSourceAuthFileDisabled`: normalized keys, `json.Marshal` form) and sets it on
/// every credential parsed from that file, in memory (the store never writes a
/// plugin-virtual credential's file). The watcher's reload of the file agrees.
fn virtual_source_status(state: &Management, target: &Credential, disabled: bool) -> Response {
    let source = virtual_source(target).to_owned();
    if source.is_empty() {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, PLUGIN_VIRTUAL);
    }
    let data = match std::fs::read(&source) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return fail(StatusCode::NOT_FOUND, "auth file not found");
        }
        Err(e) => {
            return fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to update source auth file: {e}"),
            );
        }
    };
    let mut meta = if data.trim_ascii().is_empty() {
        Map::new()
    } else {
        match serde_json::from_slice::<Map<String, Value>>(&data) {
            Ok(meta) => meta,
            Err(e) => {
                return fail(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to update source auth file: invalid auth file: {e}"),
                );
            }
        }
    };
    // Go `NormalizeCredentialMetadata`: the alias goes, and a canonical spelling already
    // there wins.
    for (alias, canon) in ALIASES {
        if let Some(v) = meta.remove(*alias) {
            meta.entry(*canon).or_insert(v);
        }
    }
    meta.insert("disabled".into(), disabled.into());
    let raw = serde_json::to_vec(&meta)
        .ok()
        .and_then(|b| cpa_common::json::GoValue::parse_f64(&b))
        .map(|v| v.marshal());
    let Some(raw) = raw else {
        return fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to update source auth file: marshal auth file",
        );
    };
    if let Err(e) = write_file(Path::new(&source), &raw) {
        return fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to update source auth file: {e}"),
        );
    }
    let same = |p: &str| {
        let clean = |p: &str| std::path::absolute(p.trim()).ok();
        !p.trim().is_empty() && clean(p) == clean(&source)
    };
    let store = state.rt.store();
    for c in store.snapshot().iter() {
        let path = c.attributes.get("path").map_or("", String::as_str);
        let from = c.attributes.get("virtual_source").map_or("", String::as_str);
        if !same(path) && !same(from) {
            continue;
        }
        let mut patch = MetadataPatch::default();
        patch.set.insert("disabled".into(), disabled.into());
        let mut current = c.clone();
        for _ in 0..16 {
            match store.apply_patch(&current.id, current.revision, &patch) {
                Ok(_) => {
                    note_disabled(state, &current.id, Some(disabled));
                    break;
                }
                Err(PatchError::Stale { .. }) => match store.get(&current.id) {
                    Some(next) => current = next,
                    None => break,
                },
                Err(e) => {
                    return fail(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("failed to update auth {}: {e:?}", current.id),
                    );
                }
            }
        }
    }
    reply(StatusCode::OK, [("status", "ok".into()), ("disabled", disabled.into())])
}

const ALIASES: &[(&str, &str)] = &[
    ("api-key", "api_key"),
    ("base-url", "base_url"),
    ("disable-cooling", "disable_cooling"),
    ("excluded-models", "excluded_models"),
    ("fingerprint-profile", "fingerprint_profile"),
    ("model-aliases", "model_aliases"),
    ("openapi-location", "openapi_location"),
    ("openapi-models", "openapi_models"),
    ("proxy-url", "proxy_url"),
    ("request-retry", "request_retry"),
    ("request-scoped-errors", "request_scoped_errors"),
    ("tool-prefix-disabled", "tool_prefix_disabled"),
];

fn canonical(key: &str) -> &str {
    ALIASES.iter().find(|(a, _)| *a == key).map_or(key, |(_, c)| *c)
}

/// One `fields` member: its canonical path, the value the handlers check and the
/// value as Go decoded it.
struct Field {
    path: String,
    value: Value,
    raw: GoValue,
}

/// `raw` as serde holds it, refused when a number is past f64 or the file would nest
/// past serde's 128 levels: `depth` is the objects its path puts around it, the file's
/// root included, so a scalar alone can reach the limit.
fn serde_value(raw: &GoValue, depth: usize) -> Option<Value> {
    let mut deepest = depth;
    let mut stack = vec![(raw, depth)];
    while let Some((v, d)) = stack.pop() {
        match v {
            GoValue::Array(items) => stack.extend(items.iter().map(|i| (i, d + 1))),
            GoValue::Object(map) => stack.extend(map.values().map(|i| (i, d + 1))),
            _ => continue,
        }
        deepest = deepest.max(d + 1);
    }
    if deepest >= 128 {
        return None;
    }
    serde_json::from_slice(&raw.marshal()).ok()
}

/// Go `PatchAuthFileFields`: dotted metadata paths, `weight`, `request_retry` and a
/// merging `headers` field. The canonical spelling wins over a config-style alias.
pub(crate) async fn fields(State(state): State<Arc<Management>>, body: Result<FirstJson, BytesRejection>) -> Response {
    let body = match body {
        Ok(FirstJson(body)) => body,
        Err(rejection) => return fail(super::read_status(&rejection), "invalid request body"),
    };
    // `json.Decoder.Decode` with `UseNumber` into `map[string]json.RawMessage`: the
    // first JSON value, a later duplicate key replacing an earlier one; null is an
    // empty map. Each value is then decoded into `any`, numbers kept as written.
    let invalid = || fail(StatusCode::BAD_REQUEST, "invalid request body");
    let Some(doc) = crate::body_limit::whole_first_value(&body).and_then(GoValue::parse) else {
        return invalid();
    };
    let req = match doc.into_object() {
        Ok(map) => map,
        Err(GoValue::Null) => BTreeMap::new(),
        Err(_) => return invalid(),
    };
    let name = match req.get("name") {
        Some(GoValue::String(s)) => s.trim().to_owned(),
        _ => String::new(),
    };
    if name.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "name is required");
    }
    let mut fields: Vec<Field> = Vec::new();
    let mut canonical_flag: Vec<bool> = Vec::new();
    for (key, raw) in req.iter().filter(|(k, _)| k.as_str() != "name") {
        let mut parts: Vec<String> = key.trim().split('.').map(|p| p.trim().to_owned()).collect();
        let original_root = parts[0].clone();
        parts[0] = canonical(&original_root).to_owned();
        let path = parts.join(".");
        let is_canonical = original_root == parts[0];
        // ponytail: serde holds the value the handlers check, so a number past f64
        // (`1e400`) or nesting past serde's 128 levels is refused; Go writes both.
        let Some(value) = serde_value(raw, parts.len()) else {
            return fail(StatusCode::BAD_REQUEST, format!("invalid field {path}"));
        };
        let field = Field {
            path,
            value,
            raw: raw.clone(),
        };
        if let Some(i) = fields.iter().position(|f| f.path == field.path) {
            if canonical_flag[i] != is_canonical {
                if is_canonical {
                    fields[i] = field;
                    canonical_flag[i] = true;
                }
                continue;
            }
            return fail(
                StatusCode::BAD_REQUEST,
                format!(
                    "auth file fields {:?} and {key:?} refer to the same field",
                    fields[i].path
                ),
            );
        }
        fields.push(field);
        canonical_flag.push(is_canonical);
    }
    let root = |p: &str| p.split('.').next().unwrap_or_default().trim().to_owned();
    let mut retry: Option<Option<i64>> = None;
    for Field { path, raw, .. } in &fields {
        // Go keeps a `use_siwc` number as its `json.Number`, which `parseBoolAny` reads
        // only through `Int64`: a fraction, an exponent or a value past i64 counts as no
        // flag until the file is loaded again (a float, true when non-zero). Credentials
        // here keep no number literal, so such a write would move the credential version
        // where Go's does not, and the reverse (DIFFERENCES-FROM-GO.md).
        if path == "use_siwc" && matches!(raw, GoValue::Number(n) if n.parse::<i64>().is_err()) {
            return fail(
                StatusCode::BAD_REQUEST,
                "use_siwc must be an integer when it is a number",
            );
        }
        if root(path) == "request_retry" && path != "request_retry" {
            return fail(StatusCode::BAD_REQUEST, "request_retry does not support nested fields");
        }
        if path == "request_retry" {
            // `json.Number.Int64`: the literal as a base-10 integer.
            retry = Some(match raw {
                GoValue::Null => None,
                GoValue::Number(n) => match n.parse::<i64>() {
                    Ok(v) if v < 0 => None,
                    Ok(v) => Some(v),
                    Err(_) => return fail(StatusCode::BAD_REQUEST, "request_retry must be an integer or null"),
                },
                _ => return fail(StatusCode::BAD_REQUEST, "request_retry must be an integer or null"),
            });
        }
    }
    fields.retain(|f| f.path != "request_retry");
    let saved = with_codex_lock(&state, move |state, lock| {
        let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
        // ponytail: a credential removed and re-added under the same ID takes the edit;
        // Go's registration epoch refuses it (404). Bounded so a busy record cannot spin.
        for _ in 0..16 {
            if let Some(done) = apply_fields(state, &name, &fields, retry, lock)? {
                return Ok(done);
            }
        }
        Ok(fail(StatusCode::CONFLICT, "stale credential"))
    })
    .await;
    saved.unwrap_or_else(|e| patch_error(Some(PatchError::Io(e))))
}

/// One attempt of [`fields`] on the credential as it is now; `None` when a concurrent
/// update (a token refresh, say) changed it first, so the caller re-reads and retries.
/// Go v8.0.35 `Manager.Modify` (`d69962e4`) applies the edit to the current record
/// for the same reason instead of failing. `Err` names a Codex file whose lock another
/// holder has ([`with_codex_lock`]).
fn apply_fields(
    state: &Management,
    name: &str,
    fields: &[Field],
    retry: Option<Option<i64>>,
    lock: &mut CodexLock,
) -> Result<Option<Response>, PathBuf> {
    let root = |p: &str| p.split('.').next().unwrap_or_default().trim().to_owned();
    let all = state.rt.store().snapshot();
    let Some(target) = all
        .iter()
        .find(|c| c.id == name)
        .or_else(|| all.iter().find(|c| file_name(c).as_deref() == Some(name)))
        .cloned()
    else {
        return Ok(Some(fail(StatusCode::NOT_FOUND, "auth file not found")));
    };
    // Go d69962e4 and 3e309fb4: config credentials are edited through the config
    // endpoints, not as auth files.
    if matches!(target.source, Source::Config { .. }) {
        return Ok(Some(fail(StatusCode::NOT_FOUND, "auth file not found")));
    }
    if plugin_virtual(&target) {
        return Ok(Some(fail(StatusCode::CONFLICT, PLUGIN_VIRTUAL)));
    }
    // Start from Go's normalized metadata so aliases persist under canonical names.
    let mut meta = target.metadata.clone();
    for (alias, canon) in ALIASES {
        if let Some(v) = meta.remove(*alias) {
            meta.entry(*canon).or_insert(v);
        }
    }
    let mut changed = false;
    // Values Go keeps as decoded (`json.Number` literals), written over the converted
    // metadata when the file is saved.
    let mut literals: Vec<(Vec<String>, GoValue)> = Vec::new();
    for Field { path, value, raw } in fields {
        if path.is_empty() {
            return Ok(Some(fail(StatusCode::BAD_REQUEST, "field name is required")));
        }
        if root(path) == "excluded_models" {
            if path != "excluded_models" {
                return Ok(Some(fail(
                    StatusCode::BAD_REQUEST,
                    "excluded_models does not support nested fields",
                )));
            }
            let strings = value.as_array().is_some_and(|l| l.iter().all(Value::is_string));
            if !value.is_null() && !strings {
                return Ok(Some(fail(
                    StatusCode::BAD_REQUEST,
                    "excluded_models must be a string array or null",
                )));
            }
        }
        if path == "weight" {
            if value.is_null() {
                meta.remove("weight");
            } else if !value.is_number() {
                return Ok(Some(fail(StatusCode::BAD_REQUEST, "weight must be an integer")));
            } else {
                match credentials::parse_weight(value) {
                    Ok(w) => {
                        meta.insert("weight".into(), w.into());
                    }
                    Err(e) => return Ok(Some(fail(StatusCode::BAD_REQUEST, e))),
                }
            }
        } else if root(path) == "weight" {
            return Ok(Some(fail(
                StatusCode::BAD_REQUEST,
                "weight does not support nested fields",
            )));
        } else if path == "headers" {
            if merge_headers(&mut meta, value) {
                // Go stores anything but a string map as decoded.
                literals.push((vec!["headers".to_owned()], raw.clone()));
            }
        } else {
            let parts: Vec<&str> = path.split('.').map(str::trim).collect();
            if parts.iter().any(|p| p.is_empty()) {
                return Ok(Some(fail(
                    StatusCode::BAD_REQUEST,
                    format!("invalid field path: {path}"),
                )));
            }
            set_path(&mut meta, &parts, value.clone());
            literals.push((parts.iter().map(|p| (*p).to_owned()).collect(), raw.clone()));
        }
        changed = true;
    }
    if let Some(retry) = retry {
        match retry {
            None => meta.remove("request_retry"),
            Some(v) => {
                // Go stores the decoded int, exactly.
                literals.push((vec!["request_retry".to_owned()], GoValue::Number(v.to_string())));
                meta.insert("request_retry".into(), v.into())
            }
        };
        changed = true;
    }
    if !changed {
        return Ok(Some(fail(StatusCode::BAD_REQUEST, "no fields to update")));
    }
    let mut roots: Vec<String> = fields.iter().map(|f| root(&f.path)).collect();
    if retry.is_some() {
        roots.push("request_retry".into());
    }
    let disabled_note = if roots.iter().any(|r| r == "disabled") {
        go_bool(meta.get("disabled"))
    } else {
        None
    };
    if matches!(target.source, Source::Runtime) {
        // Go updates runtime-only auths in memory only (no file, no config write).
        let mut next = Credential::clone(&target);
        next.metadata = meta;
        sync_patched_attributes(&mut next, &roots);
        let result = state.rt.store().replace_config_backed(next, target.revision);
        return match result {
            Ok(_) => {
                note_disabled(state, &target.id, disabled_note);
                Ok(Some(reply(StatusCode::OK, [("status", "ok".into())])))
            }
            Err(PatchError::Stale { .. }) => Ok(None),
            Err(e) => Ok(Some(patch_error(Some(e)))),
        };
    }
    // Go's file store writes the auth's disabled flag on every save; a patched
    // `disabled` field updates that flag when it parses as a bool.
    let disabled = go_bool(meta.get("disabled")).unwrap_or(target.disabled);
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
    let mut next = target.metadata.clone();
    patch.apply(&mut next);
    match save_file_backed(state, &target, &next, &literals, lock) {
        Ok(()) => {
            note_disabled(state, &target.id, disabled_note);
            Ok(Some(reply(StatusCode::OK, [("status", "ok".into())])))
        }
        Err(PatchError::LockBusy(path)) => Err(path),
        Err(PatchError::Stale { .. }) => Ok(None),
        Err(e) => Ok(Some(patch_error(Some(e)))),
    }
}

/// Go stores a record under its file's credential lock when its provider or metadata
/// type is `codex` (`PersistCodexAuthFile`), token-less records included.
fn codex_record(provider: &str, metadata: &Map<String, Value>) -> bool {
    provider.trim().go_eq_fold("codex")
        || metadata
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| t.trim().go_eq_fold("codex"))
}

/// The Codex credential lock a management write holds ([`with_codex_lock`]).
#[derive(Default)]
pub(super) struct CodexLock(Option<(PathBuf, std::fs::File)>);

impl CodexLock {
    /// Holds the credential lock of the Codex file at `path` without waiting for it:
    /// [`PatchError::LockBusy`] when another holder has it.
    fn holds(&self, path: &Path) -> bool {
        self.0.as_ref().is_some_and(|(held, _)| held == path)
    }

    pub(super) fn hold(&mut self, path: &Path) -> Result<(), PatchError> {
        if self.holds(path) {
            return Ok(());
        }
        self.0 = None;
        let file = cpa_exec::codex_siwc_bind::open_lock_file(path).map_err(PatchError::Io)?;
        if !cpa_exec::codex_siwc_bind::try_lock(&file).map_err(PatchError::Io)? {
            return Err(PatchError::LockBusy(path.to_owned()));
        }
        self.0 = Some((path.to_owned(), file));
        Ok(())
    }
}

/// Runs `job` on a blocking thread, and again each time it stops at a Codex file whose
/// credential lock another holder has (`Err` with its path): the wait for that lock
/// happens here, off `disk` and the blocking pool, and the next run holds it. A refresh
/// holds the lock through its grants and never takes `disk`, so a write waiting for the
/// lock under `disk` would stall every config write, upload and watcher reload behind
/// it. `Err` here is a wait that failed: shutdown, or a lock file that cannot be opened.
pub(super) async fn with_codex_lock<T: Send + 'static>(
    state: &Arc<Management>,
    mut job: impl FnMut(&Management, &mut CodexLock) -> Result<T, PathBuf> + Send + 'static,
) -> Result<T, String> {
    let mut lock = CodexLock::default();
    // Bounded like the stale-record retries: a lock that keeps changing hands fails.
    for _ in 0..16 {
        let worker = state.clone();
        let (returned, out) = tokio::task::spawn_blocking(move || {
            let out = job(&worker, &mut lock);
            ((job, lock), out)
        })
        .await
        .map_err(|_| "internal error".to_owned())?;
        let path = match out {
            Ok(done) => return Ok(done),
            Err(path) => path,
        };
        job = returned.0;
        drop(returned.1);
        let file = state.rt.lock_codex_file(&path).await?;
        lock = CodexLock(Some((path, file)));
    }
    Err("the credential lock kept changing hands".into())
}

/// Go `Manager.Modify` for an auth file: writes `metadata` and registers the file's
/// re-synthesis in place of `target`, only while `target` is still the current record
/// (`Stale` otherwise, and the caller retries on the record as it is now). Only this
/// credential is re-read; a whole-folder rescan could publish a token a concurrent
/// refresh had already replaced.
///
/// The bytes are Go's `FileTokenStore.Save` (sdk/auth/filestore.go): `json.Marshal`
/// of the metadata as Go holds it and the patched `literals` as decoded, aliases under
/// their canonical names. Go's numbers come from the credential's [`Provenance`]: the
/// document the last save committed, or the float64 reading of the bytes it was loaded
/// from while the file still holds them, with any token refresh's values in place. A
/// non-integer number with neither refuses the save (docs/DIFFERENCES-FROM-GO.md). An equal document leaves the file as it is, and
/// the record is published all the same. Codex records go through
/// `PersistCodexAuthFile`, which keeps the disk's credentials first, under the file's
/// credential lock, which `lock` holds or this takes without waiting
/// ([`PatchError::LockBusy`] when another holder has it). Blocking.
///
/// [`Provenance`]: crate::runtime::Provenance
fn save_file_backed(
    state: &Management,
    target: &Credential,
    metadata: &Map<String, Value>,
    literals: &[(Vec<String>, GoValue)],
    lock: &mut CodexLock,
) -> Result<(), PatchError> {
    let path = path_of(target).ok_or(PatchError::ConfigBacked)?;
    // Go `PersistCodexAuthFile`: a Codex record is read, merged and written under the
    // file's credential lock, so a refresh holding it (here or in another process)
    // lands before this read or after this write, never between.
    if codex_record(&target.provider, metadata) {
        lock.hold(path)?;
    }
    let existing = std::fs::read(path).ok();
    let held: Option<[u8; 32]> = existing
        .as_deref()
        .map(|b| <sha2::Sha256 as sha2::Digest>::digest(b).into());
    let object = |v: Option<GoValue>| v.and_then(|v| v.into_object().ok());
    let provenance = state
        .rt
        .store()
        .provenance(&target.id)
        .filter(|p| p.revision == target.revision);
    let (prior, loaded) = match provenance {
        Some(p) if p.loaded => {
            let mut loaded = p.doc.and_then(|d| GoValue::clone(&d).into_object().ok());
            if let Some(loaded) = loaded.as_mut() {
                normalize_go(loaded);
            }
            (None, loaded.map(GoValue::Object))
        }
        Some(p) if p.doc.is_some() => (p.doc, None),
        Some(p) if Some(p.sha256) == held => {
            let mut loaded = object(existing.as_deref().and_then(GoValue::parse));
            if let Some(loaded) = loaded.as_mut() {
                normalize_go(loaded);
            }
            (None, loaded.map(GoValue::Object))
        }
        _ => (None, None),
    };
    let mut unproven = Vec::new();
    let mut doc = go_map(
        metadata,
        prior.as_deref(),
        loaded.as_ref(),
        &mut Vec::new(),
        &mut unproven,
    );
    for (parts, value) in literals {
        go_set_path(&mut doc, parts, value.clone());
    }
    // A patched value replaces whatever it covers.
    unproven.retain(|at| !literals.iter().any(|(parts, _)| at.starts_with(parts)));
    if !unproven.is_empty() {
        return Err(PatchError::Io(
            "the auth file changed since it was loaded; retry after it reloads".into(),
        ));
    }
    // Go's Save ends with `Metadata["disabled"] = auth.Disabled`: a patched
    // `disabled` never reaches the file as sent.
    if let Some(Value::Bool(disabled)) = metadata.get("disabled") {
        doc.insert("disabled".into(), GoValue::Bool(*disabled));
    }
    let codex = target.provider.go_eq_fold("codex")
        || matches!(doc.get("type"), Some(GoValue::String(t)) if t.trim().go_eq_fold("codex"));
    if codex && let Some(disk) = object(existing.as_deref().and_then(GoValue::parse_f64)).filter(|d| !d.is_empty()) {
        merge_codex_from_disk(&disk, &mut doc);
    }
    normalize_go(&mut doc);
    let doc = GoValue::Object(doc);
    let bytes = doc.marshal();
    // Go compares Codex bytes exactly and other records as decoded documents.
    let unchanged = match &existing {
        Some(old) if codex => *old == bytes,
        Some(old) => GoValue::parse(old).is_some_and(|old| go_json_equal(&old, &doc)),
        None => false,
    };
    // A document the auth loader cannot read would drop the credential, written or not:
    // an equal save still publishes what these bytes synthesize.
    if serde_json::from_slice::<Map<String, Value>>(&bytes).is_err() {
        return Err(PatchError::Io("the saved auth file could not be read back".into()));
    }
    let commit = Commit {
        bytes,
        write: !unchanged,
        disk: Some(held),
        doc: Some(std::sync::Arc::new(doc)),
    };
    commit_bytes(state, target, commit)
}

/// Go's in-memory metadata for `meta`, the record the auth loader read with serde. A
/// number keeps its form in `prior` (the document the record's revision was saved as)
/// or becomes Go's float64 reading of its text in `loaded` (the bytes it was loaded
/// from; `strconv.ParseFloat`, correctly rounded where serde's default parse is not).
/// Without either, an integer is exact and becomes its float64; any other number's
/// path goes to `unproven`.
fn go_map(
    meta: &Map<String, Value>,
    prior: Option<&GoValue>,
    loaded: Option<&GoValue>,
    at: &mut Vec<String>,
    unproven: &mut Vec<Vec<String>>,
) -> BTreeMap<String, GoValue> {
    fn get<'a>(v: Option<&'a GoValue>, key: &str) -> Option<&'a GoValue> {
        match v {
            Some(GoValue::Object(m)) => m.get(key),
            _ => None,
        }
    }
    meta.iter()
        .map(|(k, v)| {
            at.push(k.clone());
            let v = go_value(v, get(prior, k), get(loaded, k), at, unproven);
            at.pop();
            (k.clone(), v)
        })
        .collect()
}

fn go_value(
    v: &Value,
    prior: Option<&GoValue>,
    loaded: Option<&GoValue>,
    at: &mut Vec<String>,
    unproven: &mut Vec<Vec<String>>,
) -> GoValue {
    let float = |text: &str| {
        cpa_common::json::json_float(cpa_common::json::parse_float(text.as_bytes())).unwrap_or_else(|| text.to_owned())
    };
    match v {
        Value::Null => GoValue::Null,
        Value::Bool(b) => GoValue::Bool(*b),
        Value::String(s) => GoValue::String(s.clone()),
        Value::Number(n) => {
            // The loader read the same text with the same parser, so a source number at
            // this path agrees with `n` unless the patch changed it.
            let same = |text: &str| serde_json::from_str::<Value>(text).ok().and_then(|t| t.as_f64()) == n.as_f64();
            GoValue::Number(match (prior, loaded) {
                (Some(GoValue::Number(form)), _) if same(form) => form.clone(),
                (_, Some(GoValue::Number(text))) if same(text) => float(text),
                _ => {
                    if !(n.is_i64() || n.is_u64()) {
                        unproven.push(at.clone());
                    }
                    float(&n.to_string())
                }
            })
        }
        Value::Array(items) => {
            fn get(v: Option<&GoValue>, i: usize) -> Option<&GoValue> {
                match v {
                    Some(GoValue::Array(items)) => items.get(i),
                    _ => None,
                }
            }
            GoValue::Array(
                items
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        at.push(i.to_string());
                        let v = go_value(v, get(prior, i), get(loaded, i), at, unproven);
                        at.pop();
                        v
                    })
                    .collect(),
            )
        }
        Value::Object(map) => GoValue::Object(go_map(map, prior, loaded, at, unproven)),
    }
}

/// Go `jsonEqual` (sdk/auth/filestore.go): documents compared as `json.Unmarshal`
/// decodes them, numbers as float64, so `0` equals `-0`.
fn go_json_equal(a: &GoValue, b: &GoValue) -> bool {
    let num = |n: &str| cpa_common::json::parse_float(n.as_bytes());
    match (a, b) {
        (GoValue::Number(x), GoValue::Number(y)) => num(x) == num(y),
        (GoValue::Object(x), GoValue::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| go_json_equal(v, w)))
        }
        (GoValue::Array(x), GoValue::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(v, w)| go_json_equal(v, w))
        }
        _ => a == b,
    }
}

/// Go `NormalizeCredentialMetadata`: aliases move to their canonical key unless it is
/// already set.
fn normalize_go(doc: &mut BTreeMap<String, GoValue>) {
    for (alias, canon) in ALIASES {
        if let Some(v) = doc.remove(*alias) {
            doc.entry((*canon).to_owned()).or_insert(v);
        }
    }
}

/// [`set_path`] on Go's decoded model.
fn go_set_path(doc: &mut BTreeMap<String, GoValue>, parts: &[String], value: GoValue) {
    let (last, parents) = parts.split_last().expect("non-empty path");
    let mut current = doc;
    for part in parents {
        let slot = current
            .entry(part.clone())
            .or_insert_with(|| GoValue::Object(BTreeMap::new()));
        if !matches!(slot, GoValue::Object(_)) {
            *slot = GoValue::Object(BTreeMap::new());
        }
        let GoValue::Object(next) = slot else {
            unreachable!("made an object above")
        };
        current = next;
    }
    current.insert(last.clone(), value);
}

/// Go `codex.MergeCodexCredentialsFromDisk` (internal/auth/codex/token.go): a disk
/// file with an OAuth login keeps its credential fields; otherwise they only fill
/// blanks. The disk's `siwc` always wins, its `use_siwc` when the save has none.
fn merge_codex_from_disk(disk: &BTreeMap<String, GoValue>, doc: &mut BTreeMap<String, GoValue>) {
    const OUTER: [&str; 8] = [
        "id_token",
        "access_token",
        "refresh_token",
        "account_id",
        "last_refresh",
        "email",
        "expired",
        "plan_type",
    ];
    let text = |m: &BTreeMap<String, GoValue>, k: &str| match m.get(k) {
        Some(GoValue::String(s)) => Some(s.trim().is_empty()),
        _ => None,
    };
    let login = ["refresh_token", "last_refresh"]
        .iter()
        .any(|k| text(disk, k) == Some(false));
    let had_use_siwc = doc.contains_key("use_siwc");
    for key in OUTER {
        match disk.get(key) {
            Some(v) if login || matches!(doc.get(key), None | Some(GoValue::Null)) || text(doc, key) == Some(true) => {
                doc.insert(key.to_owned(), v.clone());
            }
            None if login => {
                doc.remove(key);
            }
            _ => {}
        }
    }
    if let Some(v) = disk.get("siwc") {
        doc.insert("siwc".into(), v.clone());
    }
    if let Some(v) = disk.get("use_siwc").filter(|_| !had_use_siwc) {
        doc.insert("use_siwc".into(), v.clone());
    }
}

/// Writes `bytes` as `target`'s file and registers their re-synthesis in its place,
/// while `target` is still the current record (see [`save_file_backed`]).
pub(super) fn save_bytes(state: &Management, target: &Credential, bytes: Vec<u8>) -> Result<(), PatchError> {
    let commit = Commit {
        bytes,
        write: true,
        disk: None,
        doc: None,
    };
    commit_bytes(state, target, commit)
}

/// Commits `commit` for `target`, publishing the credential its bytes synthesize.
fn commit_bytes(state: &Management, target: &Credential, commit: Commit) -> Result<(), PatchError> {
    let Some(path) = path_of(target).map(Path::to_path_buf) else {
        return Err(PatchError::ConfigBacked);
    };
    // Synthesized as the auth-folder scan sees the file.
    let cfg = state.rt.config();
    let scanned = path
        .file_name()
        .map_or_else(|| path.clone(), |name| cfg.auth_dir.join(name));
    let (next, fallback) = match credentials::from_file(&cfg, &cfg.auth_dir, &scanned, &commit.bytes) {
        Ok(Some(c)) => (Some(c), false),
        Ok(None) => (
            credentials::upload_fallback(&cfg.auth_dir, &scanned, &commit.bytes),
            true,
        ),
        Err(e) => return Err(PatchError::Io(e)),
    };
    let kept = next.is_some();
    state
        .rt
        .store()
        .commit_file_backed(&target.id, target.revision, commit, next)?;
    let mut fallbacks = state.fallbacks.lock().unwrap_or_else(PoisonError::into_inner);
    fallbacks.remove(&path);
    fallbacks.remove(&scanned);
    if fallback && kept {
        fallbacks.insert(scanned);
    }
    Ok(())
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
/// else replaces `headers` verbatim, and the result is `true`.
fn merge_headers(meta: &mut Map<String, Value>, value: &Value) -> bool {
    let Some(patch) = value.as_object().filter(|m| m.values().all(Value::is_string)) else {
        meta.insert("headers".into(), value.clone());
        return true;
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
    false
}

// ---------------------------------------------------------------------------
// Models, cooldowns, refresh

/// A model as Go's `registry.ModelInfo` decodes and encodes it
/// ([`cpa_core::registry::go_model_json`]). Catalog refreshes refuse a catalog Go's
/// decode refuses, and registered models are typed, so nothing is dropped here.
fn go_model_info(raw: &Map<String, Value>) -> Option<Value> {
    cpa_core::registry::go_model_json(raw).ok()
}

/// Go `GetStaticModelDefinitions`: the built-in catalog for `channel`, else (v8.0.35,
/// `50cfd238`) a plugin's static catalog for that provider, else the models its
/// credentials registered, sorted by ID.
pub(crate) async fn model_definitions(
    State(state): State<Arc<Management>>,
    UrlPath(channel): UrlPath<String>,
) -> Response {
    let channel = channel.trim().to_owned();
    if channel.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "channel is required");
    }
    let models = match cpa_core::registry::dynamic::static_definitions(&channel) {
        Some(models) if !models.is_empty() => models.iter().filter_map(|m| go_model_info(&m.raw)).collect(),
        _ => dynamic_definitions(&state, &channel),
    };
    if models.is_empty() {
        return reply(
            StatusCode::BAD_REQUEST,
            [("error", "unknown channel".into()), ("channel", channel.into())],
        );
    }
    reply_ordered(
        StatusCode::OK,
        [("channel", channel.go_lower().into()), ("models", Value::Array(models))],
    )
}

/// Go `pluginModelDefinitionsByChannel`, each model as `registry.ModelInfo` encodes.
fn dynamic_definitions(state: &Management, channel: &str) -> Vec<Value> {
    let plugin = state.rt.plugins().models_for_provider(channel);
    if !plugin.is_empty() {
        return plugin.iter().map(plugin_model_json).collect();
    }
    let rt = &state.rt;
    let mut models: Vec<_> = rt
        .registry()
        .available_by_provider(channel, |client, model| rt.suspension(client, model))
        .iter()
        .map(|spec| spec.to_info().raw)
        .collect();
    models.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    models.iter().filter_map(go_model_info).collect()
}

/// Go `pluginModelInfoToRegistryModelInfo` then `json.Marshal`: struct order, with
/// Go's `omitempty` fields left out when zero.
fn plugin_model_json(m: &cpa_plugin::api::ModelInfo) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), m.id.clone().into());
    out.insert("object".into(), m.object.clone().into());
    out.insert("created".into(), m.created.into());
    out.insert("owned_by".into(), m.owned_by.clone().into());
    out.insert("type".into(), m.model_type.clone().into());
    let mut text = |key: &str, v: &str| {
        if !v.is_empty() {
            out.insert(key.into(), v.into());
        }
    };
    text("display_name", &m.display_name);
    text("name", &m.name);
    text("version", &m.version);
    text("description", &m.description);
    let numbers = [
        ("inputTokenLimit", m.input_token_limit),
        ("outputTokenLimit", m.output_token_limit),
    ];
    for (key, v) in numbers {
        if v != 0 {
            out.insert(key.into(), v.into());
        }
    }
    let list = |out: &mut Map<String, Value>, key: &str, v: &[String]| {
        if !v.is_empty() {
            out.insert(key.into(), v.into());
        }
    };
    list(&mut out, "supportedGenerationMethods", &m.supported_generation_methods);
    for (key, v) in [
        ("context_length", m.context_length),
        ("max_completion_tokens", m.max_completion_tokens),
    ] {
        if v != 0 {
            out.insert(key.into(), v.into());
        }
    }
    list(&mut out, "supported_parameters", &m.supported_parameters);
    list(&mut out, "supportedInputModalities", &m.supported_input_modalities);
    list(&mut out, "supportedOutputModalities", &m.supported_output_modalities);
    if let Some(t) = &m.thinking {
        let mut thinking = Map::new();
        for (key, v) in [("min", t.min), ("max", t.max)] {
            if v != 0 {
                thinking.insert(key.into(), v.into());
            }
        }
        if t.zero_allowed {
            thinking.insert("zero_allowed".into(), true.into());
        }
        if t.dynamic_allowed {
            thinking.insert("dynamic_allowed".into(), true.into());
        }
        if !t.disable_type.is_empty() {
            thinking.insert("disable_type".into(), t.disable_type.clone().into());
        }
        list(&mut thinking, "levels", &t.levels);
        out.insert("thinking".into(), Value::Object(thinking));
    }
    Value::Object(out)
}

/// Go `GetModelsForClient`: the models the credential registers (the dynamic
/// registry's `models_for`), in registration order, rendered like
/// `GetAuthFileModels`.
fn models_for(state: &Management, c: &Credential) -> Vec<Value> {
    let cfg = state.rt.config();
    let aliases = crate::registry::global_aliases(&cfg);
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let mut seen = std::collections::HashSet::new();
    crate::registry::models_for(&cfg, &aliases, c, now)
        .into_iter()
        .filter(|m| !m.id.is_empty() && seen.insert(m.id.clone()))
        .map(|m| {
            let mut out = Map::new();
            out.insert("id".into(), m.id.into());
            for (key, value) in [
                ("display_name", m.display_name),
                ("type", m.kind),
                ("owned_by", m.owned_by),
            ] {
                if !value.is_empty() {
                    out.insert(key.into(), value.into());
                }
            }
            Value::Object(out)
        })
        .collect()
}

pub(crate) async fn models(State(state): State<Arc<Management>>, RawQuery(raw): RawQuery) -> Response {
    let q = Query::parse(raw);
    let name = q.first("name").to_owned();
    if name.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "name is required");
    }
    let all = state.rt.store().snapshot();
    let models = all
        .iter()
        .find(|c| file_name(c).as_deref() == Some(name.as_str()) || c.id == name)
        .map(|c| models_for(&state, c))
        .unwrap_or_default();
    reply(StatusCode::OK, [("models", Value::Array(models))])
}

pub(crate) async fn cooldown_reset(
    State(state): State<Arc<Management>>,
    body: Result<FirstJson, BytesRejection>,
) -> Response {
    let body = match body {
        Ok(FirstJson(body)) => body,
        Err(rejection) => return fail(super::read_status(&rejection), "invalid request body"),
    };
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
        models = models_for(&state, &target)
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

pub(crate) async fn refresh(
    State(state): State<Arc<Management>>,
    RawQuery(raw): RawQuery,
    body: Result<FirstJson, BytesRejection>,
) -> Response {
    let body = match body {
        Ok(FirstJson(body)) => body,
        Err(rejection) => {
            let reason = super::read_reason(&rejection);
            return fail(
                super::read_status(&rejection),
                format!("invalid request body: {reason}"),
            );
        }
    };
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
            .filter(|c| {
                !c.disabled
                    && (meta_str(c, "refresh_token").is_some_and(|t| !t.trim().is_empty())
                        // Go `authHasRefreshCredential`'s SIWC clause; Codex only until the
                        // Claude replay, as in `Executors::has_refresh_credential`.
                        || c.provider == "codex"
                            && cpa_exec::codex_siwc::refresh_due(c, chrono::Utc::now()).is_some())
            })
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

    /// `registry.ModelInfo`'s JSON tags: struct order, `omitempty` zeros dropped, and
    /// the identity fields always present.
    #[test]
    fn plugin_models_encode_like_go_registry_model_info() {
        use cpa_plugin::api::{ModelInfo, ThinkingSupport};
        let model = ModelInfo {
            id: "work-pro".into(),
            display_name: "Work Pro".into(),
            input_token_limit: 1000,
            supported_generation_methods: vec!["generateContent".into()],
            max_completion_tokens: 64,
            supported_input_modalities: vec!["TEXT".into()],
            thinking: Some(ThinkingSupport {
                max: 2048,
                dynamic_allowed: true,
                ..Default::default()
            }),
            user_defined: true,
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&plugin_model_json(&model)).unwrap(),
            r#"{"id":"work-pro","object":"","created":0,"owned_by":"","type":"","display_name":"Work Pro","inputTokenLimit":1000,"supportedGenerationMethods":["generateContent"],"max_completion_tokens":64,"supportedInputModalities":["TEXT"],"thinking":{"max":2048,"dynamic_allowed":true}}"#
        );
    }

    fn management(name: &str, files: &[(&str, &str)]) -> (Arc<Management>, std::path::PathBuf) {
        management_with(name, files, |_| {})
    }

    /// [`management`], with `edit` applied to the loaded credentials first.
    fn management_with(
        name: &str,
        files: &[(&str, &str)],
        edit: impl FnOnce(&mut Vec<Credential>),
    ) -> (Arc<Management>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("cpa-modify-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        for (file, body) in files {
            std::fs::write(dir.join(file), body).unwrap();
        }
        let path = dir.join("config.yaml");
        std::fs::write(&path, format!("oauth: {{auth-dir: {}}}\n", dir.display())).unwrap();
        let cfg = cpa_core::config::Config::load(&path).unwrap();
        let mut scan = credentials::scan_auth_dir(&cfg);
        edit(&mut scan.credentials);
        let rt = Arc::new(crate::testing::runtime(
            cfg.clone(),
            scan.credentials,
            cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        // As the binary records what it loaded.
        rt.store().note_loaded(&scan.read);
        (Management::new(rt, path), dir)
    }

    /// A refresh that started before management edits only the credential's `use_siwc`
    /// commits, as in Go, whose credential version stays put: a number Go would hold
    /// unparsed (`json.Number` past `Int64`) is refused before anything is written, and
    /// clearing an absent flag changes nothing Go compares.
    #[tokio::test]
    async fn a_use_siwc_edit_go_cannot_parse_leaves_a_refresh_in_flight_alone() {
        let body = r#"{"type":"pp","access_token":"q","refresh_token":"r"}"#;
        let (state, dir) = management("use-siwc-number", &[("a.json", body)]);
        let store = state.rt.store();
        let (current, registered) = store.get_registered("a.json").unwrap();
        let mut codes = Vec::new();
        for value in ["1.5", "1e0", "10000000000000000000", "null"] {
            let r = fields(
                State(state.clone()),
                Ok(FirstJson(Bytes::from(format!(
                    r#"{{"name":"a.json","use_siwc":{value}}}"#
                )))),
            )
            .await;
            codes.push((value, r.status()));
        }
        let mut patch = MetadataPatch::default();
        patch.set.insert("access_token".into(), "p2".into());
        patch.set.insert("refresh_token".into(), "r2".into());
        let fence = crate::runtime::RefreshFence {
            registered,
            plugin: true,
        };
        let committed = crate::runtime::commit_refresh(store, "a.json", &current, fence, &patch, |_| {}).unwrap();
        assert_eq!(committed.metadata["refresh_token"], "r2", "the refresh commits");
        let on_disk: Value = serde_json::from_slice(&std::fs::read(dir.join("a.json")).unwrap()).unwrap();
        assert_eq!(on_disk["refresh_token"], "r2");
        assert!(store.refresh_schedules().lock().unwrap()["a.json"].last.is_some());
        assert_eq!(
            codes,
            [
                ("1.5", StatusCode::BAD_REQUEST),
                ("1e0", StatusCode::BAD_REQUEST),
                ("10000000000000000000", StatusCode::BAD_REQUEST),
                ("null", StatusCode::OK),
            ]
        );
        // An integer parses in Go as in the file it is loaded from.
        let r = fields(
            State(state.clone()),
            Ok(FirstJson(Bytes::from(r#"{"name":"a.json","use_siwc":-0}"#))),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The two management paths that disable and enable a plugin-parsed credential
    /// register it without parsing its file again (DIFFERENCES-FROM-GO.md): a status
    /// write re-synthesizes it as a built-in credential, without a parsed
    /// `NextRefreshAfter`, while a patch (a plugin-virtual source's status) keeps the one
    /// its last parse gave. Go's watcher parses the written file and takes that parse's.
    #[tokio::test]
    async fn management_status_changes_register_plugin_deadlines_without_a_parse() {
        use chrono::TimeZone;
        let parsed = chrono::Utc.with_ymd_and_hms(2099, 1, 1, 0, 0, 0).unwrap();
        let body = r#"{"type":"pp","access_token":"t","refresh_interval":"1h"}"#;
        let (state, dir) = management_with("plugin-deadlines", &[("a.json", body), ("b.json", body)], |creds| {
            for c in creds {
                c.next_refresh_after = Some(parsed.into());
            }
        });
        let store = state.rt.store();
        store.end_bootstrap();
        let next_after = |id: &str| {
            store
                .refresh_schedules()
                .lock()
                .unwrap()
                .get(id)
                .map(|s| (s.last.is_some(), s.next_after))
        };
        assert_eq!(next_after("a.json"), Some((false, Some(parsed))));
        let now = chrono::Utc::now();
        for id in ["a.json", "b.json"] {
            let registration = store.get_registered(id).unwrap().1.registration;
            store
                .record_refresh(id, registration, None, |c, s| {
                    crate::plugins::refresh::committed(c, s, now)
                })
                .unwrap();
        }
        for disabled in [true, false] {
            let r = status(
                State(state.clone()),
                Ok(FirstJson(Bytes::from(format!(
                    r#"{{"name":"a.json","disabled":{disabled}}}"#
                )))),
            )
            .await;
            assert_eq!(r.status(), StatusCode::OK);
            assert_eq!(store.get("a.json").unwrap().disabled, disabled);
            assert_eq!(next_after("a.json"), Some((false, None)), "status disabled={disabled}");
            let mut patch = MetadataPatch::default();
            patch.set.insert("disabled".into(), disabled.into());
            store
                .apply_patch("b.json", store.get("b.json").unwrap().revision, &patch)
                .unwrap();
            assert_eq!(
                next_after("b.json"),
                Some((false, Some(parsed))),
                "patch disabled={disabled}"
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// What a token refresh commits: the store's compare-and-swap patch.
    fn refresh(state: &Management, id: &str, token: &str) {
        let current = state.rt.store().get(id).unwrap();
        let patch = MetadataPatch {
            set: Map::from_iter([("access_token".into(), token.into())]),
            remove: vec![],
        };
        state.rt.store().apply_patch(id, current.revision, &patch).unwrap();
    }

    fn on_disk(dir: &std::path::Path, file: &str) -> Value {
        serde_json::from_slice(&std::fs::read(dir.join(file)).unwrap()).unwrap()
    }

    /// Go `PersistCodexAuthFile`: a fields patch of a Codex file whose credential lock
    /// another process holds (its refresh) waits for it, then reads the rotated file and
    /// keeps its tokens beside the edit. It waits holding neither `disk` nor a blocking
    /// thread, so config writes, uploads and reloads go on meanwhile.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_codex_save_waits_for_the_credential_lock() {
        let (state, dir) = management(
            "codex-lock",
            &[(
                "codex.json",
                r#"{"type":"codex","access_token":"A","refresh_token":"rA","disabled":false}"#,
            )],
        );
        let path = dir.join("codex.json");
        let lock = cpa_exec::codex_siwc_bind::lock_file(&path).unwrap();
        let body = Bytes::from(r#"{"name":"codex.json","priority":7}"#);
        let save = tokio::spawn(fields(State(state.clone()), Ok(FirstJson(body))));
        crate::runtime::tests::lock_waited(&path).await;
        assert!(state.disk.try_lock().is_ok(), "the waiting save does not hold disk");
        assert!(!save.is_finished(), "the save waits for the lock");
        // The other process's refresh, under its lock.
        std::fs::write(
            &path,
            r#"{"type":"codex","access_token":"C","refresh_token":"rC","disabled":false}"#,
        )
        .unwrap();
        drop(lock);
        let saved = tokio::time::timeout(std::time::Duration::from_secs(5), save)
            .await
            .expect("the save goes on once the lock is free")
            .unwrap();
        assert_eq!(saved.status(), StatusCode::OK);
        let disk = on_disk(&dir, "codex.json");
        assert_eq!(
            (&disk["access_token"], &disk["refresh_token"], &disk["priority"]),
            (&"C".into(), &"rC".into(), &7.into())
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A Codex save waiting for a credential lock another process holds gives up at
    /// shutdown, so the exit does not wait on that process, and leaves the file alone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_codex_save_stops_waiting_for_the_lock_at_shutdown() {
        let original = r#"{"type":"codex","access_token":"A","disabled":false}"#;
        let (state, dir) = management("codex-lock-stop", &[("codex.json", original)]);
        let path = dir.join("codex.json");
        let held = cpa_exec::codex_siwc_bind::lock_file(&path).unwrap();
        let body = Bytes::from(r#"{"name":"codex.json","disabled":true}"#);
        let save = tokio::spawn(status(State(state.clone()), Ok(FirstJson(body))));
        crate::runtime::tests::lock_waited(&path).await;
        state.rt.shutdown_codex_refreshes().await;
        let saved = tokio::time::timeout(std::time::Duration::from_secs(5), save)
            .await
            .expect("the save stops waiting")
            .unwrap();
        assert_eq!(saved.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(saved.into_body(), usize::MAX).await.unwrap();
        assert!(
            String::from_utf8_lossy(&body).contains("shutting down while waiting for the credential lock"),
            "{body:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(!state.rt.store().get("codex.json").unwrap().disabled);
        drop(held);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Go writes a Codex upload as sent, then saves its record through its store under
    /// the file's credential lock (leaving `<name>.lock`), waiting for another holder;
    /// other uploads take none. A refresh holding the lock meanwhile rotates the tokens
    /// in the uploaded file, and the save keeps them: the spent refresh token does not
    /// come back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_codex_upload_waits_for_the_credential_lock() {
        let (state, dir) = management("codex-upload-lock", &[]);
        let path = dir.join("up-codex.json");
        let held = cpa_exec::codex_siwc_bind::lock_file(&path).unwrap();
        let sent = br#"{"type":"codex","access_token":"a1","refresh_token":"r1","priority":7}"#;
        let data = sent.to_vec();
        let upload = tokio::spawn({
            let state = state.clone();
            async move { store(&state, move |s, l| store_file(s, "up-codex.json", &data, true, l)).await }
        });
        crate::runtime::tests::lock_waited(&path).await;
        assert!(state.disk.try_lock().is_ok(), "the waiting upload does not hold disk");
        assert_eq!(std::fs::read(&path).unwrap(), sent, "written as sent before the lock");
        // The refresh holding the lock patches its grant onto the uploaded file.
        std::fs::write(
            &path,
            r#"{"type":"codex","access_token":"a2","refresh_token":"r2","priority":7}"#,
        )
        .unwrap();
        drop(held);
        tokio::time::timeout(std::time::Duration::from_secs(5), upload)
            .await
            .expect("the upload goes on once the lock is free")
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"access_token":"a2","disabled":false,"priority":7,"refresh_token":"r2","type":"codex"}"#
        );
        let stored = state.rt.store().get("up-codex.json").unwrap();
        assert_eq!(stored.str("refresh_token"), Some("r2"));
        let data = br#"{"type":"claude","access_token":"B"}"#.to_vec();
        store(&state, move |s, l| store_file(s, "up-claude.json", &data, true, l))
            .await
            .unwrap();
        assert!(dir.join("up-codex.json.lock").exists());
        assert!(!dir.join("up-claude.json.lock").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Go's store leaves a file whose document is unchanged alone (`jsonEqual`, numbers
    /// as float64): no new inode or mtime, and the record is published all the same.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unchanged_save_writes_nothing() {
        use std::os::unix::fs::MetadataExt;
        let (state, dir) = management(
            "unchanged",
            &[("a.json", "{\"type\":\"claude\",\"disabled\":false,\"x\":0}\n")],
        );
        let file = dir.join("a.json");
        let stamp = || {
            let m = std::fs::metadata(&file).unwrap();
            (m.ino(), m.mtime(), m.mtime_nsec(), std::fs::read(&file).unwrap())
        };
        let before = stamp();
        let current = state.rt.store().get("a.json").unwrap();
        let literals = [(vec!["x".to_owned()], GoValue::Number("-0".into()))];
        std::thread::sleep(std::time::Duration::from_millis(20));
        save_file_backed(
            &state,
            &current,
            &current.metadata,
            &literals,
            &mut CodexLock::default(),
        )
        .unwrap();
        assert_eq!(stamp(), before);
        // Go's manager publishes the record all the same.
        assert!(state.rt.store().get("a.json").unwrap().revision > current.revision);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Go `FileTokenStore.Save` skips writing an equal document, and the manager still
    /// publishes the saved record: a file edited to the same state behind its back, and
    /// an integer float64 cannot tell from the file's, both reach memory.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_equal_save_still_publishes_the_record() {
        use std::os::unix::fs::MetadataExt;
        let (state, dir) = management(
            "equal-publish",
            &[
                ("a.json", r#"{"type":"claude","disabled":false}"#),
                ("b.json", r#"{"type":"claude","request_retry":9007199254740992}"#),
            ],
        );
        let stamp = |file: &str| {
            let m = std::fs::metadata(dir.join(file)).unwrap();
            (
                m.ino(),
                m.mtime(),
                m.mtime_nsec(),
                std::fs::read(dir.join(file)).unwrap(),
            )
        };
        // Edited to disabled behind the watcher's back: a disable is equal on disk.
        std::fs::write(dir.join("a.json"), r#"{"type":"claude","disabled":true}"#).unwrap();
        let before = stamp("a.json");
        let a = state.rt.store().get("a.json").unwrap();
        assert!(!a.disabled);
        let mut meta = a.metadata.clone();
        meta.insert("disabled".into(), true.into());
        std::thread::sleep(std::time::Duration::from_millis(20));
        save_file_backed(&state, &a, &meta, &[], &mut CodexLock::default()).unwrap();
        assert_eq!(stamp("a.json"), before);
        assert!(state.rt.store().get("a.json").unwrap().disabled);

        // 9007199254740993 reads as 9007199254740992 in float64: equal, not written,
        // and the next save writes the integer Go kept.
        let before = stamp("b.json");
        let b = state.rt.store().get("b.json").unwrap();
        let mut meta = b.metadata.clone();
        meta.insert("request_retry".into(), 9_007_199_254_740_993_u64.into());
        let literals = [(
            vec!["request_retry".to_owned()],
            GoValue::Number("9007199254740993".into()),
        )];
        std::thread::sleep(std::time::Duration::from_millis(20));
        save_file_backed(&state, &b, &meta, &literals, &mut CodexLock::default()).unwrap();
        assert_eq!(stamp("b.json"), before);
        let b = state.rt.store().get("b.json").unwrap();
        assert_eq!(b.metadata["request_retry"], 9_007_199_254_740_993_u64);
        let mut meta = b.metadata.clone();
        meta.insert("disabled".into(), true.into());
        save_file_backed(&state, &b, &meta, &[], &mut CodexLock::default()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("b.json")).unwrap(),
            r#"{"disabled":true,"request_retry":9007199254740993,"type":"claude"}"#
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Go's in-memory numbers survive what does not reload the record: a watcher read of
    /// the file half-written (the record is kept, not rebuilt), and a token refresh,
    /// which changes only the keys it patches.
    #[tokio::test]
    async fn numbers_survive_incomplete_reads_and_refreshes() {
        let (state, dir) = management(
            "provenance-kept",
            &[
                ("a.json", r#"{"type":"claude","request_retry":1}"#),
                ("b.json", r#"{"type":"claude","request_retry":1}"#),
                ("big.json", r#"{"type":"claude","x":18446744073709553665}"#),
            ],
        );
        let save = |id: &str, meta: Map<String, Value>, literals: &[(Vec<String>, GoValue)]| {
            let current = state.rt.store().get(id).unwrap();
            let mut next = current.metadata.clone();
            next.extend(meta);
            save_file_backed(&state, &current, &next, literals, &mut CodexLock::default()).unwrap();
        };
        let exact = |id: &str| {
            let literals = [(
                vec!["request_retry".to_owned()],
                GoValue::Number("9007199254740993".into()),
            )];
            save(
                id,
                Map::from_iter([("request_retry".into(), 9_007_199_254_740_993_u64.into())]),
                &literals,
            );
        };
        let disable = |id: &str| save(id, Map::from_iter([("disabled".into(), true.into())]), &[]);
        let want = r#"{"disabled":true,"request_retry":9007199254740993,"type":"claude"}"#;

        // Truncated, read by the watcher (the record is kept), then restored.
        exact("a.json");
        let saved = std::fs::read(dir.join("a.json")).unwrap();
        std::fs::write(dir.join("a.json"), b"").unwrap();
        crate::watching::reload(&state).unwrap();
        std::fs::write(dir.join("a.json"), &saved).unwrap();
        disable("a.json");
        assert_eq!(std::fs::read_to_string(dir.join("a.json")).unwrap(), want);

        // A refresh after the save.
        exact("b.json");
        refresh(&state, "b.json", "fake-refreshed");
        disable("b.json");
        let got = on_disk(&dir, "b.json");
        assert_eq!(got["access_token"], "fake-refreshed");
        assert!(
            std::fs::read_to_string(dir.join("b.json"))
                .unwrap()
                .contains(r#""request_retry":9007199254740993"#)
        );

        // A refresh after the load: Go's float64 of the loaded text, not of serde's.
        refresh(&state, "big.json", "fake-refreshed");
        disable("big.json");
        assert!(
            std::fs::read_to_string(dir.join("big.json"))
                .unwrap()
                .contains(r#""x":18446744073709556000"#)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Go's numbers come from what the record was loaded or saved from, never from the
    /// file as it is now: a save after an edit nobody reloaded writes what Go holds, or
    /// refuses when only the edited text could say (docs/DIFFERENCES-FROM-GO.md).
    #[tokio::test]
    async fn numbers_follow_the_record_not_the_file() {
        let (state, dir) = management(
            "provenance",
            &[
                ("a.json", r#"{"type":"claude","request_retry":1}"#),
                ("big.json", r#"{"type":"claude","x":18446744073709553665}"#),
                ("edited.json", r#"{"type":"claude","x":18446744073709553665}"#),
            ],
        );
        let disable = |id: &str| {
            let current = state.rt.store().get(id).unwrap();
            let mut meta = current.metadata.clone();
            meta.insert("disabled".into(), true.into());
            save_file_backed(&state, &current, &meta, &[], &mut CodexLock::default())
        };
        // The saved integer survives bytes appended behind the watcher's back.
        let a = state.rt.store().get("a.json").unwrap();
        let mut meta = a.metadata.clone();
        meta.insert("request_retry".into(), 9_007_199_254_740_993_u64.into());
        let literals = [(
            vec!["request_retry".to_owned()],
            GoValue::Number("9007199254740993".into()),
        )];
        save_file_backed(&state, &a, &meta, &literals, &mut CodexLock::default()).unwrap();
        let mut text = std::fs::read(dir.join("a.json")).unwrap();
        text.extend_from_slice(b"  \n");
        std::fs::write(dir.join("a.json"), text).unwrap();
        disable("a.json").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("a.json")).unwrap(),
            r#"{"disabled":true,"request_retry":9007199254740993,"type":"claude"}"#
        );
        // Loaded and unchanged: Go's float64 of the loaded text (serde's default parse
        // reads 18446744073709551616).
        disable("big.json").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("big.json")).unwrap(),
            r#"{"disabled":true,"type":"claude","x":18446744073709556000}"#
        );
        // Replaced before a reload: the loaded text is gone, so the save is refused and
        // neither the file nor the record changes.
        let edited = r#"{"type":"claude","x":18446744073709551617}"#;
        std::fs::write(dir.join("edited.json"), edited).unwrap();
        let before = state.rt.store().get("edited.json").unwrap();
        let refused = disable("edited.json");
        assert!(
            matches!(&refused, Err(PatchError::Io(e)) if e.contains("changed since it was loaded")),
            "{refused:?}"
        );
        assert_eq!(std::fs::read_to_string(dir.join("edited.json")).unwrap(), edited);
        let after = state.rt.store().get("edited.json").unwrap();
        assert_eq!((after.revision, after.disabled), (before.revision, false));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Go `Manager.Modify` (`d69962e4`): a refresh that commits after a patch read the
    /// record makes the patch stale instead of being overwritten, for an in-place edit
    /// and for a changed `type` alike; the retry on the current record keeps both.
    #[tokio::test]
    async fn a_refresh_after_a_patch_read_the_record_is_never_overwritten() {
        let (state, dir) = management("stale", &[("a.json", r#"{"type":"claude","access_token":"fake-0"}"#)]);
        for (round, retyped) in [(1, false), (2, true)] {
            let read = state.rt.store().get("a.json").unwrap();
            let token = format!("fake-{round}");
            refresh(&state, "a.json", &token);
            let edit = |c: &Credential| {
                let mut meta = c.metadata.clone();
                meta.insert(format!("note_{round}"), round.into());
                if retyped {
                    meta.insert("type".into(), "codex".into());
                }
                meta
            };
            let stale = save_file_backed(&state, &read, &edit(&read), &[], &mut CodexLock::default());
            assert!(matches!(stale, Err(PatchError::Stale { .. })), "{stale:?}");
            assert_eq!(on_disk(&dir, "a.json")["access_token"], token.as_str());
            assert_eq!(
                state.rt.store().get("a.json").unwrap().metadata["access_token"],
                token.as_str()
            );
            let current = state.rt.store().get("a.json").unwrap();
            save_file_backed(&state, &current, &edit(&current), &[], &mut CodexLock::default()).unwrap();
            let saved = on_disk(&dir, "a.json");
            assert_eq!(
                (saved["access_token"].as_str(), &saved[format!("note_{round}")]),
                (Some(token.as_str()), &json!(round))
            );
        }
        assert_eq!(state.rt.store().get("a.json").unwrap().provider, "codex");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A patch re-reads only its own credential. A whole-folder rescan could publish
    /// another credential's file as a scan read it before that credential's refresh
    /// committed; here that older file sits on disk while the store holds the refresh.
    #[tokio::test]
    async fn a_patch_publishes_only_the_credential_it_changed() {
        let (state, dir) = management(
            "only",
            &[
                ("a.json", r#"{"type":"claude","access_token":"fake-a"}"#),
                ("c.json", r#"{"type":"claude","access_token":"fake-old"}"#),
            ],
        );
        refresh(&state, "c.json", "fake-new");
        std::fs::write(dir.join("c.json"), r#"{"type":"claude","access_token":"fake-old"}"#).unwrap();
        let token = |state: &Management| state.rt.store().get("c.json").unwrap().metadata["access_token"].clone();
        let r = fields(
            State(state.clone()),
            Ok(FirstJson(Bytes::from(r#"{"name":"a.json","note":1}"#))),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let r = fields(
            State(state.clone()),
            Ok(FirstJson(Bytes::from(r#"{"name":"a.json","type":"codex"}"#))),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let r = status(
            State(state.clone()),
            Ok(FirstJson(Bytes::from(r#"{"name":"a.json","disabled":true}"#))),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(token(&state), "fake-new");
        let a = state.rt.store().get("a.json").unwrap();
        assert_eq!(
            (a.provider.as_str(), a.disabled, &a.metadata["note"]),
            ("codex", true, &json!(1))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// An upload registers what it uploaded, as Go's upload does without asking plugin
    /// auth providers, even while one is active and has not parsed the new bytes: it
    /// replaces the credential with the same ID instead of keeping the old one.
    #[tokio::test]
    async fn an_upload_replaces_its_credential_while_plugins_parse_auth_files() {
        use cpa_core::config::credentials::{ParseRequest, Parsed, PluginAuthParser};
        struct Nothing;
        impl PluginAuthParser for Nothing {
            fn parse_auths(&self, _: &ParseRequest<'_>) -> Parsed {
                Parsed::NotHandled
            }
        }
        let (state, dir) = management(
            "upload-plugins",
            &[("a.json", r#"{"type":"claude","access_token":"fake-0"}"#)],
        );
        // This thread only: tests sharing the process never see the parser.
        credentials::with_thread_plugin_auth_parser(Arc::new(Nothing), || {
            let token = |state: &Management| state.rt.store().get("a.json").unwrap().metadata["access_token"].clone();
            assert_eq!(token(&state), "fake-0");
            store_file(
                &state,
                "a.json",
                br#"{"type":"claude","access_token":"fake-1"}"#,
                true,
                &mut CodexLock::default(),
            )
            .unwrap()
            .unwrap();
            assert_eq!(token(&state), "fake-1");
        });
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A token refresh that commits between an upload's write and its publication
    /// changed the file: the refreshed credential stays, on disk and in the store.
    #[tokio::test]
    async fn a_refresh_between_an_upload_and_its_publication_survives() {
        use cpa_core::config::credentials::{ParseRequest, Parsed, PluginAuthParser};
        struct Nothing;
        impl PluginAuthParser for Nothing {
            fn parse_auths(&self, _: &ParseRequest<'_>) -> Parsed {
                Parsed::NotHandled
            }
        }
        let (state, dir) = management(
            "upload-refresh",
            &[("a.json", r#"{"type":"claude","access_token":"fake-0"}"#)],
        );
        // This thread only: tests sharing the process never see the parser.
        credentials::with_thread_plugin_auth_parser(Arc::new(Nothing), || {
            let refreshing = state.clone();
            crate::management::BEFORE_RECONCILE.lock().unwrap().push((
                state.rt.config().auth_dir.clone(),
                Box::new(move || refresh(&refreshing, "a.json", "fake-refreshed")),
            ));
            store_file(
                &state,
                "a.json",
                br#"{"type":"claude","access_token":"fake-1"}"#,
                true,
                &mut CodexLock::default(),
            )
            .unwrap()
            .unwrap();
            let stored = state.rt.store().get("a.json").unwrap();
            assert_eq!(stored.provider, "claude");
            assert_eq!(stored.metadata["access_token"], "fake-refreshed");
            assert_eq!(on_disk(&dir, "a.json")["access_token"], "fake-refreshed");
        });
        std::fs::remove_dir_all(dir).unwrap();
    }
}
