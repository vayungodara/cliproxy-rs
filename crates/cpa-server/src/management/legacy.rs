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
use axum::http::{Method, StatusCode};
use axum::response::Response;
use cpa_core::credential::Source;
use serde_json::{Value, json};

use super::api_call::Members;
use super::{Management, json_error};

mod keys;
mod view;

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

fn ok() -> Response {
    super::json(StatusCode::OK, &json!({"status": "ok"}))
}

fn bad(message: &str) -> Response {
    json_error(StatusCode::BAD_REQUEST, message)
}

/// The published runtime config as Go's `json.Marshal(cfg)` view.
fn current(state: &Management) -> Value {
    view::config(&state.rt.config().document)
}

/// GET /v0/management/config (Go `GetConfig`).
pub(crate) async fn config(State(state): State<Arc<Management>>) -> Response {
    super::json(StatusCode::OK, &current(&state))
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

/// gin `ShouldBindJSON` into `struct { Value *T }`: the first JSON value, members in
/// order with case-insensitive names; a type mismatch fails, null leaves it unset.
fn bind_value(body: &[u8], kind: Kind) -> Option<Value> {
    let Members(members) = serde_json::Deserializer::from_slice(body)
        .into_iter::<Members>()
        .next()?
        .ok()?;
    let mut value = None;
    for (key, v) in members.unwrap_or_default() {
        if !key.eq_ignore_ascii_case("value") {
            continue;
        }
        value = match (kind, v) {
            (_, Value::Null) => None,
            (Kind::Bool, v @ Value::Bool(_)) => Some(v),
            (Kind::Int, Value::Number(n)) => Some(Value::from(n.as_i64()?)),
            (Kind::Str, v @ Value::String(_)) => Some(v),
            _ => return None,
        };
    }
    value
}

/// Writes one v8 path through the shared config writer; Go's v0 success body.
async fn write(state: Arc<Management>, v8: String, value: Value) -> Response {
    let path = format!("/v8/management/config/{v8}");
    let body = value.to_string().into_bytes();
    let res = tokio::task::spawn_blocking(move || super::config_sync(&state, &path, Method::PUT, &body))
        .await
        .unwrap_or_else(|_| json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error"));
    if res.status() == StatusCode::OK { ok() } else { res }
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
        return super::json(StatusCode::OK, &json!({ f.key: value }));
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

/// Go `fmt.Sscanf(s, "%d", &n)`: an optionally signed decimal prefix after spaces.
fn sscanf_int(s: &str) -> Option<i64> {
    let s = s.trim_start_matches([' ', '\t', '\r']);
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
    let cfg = current(&state);
    let mut keys: Vec<Value> = cfg["api-keys"].as_array().cloned().unwrap_or_default();
    let v8 = "access/api-keys".to_owned();
    match method {
        Method::GET => super::json(StatusCode::OK, &json!({"api-keys": cfg["api-keys"]})),
        Method::PUT => {
            let list = match serde_json::from_slice::<Option<Vec<Option<String>>>>(&body) {
                Ok(list) => list.map(|l| l.into_iter().map(Option::unwrap_or_default).collect::<Vec<_>>()),
                Err(_) => match serde_json::from_slice::<Value>(&body)
                    .ok()
                    .and_then(|v| v.get("items").cloned())
                    .and_then(|i| serde_json::from_value::<Vec<Option<String>>>(i).ok())
                {
                    Some(items) if !items.is_empty() => {
                        Some(items.into_iter().map(Option::unwrap_or_default).collect())
                    }
                    _ => return bad("invalid body"),
                },
            };
            // Go copies with append([]string(nil), v...): an empty list becomes nil.
            let list = list.filter(|l| !l.is_empty());
            write(state, v8, list.map_or(Value::Null, Value::from)).await
        }
        Method::PATCH => {
            let Some(Members(members)) = serde_json::Deserializer::from_slice(&body)
                .into_iter::<Members>()
                .next()
                .and_then(Result::ok)
            else {
                return bad("invalid body");
            };
            let (mut old, mut new, mut index, mut value) = (None, None, None, None);
            for (k, v) in members.unwrap_or_default() {
                let slot = match k.to_ascii_lowercase().as_str() {
                    "old" => &mut old,
                    "new" => &mut new,
                    "value" => &mut value,
                    "index" => {
                        index = match v {
                            Value::Null => None,
                            Value::Number(n) => match n.as_i64() {
                                Some(i) => Some(i),
                                None => return bad("invalid body"),
                            },
                            _ => return bad("invalid body"),
                        };
                        continue;
                    }
                    _ => continue,
                };
                *slot = match v {
                    Value::Null => None,
                    Value::String(s) => Some(s),
                    _ => return bad("invalid body"),
                };
            }
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

/// GET of Go's provider key lists (with `auth-index`) and OAuth maps.
pub(crate) async fn list_route(State(state): State<Arc<Management>>, OriginalUri(uri): OriginalUri) -> Response {
    let route = route_of(&uri).to_owned();
    let cfg = current(&state);
    let value = if route.starts_with("oauth-") {
        cfg.get(&route).cloned().unwrap_or(Value::Null)
    } else {
        keys::with_auth_index(&route, &cfg[&route], &live_indexes(&state))
    };
    super::json(StatusCode::OK, &json!({ route: value }))
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
        assert_eq!(bind_value(b"", Kind::Bool), None);
        assert_eq!(bind_value(b"null", Kind::Bool), None);
    }

    #[test]
    fn sscanf_reads_an_integer_prefix() {
        assert_eq!(sscanf_int("1"), Some(1));
        assert_eq!(sscanf_int(" -2x"), Some(-2));
        assert_eq!(sscanf_int("x"), None);
        assert_eq!(sscanf_int("+"), None);
    }
}
