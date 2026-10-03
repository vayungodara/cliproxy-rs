//! Go's v0 management routes (`internal/api/server_management.go`) that predate the v8
//! tree. Reads render Go's runtime `Config` view ([`view`]); writes go through the v8
//! config writer ([`super::config_sync`]) and the v8 OAuth and logs handlers, so v0 and
//! v8 share one implementation as they do in Go.
//!
//! Deliberate difference: Go's v0 saver keeps a legacy-layout file in the legacy
//! layout; the shared v8 writer migrates it on the first write, as a v8 write does.
use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{OriginalUri, RawQuery, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use cpa_core::credential::Source;
use serde_json::{Value, json};

use super::{Management, json_error};

mod decode;
mod keys;
mod lists;
mod view;

/// Serializes v0 read-modify-write handlers, as Go's handler mutex does.
static WRITES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    match go_trim(s).to_lowercase().as_str() {
        "" | "round-robin" | "roundrobin" | "rr" => Some("round-robin"),
        "weighted-round-robin" | "weightedroundrobin" | "wrr" => Some("weighted-round-robin"),
        "fill-first" | "fillfirst" | "ff" => Some("fill-first"),
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

/// Writes one v8 path through the shared config writer; Go's v0 success body.
async fn write(state: Arc<Management>, v8: String, value: Value) -> Response {
    let path = format!("/v8/management/config/{v8}");
    let body = value.to_string().into_bytes();
    let res = tokio::task::spawn_blocking(move || super::config_sync(&state, &path, Method::PUT, &body))
        .await
        .unwrap_or_else(|_| json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error"));
    if res.status() == StatusCode::OK {
        ok()
    } else {
        save_error(res).await
    }
}

/// Saves one v8 path unchanged, as Go saves after a handler that changed nothing.
/// ponytail: a path absent from the document is not saved at all; Go would still
/// rewrite the file (and fail on a read-only disk).
async fn touch(state: Arc<Management>, v8: String) -> Response {
    let cfg = state.rt.config();
    let current = v8
        .split('/')
        .try_fold(&cfg.document, |v, k| v.get(k))
        .and_then(|v| serde_json::to_value(v).ok());
    match current {
        Some(value) => write(state, v8, value).await,
        None => ok(),
    }
}

/// Deletes one v8 path through the shared config writer (absent counts as done).
async fn remove(state: Arc<Management>, v8: String) -> Response {
    let path = format!("/v8/management/config/{v8}");
    let res = tokio::task::spawn_blocking(move || super::config_sync(&state, &path, Method::DELETE, b""))
        .await
        .unwrap_or_else(|_| json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error"));
    match res.status() {
        StatusCode::OK | StatusCode::NOT_FOUND => ok(),
        _ => save_error(res).await,
    }
}

/// Go's scalar v0 routes: GET reads the runtime value, PUT and PATCH take
/// `{"value": ...}`, DELETE (proxy-url only) clears it.
pub(crate) async fn field_route(
    State(state): State<Arc<Management>>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    body: Bytes,
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
        return write(state, f.v8.into(), Value::from("")).await;
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
    write(state, f.v8.into(), value).await
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
    body: Bytes,
) -> Response {
    let _serial = WRITES.lock().await;
    let cfg = current(&state);
    let mut keys: Vec<Value> = cfg["api-keys"].as_array().cloned().unwrap_or_default();
    let v8 = "access/api-keys".to_owned();
    match method {
        Method::GET => go_json(StatusCode::OK, &json!({"api-keys": cfg["api-keys"]})),
        Method::PUT => {
            // Go `putStringList`.
            let Some(list) = decode::put_collection(view::field("api-keys"), &body, true) else {
                return bad("invalid body");
            };
            // Go copies with append([]string(nil), v...): an empty list becomes nil.
            let list = list.as_array().filter(|l| !l.is_empty()).cloned();
            write(state, v8, list.map_or(Value::Null, Value::Array)).await
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
                return bad("invalid body");
            };
            let text = |k: &str| b.get(k).and_then(Value::as_str).map(str::to_owned);
            let (old, new, value) = (text("old"), text("new"), text("value"));
            let index = b.get("index").and_then(Value::as_i64);
            if let (Some(i), Some(v)) = (index, &value)
                && i >= 0
                && (i as usize) < keys.len()
            {
                keys[i as usize] = Value::from(v.clone());
                return write(state, v8, Value::Array(keys)).await;
            }
            if let (Some(old), Some(new)) = (old, new) {
                match keys.iter_mut().find(|k| k.as_str() == Some(old.as_str())) {
                    Some(slot) => *slot = Value::from(new),
                    None => keys.push(Value::from(new)),
                }
                return write(state, v8, Value::Array(keys)).await;
            }
            bad("missing fields")
        }
        Method::DELETE => {
            let raw = raw.as_deref();
            if let Some(i) = query_first(raw, "index")
                .filter(|s| !s.is_empty())
                .and_then(|s| sscanf_int(&s))
                && i >= 0
                && (i as usize) < keys.len()
            {
                keys.remove(i as usize);
                return write(state, v8, Value::Array(keys)).await;
            }
            if let Some(value) = query_first(raw, "value")
                .map(|v| go_trim(&v).to_owned())
                .filter(|v| !v.is_empty())
            {
                keys.retain(|k| go_trim(k.as_str().unwrap_or_default()) != value);
                return write(state, v8, Value::Array(keys)).await;
            }
            bad("missing index or value")
        }
        _ => super::access::not_found(),
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
    body: Bytes,
) -> Response {
    let route = route_of(&uri).to_owned();
    if method != Method::GET {
        return lists::change(state, &route, method, raw.as_deref(), &body).await;
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

/// POST /v0/management/vertex/import: v8 `/oauth/import?provider=vertex`.
pub(crate) async fn vertex_import(RawQuery(raw): RawQuery) -> Response {
    let query = match raw.filter(|r| !r.is_empty()) {
        Some(raw) => format!("provider=vertex&{raw}"),
        None => "provider=vertex".to_owned(),
    };
    super::oauth::import(RawQuery(Some(query))).await
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn sscanf_reads_an_integer_prefix() {
        assert_eq!(sscanf_int("1"), Some(1));
        assert_eq!(sscanf_int(" -2x"), Some(-2));
        assert_eq!(sscanf_int("x"), None);
        assert_eq!(sscanf_int("+"), None);
        assert_eq!(sscanf_int("\u{a0}\u{3000}\u{b}3"), Some(3));
        assert_eq!(sscanf_int("\r\n3"), None);
        assert_eq!(sscanf_int("\r3"), Some(3));
    }
}
