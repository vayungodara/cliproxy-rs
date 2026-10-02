//! `POST /requests/api-call` (Go api_tools.go `APICall`): one HTTP request made on the
//! management caller's behalf, optionally with a credential's token substituted for
//! `$TOKEN$` and through that credential's proxy.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use cpa_core::config::{credentials, go_url};
use cpa_core::credential::Credential;
use cpa_exec::proxy::{self, GoHeaders, Proxy, Route};
use serde_json::{Map, Value, json};

use super::auth_files::fail;
use super::{Management, json as respond};

const TIMEOUT: Duration = Duration::from_secs(60);
/// ponytail: Go reads the upstream body without a bound; 64 MiB keeps a hostile
/// upstream from exhausting a small VPS and covers every provider usage endpoint.
const MAX_BODY: usize = 64 << 20;

/// The request body as Go's `apiCallRequest` decodes it.
#[derive(Default)]
struct Request {
    auth_index: [Option<String>; 3],
    method: String,
    url: String,
    proxy_url: String,
    header: Option<Vec<(String, String)>>,
    data: String,
}

/// gin `ShouldBindJSON` into `apiCallRequest`: the first JSON value only (trailing data
/// is ignored), field names matched exactly first and then case-insensitively in
/// struct order, a type mismatch anywhere fails the bind.
fn decode(body: &[u8]) -> Option<Request> {
    const FIELDS: [&str; 8] = [
        "auth_index",
        "authIndex",
        "AuthIndex",
        "method",
        "url",
        "proxy_url",
        "header",
        "data",
    ];
    let value = serde_json::Deserializer::from_slice(body)
        .into_iter::<Value>()
        .next()?
        .ok()?;
    let mut req = Request::default();
    let object = match value {
        Value::Null => return Some(req),
        Value::Object(o) => o,
        _ => return None,
    };
    let text = |v: &Value, slot: &mut String| -> Option<()> {
        match v {
            Value::String(s) => *slot = s.clone(),
            Value::Null => {}
            _ => return None,
        }
        Some(())
    };
    for (key, v) in &object {
        let field = FIELDS
            .iter()
            .position(|f| f == key)
            .or_else(|| FIELDS.iter().position(|f| f.eq_ignore_ascii_case(key)));
        match field {
            Some(i @ 0..=2) => {
                req.auth_index[i] = match v {
                    Value::String(s) => Some(s.clone()),
                    Value::Null => None,
                    _ => return None,
                }
            }
            Some(3) => text(v, &mut req.method)?,
            Some(4) => text(v, &mut req.url)?,
            Some(5) => text(v, &mut req.proxy_url)?,
            Some(6) => {
                req.header = match v {
                    Value::Null => None,
                    Value::Object(m) => Some(
                        m.iter()
                            .map(|(k, v)| match v {
                                Value::String(s) => Some((k.clone(), s.clone())),
                                Value::Null => Some((k.clone(), String::new())),
                                _ => None,
                            })
                            .collect::<Option<_>>()?,
                    ),
                    _ => return None,
                }
            }
            Some(7) => text(v, &mut req.data)?,
            _ => {}
        }
    }
    Some(req)
}

