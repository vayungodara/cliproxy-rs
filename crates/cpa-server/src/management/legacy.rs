//! Go's v0 management routes (`internal/api/server_management.go`) that predate the v8
//! tree. Reads render Go's runtime `Config` view ([`view`]); writes go through
//! [`super::v0_save`], which keeps a legacy-layout file in its layout and sends a v8
//! file to the v8 config writer, and through the v8 OAuth and logs handlers, so v0 and
//! v8 share one implementation as they do in Go.
use cpa_core::gostr::GoStr;
use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{OriginalUri, RawQuery, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use cpa_core::credential::Source;
use serde_json::{Value, json};

use super::{Management, json_error};

mod decode;
mod keys;
mod lists;
pub(crate) mod view;

/// Go `strings.TrimSpace`.
pub(crate) fn go_trim(s: &str) -> &str {
    std::str::from_utf8(cpa_core::config::go_trim_space(s.as_bytes())).unwrap_or(s)
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Bool,
    Int,
    Str,
}

/// A v0 scalar route: its path, response key, v8 document path and Go type.
struct Field {
    route: &'static str,
    key: &'static str,
    v8: &'static str,
    kind: Kind,
}

const fn field(route: &'static str, key: &'static str, v8: &'static str, kind: Kind) -> Field {
    Field { route, key, v8, kind }
}

/// Go's `Get*`/`Put*` pairs for single config values.
pub(crate) const FIELD_ROUTES: [&str; 15] = [
    "debug",
    "logging-to-file",
    "logs-max-total-size-mb",
    "error-logs-max-files",
    "usage-statistics-enabled",
    "proxy-url",
    "quota-exceeded/switch-project",
    "quota-exceeded/switch-preview-model",
    "request-log",
    "ws-auth",
    "request-retry",
    "max-retry-credentials",
    "max-retry-interval",
    "force-model-prefix",
    "routing/strategy",
];

const FIELDS: [Field; 15] = [
    field("debug", "debug", "observability/logs/debug", Kind::Bool),
    field(
        "logging-to-file",
        "logging-to-file",
        "observability/logs/logging-to-file",
        Kind::Bool,
    ),
    field(
        "logs-max-total-size-mb",
        "logs-max-total-size-mb",
        "observability/logs/logs-max-total-size-mb",
        Kind::Int,
    ),
    field(
        "error-logs-max-files",
        "error-logs-max-files",
        "observability/logs/error-logs-max-files",
        Kind::Int,
    ),
    field(
        "usage-statistics-enabled",
        "usage-statistics-enabled",
        "observability/usage/usage-statistics-enabled",
        Kind::Bool,
    ),
    field("proxy-url", "proxy-url", "requests/proxy-url", Kind::Str),
    field(
        "quota-exceeded/switch-project",
        "switch-project",
        "quota-exceeded/switch-project",
        Kind::Bool,
    ),
    field(
        "quota-exceeded/switch-preview-model",
        "switch-preview-model",
        "quota-exceeded/switch-preview-model",
        Kind::Bool,
    ),
    field(
        "request-log",
        "request-log",
        "observability/logs/request-log",
        Kind::Bool,
    ),
    field("ws-auth", "ws-auth", "oauth/providers/aistudio/ws-auth", Kind::Bool),
    field(
        "request-retry",
        "request-retry",
        "routing/retry/request-retry",
        Kind::Int,
    ),
    field(
        "max-retry-credentials",
        "max-retry-credentials",
        "routing/retry/max-retry-credentials",
        Kind::Int,
    ),
    field(
        "max-retry-interval",
        "max-retry-interval",
        "routing/retry/max-retry-interval",
        Kind::Int,
    ),
    field(
        "force-model-prefix",
        "force-model-prefix",
        "routing/force-model-prefix",
        Kind::Bool,
    ),
    field("routing/strategy", "strategy", "routing/strategy", Kind::Str),
];

/// Go's provider key lists and OAuth maps, by v0 route.
pub(crate) const LIST_ROUTES: [&str; 11] = [
    "gemini-api-key",
    "interactions-api-key",
    "claude-api-key",
    "codex-api-key",
    "xai-api-key",
    "meta-api-key",
    "openai-compatibility",
    "vertex-api-key",
    "oauth-excluded-models",
    "oauth-model-alias",
    "oauth-request-scoped-errors",
];

/// v0 `*-auth-url` routes and the provider the v8 handler dispatches them to.
pub(crate) const AUTH_URL_ROUTES: [(&str, &str); 8] = [
    ("anthropic-auth-url", "claude"),
    ("codex-auth-url", "codex"),
    ("antigravity-auth-url", "antigravity"),
    ("kimi-auth-url", "kimi"),
    ("kimi-ai-auth-url", "kimi-ai"),
    ("xai-auth-url", "xai"),
    ("devin-auth-url", "devin"),
    ("meta-auth-url", "meta"),
];

fn route_of(uri: &axum::http::Uri) -> &str {
    uri.path().strip_prefix("/v0/management/").unwrap_or_default()
}

/// gin `c.JSON`: `json.Marshal` escapes `<`, `>`, `&`, U+2028 and U+2029 inside
/// strings (the only places these characters occur in compact JSON).
fn go_json(status: StatusCode, value: &Value) -> Response {
    let text = value
        .to_string()
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    (
        status,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        )],
        text,
    )
        .into_response()
}

fn ok() -> Response {
    go_json(StatusCode::OK, &json!({"status": "ok"}))
}

fn bad(message: &str) -> Response {
    go_json(StatusCode::BAD_REQUEST, &json!({"error": message}))
}

/// The published runtime config as Go's `json.Marshal(cfg)` view.
fn current(state: &Management) -> Value {
    view::config(&state.rt.config().document)
}

/// GET /v0/management/config (Go `GetConfig`).
pub(crate) async fn config(State(state): State<Arc<Management>>) -> Response {
    go_json(StatusCode::OK, &current(&state))
}

/// Go `normalizeRoutingStrategy`.
fn routing_strategy(s: &str) -> Option<&'static str> {
    match go_trim(s).go_lower().as_str() {
        "" | "round-robin" | "roundrobin" | "rr" => Some("round-robin"),
        "weighted-round-robin" | "weightedroundrobin" | "wrr" => Some("weighted-round-robin"),
        "fill-first" | "fillfirst" | "ff" => Some("fill-first"),
        // cliproxy-rs addition (see crate::scheduler::Strategy::SoonestReset).
        "soonest-reset" | "soonestreset" | "reset-first" | "resetfirst" => Some("soonest-reset"),
        _ => None,
    }
}

/// gin `ShouldBindJSON` into `struct { Value *T }`; `None` (also for a null value) is
/// Go's 400.
fn bind_value(body: &[u8], kind: Kind) -> Option<Value> {
    let kind = match kind {
        Kind::Bool => "bool",
        Kind::Int => "int",
        Kind::Str => "string",
    };
    let shape = decode::record(vec![decode::field("value", kind, true, None)]);
    decode::bind(&shape, body)?.remove("value").filter(|v| !v.is_null())
}

/// Go `persistLocked`'s failure body for a save the shared writer could not make.
async fn save_error(res: Response) -> Response {
    if res.status() != StatusCode::INTERNAL_SERVER_ERROR {
        return res;
    }
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap_or_default();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let detail = body["message"]
        .as_str()
        .or(body["error"].as_str())
        .unwrap_or("internal_error");
    go_json(
        StatusCode::INTERNAL_SERVER_ERROR,
        &json!({ "error": format!("failed to save config: {detail}") }),
    )
}

/// Where a v0 save lands: its v8 document path, and the legacy path (`/`-separated) a
/// file without v8 sections keeps it at, since Go v8.0.35 saves such a file in its
/// legacy layout ([`super::v0_save`]).
pub(super) struct Target {
    v8: String,
    legacy: String,
}

pub(super) fn target(v8: impl Into<String>, legacy: impl Into<String>) -> Target {
    Target {
        v8: v8.into(),
        legacy: legacy.into(),
    }
}

/// One v0 save.
pub(super) enum Change {
    /// `value` at the v8 path, or `legacy` at the legacy path of a legacy file.
    Write(Target, Value, Value),
    /// One path saved unchanged, as Go saves after a handler that changed nothing.
    /// ponytail: a path absent from the document is not saved at all; Go would still
    /// rewrite the file (and fail on a read-only disk).
    Touch(Target),
    /// One path deleted (absent counts as done).
    Remove(Target),
    /// No save: this answer as is.
    Answer(Response),
}