/// Go `tokenValueFromMetadata` then the `api_key` / `session_token` attributes.
fn token_for(c: &Credential) -> String {
    let meta = |key: &str| {
        c.metadata
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let nested = || {
        let token = c.metadata.get("token")?.as_object()?;
        ["access_token", "accessToken"]
            .iter()
            .filter_map(|k| token.get(*k)?.as_str())
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(str::to_owned)
    };
    meta("accessToken")
        .or_else(|| meta("access_token"))
        .or_else(nested)
        .or_else(|| meta("token"))
        .or_else(|| meta("id_token"))
        .or_else(|| meta("api_key"))
        .or_else(|| meta("session_token"))
        .or_else(|| meta("cookie"))
        .or_else(|| {
            ["api_key", "session_token"]
                .iter()
                .filter_map(|k| c.attributes.get(*k))
                .map(|v| v.trim())
                .find(|v| !v.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

/// Go `apiCallTransport`: the request's proxy, else the first usable of the
/// credential's and the global proxy, else a direct connection (environment proxies
/// are never used here).
fn transport(state: &Management, credential: Option<&Credential>, request_proxy: &str) -> Proxy {
    if !request_proxy.is_empty() {
        return Proxy::parse(request_proxy);
    }
    let cfg = state.rt.config();
    let own = credential.and_then(|c| {
        c.attributes
            .get("proxy_url")
            .map(String::as_str)
            .or_else(|| c.str("proxy_url"))
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
    });
    let global = cfg
        .document
        .get("requests")
        .and_then(|r| r.get("proxy-url"))
        .and_then(serde_yaml_ng::Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_owned);
    own.into_iter()
        .chain(global)
        .map(|p| Proxy::parse(&p))
        .find(|p| matches!(p, Proxy::Url(_) | Proxy::Direct))
        .unwrap_or(Proxy::Direct)
}

pub(super) async fn api_call(State(state): State<Arc<Management>>, body: Bytes) -> Response {
    let Some(mut req) = decode(&body) else {
        return fail(StatusCode::BAD_REQUEST, "invalid body");
    };
    let method = req.method.trim().to_uppercase();
    if method.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "missing method");
    }
    let url = req.url.trim().to_owned();
    if url.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "missing url");
    }
    if !go_url::parse(&url).is_some_and(|u| !u.scheme.is_empty() && !u.host().is_empty()) {
        return fail(StatusCode::BAD_REQUEST, "invalid url");
    }
    let request_proxy = req.proxy_url.trim().to_owned();
    if !request_proxy.is_empty() && matches!(Proxy::parse(&request_proxy), Proxy::Invalid) {
        return fail(StatusCode::BAD_REQUEST, "invalid proxy_url");
    }
    let auth_index = req
        .auth_index
        .iter()
        .flatten()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .unwrap_or_default()
        .to_owned();
    let credential = (!auth_index.is_empty())
        .then(|| {
            state
                .rt
                .store()
                .snapshot()
                .into_iter()
                .find(|c| credentials::auth_index(c) == auth_index)
        })
        .flatten();
    // ponytail: Go refreshes antigravity, meta and xai tokens here when due; those
    // providers have no executor in cliproxy-rs yet, so their stored token is used.
    let token = credential.as_deref().map(token_for).unwrap_or_default();
    let token_error = || {
        if !auth_index.is_empty() && credential.is_none() {
            "auth credential not found for auth_index"
        } else {
            "auth token not found"
        }
    };
    let mut headers = req.header.take().unwrap_or_default();
    for (_, value) in headers.iter_mut() {
        if value.contains("$TOKEN$") {
            if token.is_empty() {
                return fail(StatusCode::BAD_REQUEST, token_error());
            }
            *value = value.replace("$TOKEN$", &token);
        }
    }
    if req.data.contains("$TOKEN$") {
        if token.is_empty() {
            return fail(StatusCode::BAD_REQUEST, token_error());
        }
        let valid_json = serde_json::from_str::<serde::de::IgnoredAny>(&req.data).is_ok();
        let replacement = if valid_json && token.contains(['"', '\\', '\r', '\n', '\t']) {
            let mut quoted = Vec::new();
            cpa_common::json::marshal_str(&mut quoted, token.as_bytes(), true);
            String::from_utf8_lossy(&quoted[1..quoted.len() - 1]).into_owned()
        } else {
            token.clone()
        };
        req.data = req.data.replace("$TOKEN$", &replacement);
    }
    let Ok(method) = axum::http::Method::from_bytes(method.as_bytes()) else {
        return fail(StatusCode::BAD_REQUEST, "failed to build request");
    };
    let mut go_headers = GoHeaders::new();
    for (key, value) in headers {
        if key.eq_ignore_ascii_case("host") {
            let host = value.trim();
            if !host.is_empty() {
                go_headers.set("Host", host);
            }
            continue;
        }
        go_headers.set(&key, value);
    }
    let client = state
        .clients
        .get(&transport(&state, credential.as_deref(), &request_proxy));
    let body = (!req.data.is_empty()).then(|| Bytes::from(req.data));
    let upstream = proxy::send_request(
        &|_| {
            Ok(Route {
                client: client.clone(),
                order: None,
            })
        },
        method,
        &url,
        go_headers,
        body,
        Some(TIMEOUT),
    )
    .await;
    let upstream = match upstream {
        Ok(u) => u,
        Err(_) => return fail(StatusCode::BAD_GATEWAY, "request failed"),
    };
    let status = upstream.status;
    let mut header: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for (name, value) in upstream.headers.iter() {
        header
            .entry(proxy::canonical_header(name.as_str()))
            .or_default()
            .push(String::from_utf8_lossy(value.as_bytes()).into_owned().into());
    }
    let data = match proxy::read_all(upstream.body, MAX_BODY + 1, false).await {
        Ok(d) if d.len() <= MAX_BODY => d,
        _ => return fail(StatusCode::BAD_GATEWAY, "failed to read response"),
    };
    let header: Map<String, Value> = header.into_iter().map(|(k, v)| (k, Value::Array(v))).collect();
    respond(
        StatusCode::OK,
        &json!({
            "status_code": status,
            "header": header,
            "body": String::from_utf8_lossy(&data),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_like_gin_should_bind_json() {
        assert!(decode(b"").is_none());
        assert!(decode(b"  ").is_none());
        assert!(decode(b"[]").is_none());
        assert!(decode(br#"{"method":1}"#).is_none());
        assert!(decode(br#"{"header":{"a":1}}"#).is_none());
        let r = decode(br#"{"METHOD":"get","AuthIndex":"p","authindex":"c","header":{"x":null}} trailing"#).unwrap();
        assert_eq!(r.method, "get");
        assert_eq!(r.auth_index, [None, Some("c".into()), Some("p".into())]);
        assert_eq!(r.header.unwrap(), [("x".to_owned(), String::new())]);
        assert_eq!(decode(b"null").unwrap().method, "");
    }
}