/// Go resolves and changes a v0 setting under the lock its saves hold (1631875b): `f`
/// reads the published config and decides the change, which is saved under the same
/// `disk` lock, so no other save or reload lands between the read and the write. A
/// save answers Go's v0 success body.
pub(super) async fn locked(state: Arc<Management>, f: impl FnOnce(Value) -> Change + Send + 'static) -> Response {
    let saved = tokio::task::spawn_blocking(move || {
        let disk = state.disk.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let cfg = current(&state);
        #[cfg(test)]
        {
            let hook = {
                let mut slot = AFTER_READ.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                match slot.as_ref() {
                    Some((path, _)) if *path == state.path => slot.take(),
                    _ => None,
                }
            };
            if let Some((_, hook)) = hook {
                hook();
            }
        }
        save(&state, &disk, f(cfg))
    })
    .await;
    match saved {
        Ok((res, None)) => res,
        Ok((res, Some(delete))) => match res.status() {
            StatusCode::OK => ok(),
            StatusCode::NOT_FOUND if delete => ok(),
            _ => save_error(res).await,
        },
        // A failed save task fails as Go's `persistLocked` does.
        Err(_) => save_error(json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")).await,
    }
}

/// Test hook run once, in [`locked`] for the config file it names, after the published
/// config was read and before the change it decides is saved.
#[cfg(test)]
static AFTER_READ: std::sync::Mutex<Option<(std::path::PathBuf, super::LayoutHook)>> = std::sync::Mutex::new(None);

/// Saves through the legacy or the shared config writer, with whether it was a delete
/// (`None` for an answer that saved nothing).
fn save(state: &Management, disk: &std::sync::MutexGuard<'_, ()>, change: Change) -> (Response, Option<bool>) {
    let (t, v8, legacy) = match change {
        Change::Write(t, value, legacy) => (t, Some(value), super::LegacyEdit::Set(legacy)),
        Change::Touch(t) => {
            let cfg = state.rt.config();
            let current =
                t.v8.split('/')
                    .try_fold(&cfg.document, |v, k| v.get(k))
                    .and_then(|v| serde_json::to_value(cpa_core::config::AsJson(v)).ok());
            match current {
                Some(value) => (t, Some(value), super::LegacyEdit::Keep),
                None => return (ok(), None),
            }
        }
        Change::Remove(t) => (t, None, super::LegacyEdit::Delete),
        Change::Answer(res) => return (res, None),
    };
    let delete = v8.is_none();
    let parts: Vec<&str> = t.legacy.split('/').collect();
    let path = format!("/v8/management/config/{}", t.v8);
    let res = super::v0_save(state, disk, &parts, legacy, super::V8Edit { path: &path, value: v8 });
    (res, Some(delete))
}

/// Writes `value` at the v8 path, or `legacy` at the legacy path of a legacy file.
async fn write(state: Arc<Management>, t: Target, value: Value, legacy: Value) -> Response {
    locked(state, move |_| Change::Write(t, value, legacy)).await
}

/// Go's scalar v0 routes: GET reads the runtime value, PUT and PATCH take
/// `{"value": ...}`, DELETE (proxy-url only) clears it.
pub(crate) async fn field_route(
    State(state): State<Arc<Management>>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    crate::body_limit::FirstJson(body): crate::body_limit::FirstJson,
) -> Response {
    let Some(f) = FIELDS.iter().find(|f| f.route == route_of(&uri)) else {
        return super::access::not_found();
    };
    if method == Method::GET {
        let cfg = current(&state);
        let value = match f.route {
            "routing/strategy" => {
                let raw = cfg["routing"]["strategy"].as_str().unwrap_or_default();
                Value::from(routing_strategy(raw).unwrap_or(go_trim(raw)))
            }
            r if r.starts_with("quota-exceeded/") => cfg["quota-exceeded"][f.key].clone(),
            _ => cfg[f.key].clone(),
        };
        return go_json(StatusCode::OK, &json!({ f.key: value }));
    }
    if method == Method::DELETE {
        return write(state, target(f.v8, f.route), Value::from(""), Value::from("")).await;
    }
    let Some(mut value) = bind_value(&body, f.kind) else {
        return bad("invalid body");
    };
    match f.route {
        "logs-max-total-size-mb" if value.as_i64() < Some(0) => value = 0.into(),
        "error-logs-max-files" if value.as_i64() < Some(0) => value = 10.into(),
        "routing/strategy" => match routing_strategy(value.as_str().unwrap_or_default()) {
            Some(s) => value = s.into(),
            None => return bad("invalid strategy"),
        },
        _ => {}
    }
    write(state, target(f.v8, f.route), value.clone(), value).await
}

/// Go `fmt.Sscanf(s, "%d", &n)`: an optionally signed decimal prefix after Go's
/// scanner spaces; a newline before it fails, as Sscanf does not treat it as space.
fn sscanf_int(s: &str) -> Option<i64> {
    // fmt's `space` table.
    let space = |c: char| {
        matches!(c, '\u{9}'..='\u{d}' | ' ' | '\u{85}' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}')
            || matches!(c, '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
    };
    let mut s = s;
    loop {
        let mut chars = s.chars();
        match chars.next() {
            Some('\n') => return None,
            Some('\r') if chars.as_str().starts_with('\n') => return None,
            Some(c) if space(c) => s = chars.as_str(),
            _ => break,
        }
    }
    let digits_from = usize::from(s.starts_with(['+', '-']));
    let end = s[digits_from..]
        .find(|c: char| !c.is_ascii_digit())
        .map_or(s.len(), |i| i + digits_from);
    if end == digits_from {
        return None;
    }
    s[..end].parse().ok()
}

fn query_first(raw: Option<&str>, name: &str) -> Option<String> {
    url_pairs(raw.unwrap_or_default())
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
}

/// `url.ParseQuery` pairs (malformed pairs skipped).
fn url_pairs(raw: &str) -> impl Iterator<Item = (String, String)> + '_ {
    raw.split('&')
        .filter(|p| !p.is_empty() && !p.contains(';'))
        .filter_map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            Some((unescape(k)?, unescape(v)?))
        })
}

fn unescape(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// Go's `api-keys` routes (`putStringList`, `patchStringList`, `deleteFromStringList`).
pub(crate) async fn api_keys(
    State(state): State<Arc<Management>>,
    method: Method,
    RawQuery(raw): RawQuery,
    read: crate::body_limit::Read,
) -> Response {
    let body = match patch_body(&method, read) {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    if method == Method::GET {
        return go_json(StatusCode::OK, &json!({"api-keys": current(&state)["api-keys"]}));
    }
    if !matches!(method, Method::PUT | Method::PATCH | Method::DELETE) {
        return super::access::not_found();
    }
    locked(state, move |cfg| {
        let mut keys: Vec<Value> = cfg["api-keys"].as_array().cloned().unwrap_or_default();
        let write = |keys: Vec<Value>| {
            Change::Write(
                target("access/api-keys", "api-keys"),
                Value::Array(keys.clone()),
                Value::Array(keys),
            )
        };
        match method {
            Method::PUT => {
                // Go `putStringList`.
                let Some(list) = decode::put_collection(view::field("api-keys"), &body, true) else {
                    return Change::Answer(bad("invalid body"));
                };
                // Go copies with append([]string(nil), v...): an empty list becomes nil.
                let list = list.as_array().filter(|l| !l.is_empty()).cloned();
                let list = list.map_or(Value::Null, Value::Array);
                Change::Write(target("access/api-keys", "api-keys"), list.clone(), list)
            }
            Method::PATCH => {
                // Go `patchStringList`: `{old, new, index, value}`, all pointers.
                let shape = decode::record(vec![
                    decode::field("old", "string", true, None),
                    decode::field("new", "string", true, None),
                    decode::field("index", "int", true, None),
                    decode::field("value", "string", true, None),
                ]);
                let Some(b) = decode::bind(&shape, &body) else {
                    return Change::Answer(bad("invalid body"));
                };
                let text = |k: &str| b.get(k).and_then(Value::as_str).map(str::to_owned);
                let (old, new, value) = (text("old"), text("new"), text("value"));
                let index = b.get("index").and_then(Value::as_i64);
                if let (Some(i), Some(v)) = (index, &value)
                    && i >= 0
                    && (i as usize) < keys.len()
                {
                    keys[i as usize] = Value::from(v.clone());
                    return write(keys);
                }
                if let (Some(old), Some(new)) = (old, new) {
                    match keys.iter_mut().find(|k| k.as_str() == Some(old.as_str())) {
                        Some(slot) => *slot = Value::from(new),
                        None => keys.push(Value::from(new)),
                    }
                    return write(keys);
                }
                Change::Answer(bad("missing fields"))
            }
            _ => {
                let raw = raw.as_deref();
                if let Some(i) = query_first(raw, "index")
                    .filter(|s| !s.is_empty())
                    .and_then(|s| sscanf_int(&s))
                    && i >= 0
                    && (i as usize) < keys.len()
                {
                    keys.remove(i as usize);
                    return write(keys);
                }
                if let Some(value) = query_first(raw, "value")
                    .map(|v| go_trim(&v).to_owned())
                    .filter(|v| !v.is_empty())
                {
                    keys.retain(|k| go_trim(k.as_str().unwrap_or_default()) != value);
                    return write(keys);
                }
                Change::Answer(bad("missing index or value"))
            }
        }
    })
    .await
}

/// The body of a list route: PATCH binds it with gin `ShouldBindJSON`, the other
/// methods read it whole (`GetRawData`).
fn patch_body(method: &Method, read: crate::body_limit::Read) -> Result<Bytes, BytesRejection> {
    match *method {
        Method::PATCH => read.first_json(),
        _ => read.body,
    }
}

/// Live credential indexes by credential ID (Go `liveAuthIndexByID`).
fn live_indexes(state: &Management) -> HashMap<String, String> {
    state
        .rt
        .store()
        .snapshot()
        .iter()
        .filter(|c| matches!(c.source, Source::Config { .. }))
        .map(|c| (c.id.clone(), cpa_core::config::credentials::auth_index(c)))
        .collect()
}

/// Go's provider key lists (GET with `auth-index`) and OAuth maps; writes go to
/// [`lists::change`].
pub(crate) async fn list_route(
    State(state): State<Arc<Management>>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    RawQuery(raw): RawQuery,
    read: crate::body_limit::Read,
) -> Response {
    let body = match patch_body(&method, read) {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let route = route_of(&uri).to_owned();
    if method != Method::GET {
        return lists::change(state, route, method, raw, body).await;
    }
    let cfg = current(&state);
    let value = if route.starts_with("oauth-") {
        cfg.get(&route).cloned().unwrap_or(Value::Null)
    } else {
        keys::with_auth_index(&route, &cfg[&route], &live_indexes(&state))
    };
    go_json(StatusCode::OK, &json!({ route: value }))
}

/// v0 `*-auth-url`: the v8 `/oauth/auth-url` handler for that provider (Go's
/// `StartOAuthV8` dispatches to the same functions).
pub(crate) async fn auth_url(
    State(state): State<Arc<Management>>,
    OriginalUri(uri): OriginalUri,
    RawQuery(raw): RawQuery,
) -> Response {
    let route = route_of(&uri);
    let Some((_, provider)) = AUTH_URL_ROUTES.iter().find(|(r, _)| *r == route) else {
        return super::access::not_found();
    };
    let query = match raw.filter(|r| !r.is_empty()) {
        Some(raw) => format!("provider={provider}&{raw}"),
        None => format!("provider={provider}"),
    };
    super::oauth::auth_url(State(state), RawQuery(Some(query))).await
}

/// Go `NormalizeCommentIndentation`: comment lines lose their leading spaces and tabs.
fn normalize_comment_indentation(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            let trimmed = line.trim_start_matches([' ', '\t']);
            if trimmed.starts_with('#') { trimmed } else { line }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A weight Go's config loading rejects.
struct WeightError {
    /// `<path>[<i>].weight: <reason>`.
    message: String,
    /// The weight as written.
    value: serde_yaml_ng::Value,
}

/// Go `validateWeightMappingNode` on each mapping of a sequence: the first weight that
/// is not an integer or is above the limit.
fn weight_error_in(items: &serde_yaml_ng::Value, path: &str) -> Option<WeightError> {
    for (i, item) in items.as_sequence()?.iter().enumerate() {
        let Some(weight) = item.as_mapping().and_then(|m| m.get("weight")) else {
            continue;
        };
        let reason = match weight.as_i64().filter(|_| weight.is_number()) {
            None => "weight must be an integer",
            Some(w) if w > 1_000_000 => "weight must not exceed 1000000",
            Some(_) => continue,
        };
        return Some(WeightError {
            message: format!("{path}[{i}].weight: {reason}"),
            value: weight.clone(),
        });
    }
    None
}

/// Go `validateCredentialWeightYAML` (run first by `LoadConfig`, so 422) on the legacy
/// key lists and OpenAI-compatible `api-key-entries` of the merge-expanded document, in
/// document order, skipping lists a v8 family replaces. A weight that is neither a
/// number nor null fails Go's decode into `*int` before that: 400.
// ponytail: that 400 message names the value where yaml.v3 also names the line.
fn legacy_weight_error(root: &serde_yaml_ng::Value) -> Option<Response> {
    let v8 = root.get("api-keys").and_then(serde_yaml_ng::Value::as_mapping);
    // Go's flattenV8 replaces a legacy list with the v8 family when that is present.
    let superseded = |legacy: &str| {
        view::KEY_FAMILIES
            .iter()
            .any(|(old, new)| *old == legacy && v8.is_some_and(|m| m.contains_key(*new)))
    };
    let mut found = None;
    for (name, value) in root.as_mapping()? {
        let name = name.as_str().unwrap_or_default();
        if superseded(name) {
            continue;
        }
        if name == "openai-compatibility" {
            found = value
                .as_sequence()
                .into_iter()
                .flatten()
                .enumerate()
                .find_map(|(p, provider)| {
                    let entries = provider.as_mapping()?.get("api-key-entries")?;
                    weight_error_in(entries, &format!("openai-compatibility[{p}].api-key-entries"))
                });
        } else if view::KEY_FAMILIES.iter().any(|(legacy, _)| *legacy == name) {
            found = weight_error_in(value, name);
        }
        if found.is_some() {
            break;
        }
    }
    let e = found?;
    // yaml.v3 decodes a float into an int (truncating); only the raw-tag check after
    // decoding rejects it.
    Some(if !e.value.is_number() && !e.value.is_null() {
        let written = serde_yaml_ng::to_string(&e.value).unwrap_or_default();
        let message = format!(
            "yaml: unmarshal errors:\n  cannot unmarshal {} into int",
            written.trim()
        );
        go_json(
            StatusCode::BAD_REQUEST,
            &json!({"error": "invalid_yaml", "message": message}),
        )
    } else {
        go_json(
            StatusCode::UNPROCESSABLE_ENTITY,
            &json!({"error": "invalid_config", "message": e.message}),
        )
    })
}

/// Go `PutConfigYAML` (v0 only; v8 has its own writer): the upload, with comment lines
/// unindented, must decode and load as a config, then is written and reloaded, which
/// hashes a plaintext management key as Go's `LoadConfig` does.
///
/// Go answers 400 for what fails while decoding (syntax, shape, v8 key groups) and 422
/// for `LoadConfig` validation. ponytail: a type mismatch (`port: abc`) is also a decode
/// error in Go but only `Config::parse` finds it here, so it is 422; and syntax messages
/// are this parser's, not yaml.v3's.
pub(crate) async fn put_config_yaml(State(state): State<Arc<Management>>, body: Bytes) -> Response {
    let fail = |status: StatusCode, error: &str, message: String| {
        go_json(status, &json!({ "error": error, "message": message }))
    };
    let Ok(text) = std::str::from_utf8(&body) else {
        return fail(
            StatusCode::BAD_REQUEST,
            "invalid_yaml",
            "yaml: invalid leading UTF-8 octet".into(),
        );
    };
    // Validate the bytes that will be written. Go validates the upload and unindents
    // comment lines only while writing, which can break a block scalar holding a `#` line
    // after the file is already overwritten.
    let text = normalize_comment_indentation(text);
    let root = match cpa_core::config::yaml_value(&text) {
        Err(e) => return fail(StatusCode::BAD_REQUEST, "invalid_yaml", e.to_string()),
        Ok(root @ serde_yaml_ng::Value::Mapping(_)) => root,
        Ok(serde_yaml_ng::Value::Null) if text.trim().is_empty() => serde_yaml_ng::Value::Null,
        Ok(_) => {
            return fail(
                StatusCode::BAD_REQUEST,
                "invalid_yaml",
                "config must be a mapping".into(),
            );
        }
    };
    if let Err(e) = cpa_core::config::ConfigDocument::parse(&text) {
        return fail(StatusCode::BAD_REQUEST, "invalid_yaml", format!("{e:#}"));
    }
    // Go checks the alias- and merge-expanded tree (`expandConfigAliases`).
    let mut root = root;
    if let Err(e) = cpa_core::config::expand_merges(&mut root) {
        return fail(StatusCode::BAD_REQUEST, "invalid_yaml", format!("{e:#}"));
    }
    // The v8 key groups, which Go checks while decoding.
    if let Some(e) = root.as_mapping().and_then(cpa_core::config::v8_group_error) {
        return fail(StatusCode::BAD_REQUEST, "invalid_yaml", e);
    }
    // Go v8.0.35 validates the model catalog sources while decoding the upload into
    // Config (Config.UnmarshalYAML), so a bad source is this 400, not LoadConfig's 422.
    if let Err(e) = cpa_core::config::validate_catalog_sources(root.get("models")) {
        return fail(StatusCode::BAD_REQUEST, "invalid_yaml", format!("{e:#}"));
    }
    if let Some(answer) = legacy_weight_error(&root) {
        return answer;
    }
    let cfg = match cpa_core::config::Config::parse(&text) {
        Ok(cfg) => cfg,
        Err(e) => return fail(StatusCode::UNPROCESSABLE_ENTITY, "invalid_config", format!("{e:#}")),
    };
    // Go's LoadConfig hashes a plaintext key during validation, and bcrypt refuses more
    // than 72 bytes (the Rust crate would silently truncate).
    let secret = &cfg.management.secret_key;
    if !secret.is_empty() && !cpa_core::config::is_bcrypt(secret) && secret.len() > 72 {
        return fail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_config",
            "failed to hash remote management key: bcrypt: password length exceeds 72 bytes".into(),
        );
    }
    // Go v8.0.35 `WriteConfig` (3be5fa44): a v8 upload is saved in the latest layout;
    // a legacy-only one as submitted.
    let text = if cpa_core::config::is_v8_layout(&root) {
        let mut doc = match cpa_core::config::ConfigDocument::parse(&text) {
            Ok(doc) => doc,
            Err(e) => return fail(StatusCode::BAD_REQUEST, "invalid_yaml", format!("{e:#}")),
        };
        let basis = doc.migrated_text(&text).unwrap_or_else(|| text.clone());
        let archived = doc.archive_unknown();
        match doc.render_preserving(&basis) {
            Ok(rendered) => rendered + &cpa_core::config::archive_comments(&archived),
            Err(e) => return fail(StatusCode::INTERNAL_SERVER_ERROR, "write_failed", format!("{e:#}")),
        }
    } else {
        text
    };
    tokio::task::spawn_blocking(move || {
        let _guard = state.disk.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if cpa_core::config::ConfigDocument::write(&state.path, &text).is_err() {
            return fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "write_failed",
                "failed to write config".into(),
            );
        }
        match cpa_core::config::Config::load(&state.path) {
            Ok(cfg) => {
                state.publish(cfg, None);
                go_json(StatusCode::OK, &json!({"changed": ["config"], "ok": true}))
            }
            Err(e) => fail(StatusCode::INTERNAL_SERVER_ERROR, "reload_failed", format!("{e:#}")),
        }
    })
    .await
    .unwrap_or_else(|_| json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error"))
}

/// POST /v0/management/vertex/import: v8 `/oauth/import?provider=vertex`.
pub(crate) async fn vertex_import(
    state: State<Arc<Management>>,
    RawQuery(raw): RawQuery,
    headers: axum::http::HeaderMap,
    read: crate::body_limit::Read,
) -> Response {
    // Go routes straight to the handler: a `provider` in the query does not matter.
    let query = match raw.filter(|r| !r.is_empty()) {
        Some(raw) => format!("provider=vertex&{raw}"),
        None => "provider=vertex".to_owned(),
    };
    super::oauth::import(state, RawQuery(Some(query)), headers, read).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A v0 setter chooses its writer and writes under one `disk` lock. At the point
    /// where it has chosen the v8 writer, a legacy `config.yaml` upload starts; it must
    /// not get in before the setter writes. If it could (the lock released between the
    /// layout check and the write), the setter would then migrate the uploaded legacy
    /// file and its `codex-header-defaults` would become OAuth-only.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_v0_save_holds_the_disk_lock_from_layout_to_write() {
        let dir = std::env::temp_dir().join(format!("cpa-v0-race-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        let auth = format!("{}", dir.display());
        std::fs::write(&path, format!("config-version: 8\noauth: {{auth-dir: {auth}}}\n")).unwrap();
        let cfg = cpa_core::config::Config::load(&path).unwrap();
        let rt = Arc::new(crate::testing::runtime(
            cfg.clone(),
            Vec::new(),
            cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        let state = Management::with_options(rt, path.clone(), Default::default());
        let upload =
            format!("auth-dir: {auth}\nrequest-retry: 3\ncodex-header-defaults: {{user-agent: legacy-agent}}\n");
        let (upload_tx, upload_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let handle = tokio::runtime::Handle::current();
        let hook = {
            let (state, upload) = (state.clone(), upload.clone());
            move || {
                let task = handle.spawn(put_config_yaml(State(state), Bytes::from(upload)));
                upload_tx.send(task).unwrap();
                // Unlocked here, the upload finishes well within this window; locked, it
                // cannot write until the setter has.
                let raced = finished_rx.recv_timeout(std::time::Duration::from_secs(2)).is_ok();
                assert!(!raced, "the upload wrote between the layout check and the write");
            }
        };
        *super::super::AFTER_LAYOUT.lock().unwrap() = Some((path.clone(), Box::new(hook)));
        let setter = field_route(
            State(state.clone()),
            OriginalUri("/v0/management/request-retry".parse().unwrap()),
            Method::PUT,
            crate::body_limit::FirstJson(Bytes::from(r#"{"value":2}"#)),
        );
        let setter = tokio::time::timeout(std::time::Duration::from_secs(10), setter);
        let watcher = tokio::task::spawn_blocking(move || {
            let task = upload_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("the hook ran");
            let status = tokio::runtime::Handle::current().block_on(task).unwrap().status();
            let _ = finished_tx.send(());
            status
        });
        let saved = setter.await.expect("the setter finished");
        assert_eq!(saved.status(), StatusCode::OK);
        assert_eq!(watcher.await.unwrap(), StatusCode::OK);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), upload);
        let cfg = state.rt.config();
        let api = cfg.for_api_key();
        let ua = &api.document["oauth"]["providers"]["codex"]["header-defaults"]["user-agent"];
        assert_eq!(ua.as_str(), Some("legacy-agent"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Go 1631875b: a v0 list mutation reads and saves under the lock v8 saves hold. A
    /// v8 save of the same list that starts after the v0 PATCH read [a, b] lands after
    /// the PATCH, so it is kept; were the read outside the lock, the PATCH would
    /// overwrite it with its stale [a2, b].
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_v0_list_change_reads_and_saves_under_the_disk_lock() {
        let dir = std::env::temp_dir().join(format!("cpa-v0-list-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        let config = format!(
            "config-version: 8\noauth: {{auth-dir: {}}}\naccess: {{api-keys: [a, b]}}\n",
            dir.display()
        );
        std::fs::write(&path, config).unwrap();
        let rt = Arc::new(crate::testing::runtime(
            cpa_core::config::Config::load(&path).unwrap(),
            Vec::new(),
            cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        let state = Management::with_options(rt, path.clone(), Default::default());
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let v8 = {
            let state = state.clone();
            std::thread::spawn(move || {
                go_rx.recv().unwrap();
                let res = super::super::config_sync(
                    &state,
                    "/v8/management/config/access/api-keys",
                    Method::PUT,
                    &[],
                    Ok(br#"["a","b","c"]"#),
                );
                let _ = done_tx.send(());
                res.status()
            })
        };
        let hook = move || {
            go_tx.send(()).unwrap();
            // Unlocked, the v8 save finishes well within this window.
            let _ = done_rx.recv_timeout(std::time::Duration::from_secs(1));
        };
        *AFTER_READ.lock().unwrap() = Some((path.clone(), Box::new(hook)));
        let patch = api_keys(
            State(state.clone()),
            Method::PATCH,
            RawQuery(None),
            Bytes::from(r#"{"old":"a","new":"a2"}"#).into(),
        )
        .await;
        assert_eq!(patch.status(), StatusCode::OK);
        assert_eq!(
            tokio::task::spawn_blocking(move || v8.join().unwrap()).await.unwrap(),
            StatusCode::OK
        );
        assert_eq!(current(&state)["api-keys"], json!(["a", "b", "c"]));
        assert!(std::fs::read_to_string(&path).unwrap().contains("- c"));

        // The other order: a writer holds the lock and publishes [a, b, c, d]; a v0 PATCH
        // queued behind it reads after the publish (a read before taking the lock would
        // see [a, b, c] and drop d).
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let writer = {
            let (state, path) = (state.clone(), path.clone());
            std::thread::spawn(move || {
                let _disk = state.disk.lock().unwrap();
                held_tx.send(()).unwrap();
                go_rx.recv().unwrap();
                let text = std::fs::read_to_string(&path).unwrap().replace("- c", "- c\n    - d");
                std::fs::write(&path, text).unwrap();
                state.publish(cpa_core::config::Config::load(&path).unwrap(), None);
            })
        };
        held_rx.recv().unwrap();
        let patch = tokio::spawn(api_keys(
            State(state.clone()),
            Method::PATCH,
            RawQuery(None),
            Bytes::from(r#"{"old":"b","new":"b2"}"#).into(),
        ));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await; // Queued on the lock.
        go_tx.send(()).unwrap();
        assert_eq!(patch.await.unwrap().status(), StatusCode::OK);
        tokio::task::spawn_blocking(move || writer.join().unwrap())
            .await
            .unwrap();
        assert_eq!(current(&state)["api-keys"], json!(["a", "b2", "c", "d"]));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn value_binding_follows_gin() {
        assert_eq!(bind_value(br#"{"value":true}"#, Kind::Bool), Some(json!(true)));
        assert_eq!(bind_value(br#"{"Value":true} trailing"#, Kind::Bool), Some(json!(true)));
        assert_eq!(bind_value(br#"{"value":"true"}"#, Kind::Bool), None);
        assert_eq!(bind_value(br#"{"value":1.5}"#, Kind::Int), None);
        assert_eq!(bind_value(br#"{"value":7,"value":null}"#, Kind::Int), None);
        assert_eq!(bind_value(br#"{"value":-3}"#, Kind::Int), Some(json!(-3)));
        assert_eq!(bind_value(br#"{"value":-0}"#, Kind::Int), Some(json!(0)));
        assert_eq!(bind_value(b"", Kind::Bool), None);
        assert_eq!(bind_value(b"null", Kind::Bool), None);
    }

    #[test]
    fn put_string_list_follows_go_unmarshal() {
        let list = |b: &str| decode::put_collection(view::field("api-keys"), b.as_bytes(), true);
        assert_eq!(list(r#"["a",null]"#), Some(json!(["a", ""])));
        assert_eq!(list("null"), Some(Value::Null));
        assert_eq!(list(r#"{"ITEMS":["r"]}"#), Some(json!(["r"])));
        assert_eq!(list(r#"{"item\u017f":["r"]}"#), Some(json!(["r"])));
        assert_eq!(list(r#"{"items":["a"],"items":[null]}"#), Some(json!(["a"])));
        assert_eq!(list(r#"{"items":7,"items":["r"]}"#), None);
        assert_eq!(list(r#"{"items":["r"],"ITEMS":null}"#), None);
        assert_eq!(list(r#"{"items":[]}"#), None);
        assert_eq!(list(r#"["a"] x"#), None);
        assert_eq!(list(r#""x""#), None);
    }

    #[test]
    fn comment_lines_lose_their_indentation_like_go() {
        assert_eq!(
            normalize_comment_indentation("a:\n    # c\n  b: 1 # d\n\t#e\n"),
            "a:\n# c\n  b: 1 # d\n#e\n"
        );
    }

    #[test]
    fn sscanf_reads_an_integer_prefix() {
        assert_eq!(sscanf_int("1"), Some(1));
        assert_eq!(sscanf_int(" -2x"), Some(-2));
        assert_eq!(sscanf_int("x"), None);
        assert_eq!(sscanf_int("+"), None);
        assert_eq!(sscanf_int("\u{a0}\u{3000}\u{b}3"), Some(3));
        assert_eq!(sscanf_int("\r\n3"), None);
        assert_eq!(sscanf_int("\r3"), Some(3));
    }

    /// Go's ConfigV8 reads the file under its lock before the body. A save
    /// holding `state.disk` truncates then rewrites the file in place; a stale write's
    /// precondition must wait for it and answer with the finished file's ETag, not that
    /// of the empty file it would read unlocked.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_config_precondition_waits_for_an_in_place_save() {
        let dir = std::env::temp_dir().join(format!("cpa-v8-precheck-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        let config = format!("config-version: 8\noauth: {{auth-dir: {}}}\n", dir.display());
        std::fs::write(&path, &config).unwrap();
        let rt = Arc::new(crate::testing::runtime(
            cpa_core::config::Config::load(&path).unwrap(),
            Vec::new(),
            cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        let state = Management::with_options(rt, path.clone(), Default::default());
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let writer = {
            let (state, path, config) = (state.clone(), path.clone(), config.clone());
            std::thread::spawn(move || {
                let _disk = state.disk.lock().unwrap();
                std::fs::write(&path, "").unwrap();
                held_tx.send(()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(200));
                std::fs::write(&path, config).unwrap();
            })
        };
        held_rx.recv().unwrap();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::IF_MATCH, axum::http::HeaderValue::from_static("\"stale\""));
        let response = super::super::config(
            State(state.clone()),
            OriginalUri("/v8/management/config/observability/logs/debug".parse().unwrap()),
            Method::PUT,
            headers,
            axum::body::Body::from("true"),
        )
        .await;
        writer.join().unwrap();
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
        assert_eq!(
            response.headers()[header::ETAG],
            super::super::config_etag(config.as_bytes())
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
