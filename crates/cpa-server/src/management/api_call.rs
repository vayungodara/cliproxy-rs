//! `POST /requests/api-call` (Go api_tools.go `APICall`): one HTTP request made on the
//! management caller's behalf, optionally with a credential's token substituted for
//! `$TOKEN$` and through that credential's proxy.

use cpa_core::gostr::GoStr;
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

use super::Management;
use super::auth_files::{auth_kind, fail};

const TIMEOUT: Duration = Duration::from_secs(60);
/// ponytail: Go reads the upstream body without a bound; 64 MiB keeps a hostile
/// upstream from exhausting a small VPS and covers every provider usage endpoint.
const MAX_BODY: usize = 64 << 20;
const MAX_HEADERS: usize = 16_384;

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

/// The members of the first JSON value in `body` (what follows it is never read) that
/// `select` names, in order with duplicates kept; empty for null; `None` when the value
/// is not an object. Other members are skipped unconverted (`IgnoredAny`), as
/// `encoding/json` skips unknown fields.
fn first_members<K>(body: &[u8], select: impl Fn(&str) -> Option<K>) -> Option<Vec<(K, Value)>> {
    use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, Visitor};
    struct Seed<F>(F);
    impl<'de, K, F: Fn(&str) -> Option<K>> DeserializeSeed<'de> for Seed<F> {
        type Value = Vec<(K, Value)>;
        fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
            d.deserialize_any(self)
        }
    }
    impl<'de, K, F: Fn(&str) -> Option<K>> Visitor<'de> for Seed<F> {
        type Value = Vec<(K, Value)>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("an object or null")
        }
        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(key) = map.next_key::<String>()? {
                match (self.0)(&key) {
                    Some(k) => out.push((k, map.next_value()?)),
                    None => {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
            }
            Ok(out)
        }
    }
    Seed(select)
        .deserialize(&mut serde_json::Deserializer::from_slice(body))
        .ok()
}

/// gin `ShouldBindJSON` into a struct with these JSON field names: each matching
/// member as its field's index, in order; see [`first_members`].
pub(super) fn bind_struct(body: &[u8], fields: &[&str]) -> Option<Vec<(usize, Value)>> {
    first_members(body, |key| go_field(fields, key))
}

/// The struct field `key` decodes into (`encoding/json`): an exact name first, else
/// the first name equal under Unicode case folding.
pub(super) fn go_field(fields: &[&str], key: &str) -> Option<usize> {
    fields
        .iter()
        .position(|f| *f == key)
        .or_else(|| fields.iter().position(|f| f.go_eq_fold(key)))
}

/// gin `ShouldBindJSON` into `apiCallRequest`: the first JSON value only (trailing data
/// is ignored); every member applied in order with field names matched exactly first
/// and then under Unicode case folding in struct order; null leaves a string as it was, clears
/// a pointer and nils the header map; a later header object merges into the map; any
/// type mismatch fails the bind.
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
    let members = bind_struct(body, &FIELDS)?;
    let mut req = Request::default();
    let text = |v: &Value, slot: &mut String| -> Option<()> {
        match v {
            Value::String(s) => *slot = s.clone(),
            Value::Null => {}
            _ => return None,
        }
        Some(())
    };
    for (field, v) in members {
        match field {
            i @ 0..=2 => {
                req.auth_index[i] = match v {
                    Value::String(s) => Some(s),
                    Value::Null => None,
                    _ => return None,
                }
            }
            3 => text(&v, &mut req.method)?,
            4 => text(&v, &mut req.url)?,
            5 => text(&v, &mut req.proxy_url)?,
            6 => match v {
                Value::Null => req.header = None,
                Value::Object(m) => {
                    let header = req.header.get_or_insert_with(Vec::new);
                    for (k, v) in m {
                        let v = match v {
                            Value::String(s) => s,
                            Value::Null => String::new(),
                            _ => return None,
                        };
                        match header.iter_mut().find(|(name, _)| *name == k) {
                            Some(slot) => slot.1 = v,
                            None => header.push((k, v)),
                        }
                    }
                }
                _ => return None,
            },
            _ => text(&v, &mut req.data)?,
        }
    }
    Some(req)
}

/// Go `tokenValueFromMetadata` then the `api_key` / `session_token` attributes.
pub(super) fn token_for(c: &Credential) -> String {
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

/// Go `apiCallTransport`: the request's proxy (direct if it cannot be built), else
/// the first buildable of the credential's own proxy, its API-key config entry's proxy
/// and the global proxy, else a direct connection. Environment proxies are never
/// used; `None` only if not even a direct client can be built.
pub(super) fn client_for(
    state: &Management,
    credential: Option<&Credential>,
    request_proxy: &str,
) -> Option<wreq::Client> {
    let direct = || state.clients.try_get(&Proxy::Direct);
    let build = |raw: &str| match Proxy::parse(raw) {
        p @ (Proxy::Url(_) | Proxy::Direct) => state.clients.try_get(&p),
        Proxy::Inherit | Proxy::Invalid => None,
    };
    if !request_proxy.is_empty() {
        return build(request_proxy).or_else(direct);
    }
    let cfg = state.rt.config();
    let mut candidates: Vec<String> = Vec::new();
    if let Some(c) = credential {
        let own = c
            .attributes
            .get("proxy_url")
            .map(String::as_str)
            .or_else(|| c.str("proxy_url"))
            .unwrap_or_default();
        candidates.push(own.trim().to_owned());
        if auth_kind(c) == Some("apikey") {
            candidates.push(credentials::api_key_config_proxy(&cfg, c));
        }
    }
    let global = cfg
        .document
        .get("requests")
        .and_then(|r| r.get("proxy-url"))
        .and_then(serde_yaml_ng::Value::as_str)
        .unwrap_or_default();
    candidates.push(global.trim().to_owned());
    candidates
        .iter()
        .filter(|p| !p.is_empty())
        .find_map(|p| build(p))
        .or_else(direct)
}

pub(crate) async fn api_call(
    State(state): State<Arc<Management>>,
    body: Result<crate::body_limit::FirstJson, axum::extract::rejection::BytesRejection>,
) -> Response {
    let body = match body {
        Ok(crate::body_limit::FirstJson(body)) => body,
        Err(rejection) => return fail(super::read_status(&rejection), "invalid body"),
    };
    let Some(mut req) = decode(&body) else {
        return fail(StatusCode::BAD_REQUEST, "invalid body");
    };
    let method = req.method.trim().go_upper();
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
    // ponytail: `http::HeaderMap` panics past 32768 entries where Go's map would not;
    // such a request is refused instead of sent.
    if headers.len() > MAX_HEADERS {
        return fail(StatusCode::BAD_GATEWAY, "request failed");
    }
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
    let Some(client) = client_for(&state, credential.as_deref(), &request_proxy) else {
        return fail(StatusCode::BAD_GATEWAY, "request failed");
    };
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
    // Go's transport moves these out of `Response.Header`: `Transfer-Encoding` always,
    // and a chunked response's `Trailer` declaration.
    let chunked = upstream
        .headers
        .get_all("transfer-encoding")
        .iter()
        .any(|v| v.to_str().is_ok_and(|v| v.to_ascii_lowercase().contains("chunked")));
    let mut header: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for (name, value) in upstream.headers.iter() {
        if name == "transfer-encoding" || (chunked && name == "trailer") {
            continue;
        }
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
    // apiCallResponse struct: status_code, header, body.
    super::json_ordered(
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
        // Go applies every duplicate member: a type error anywhere fails, null keeps a
        // string, header objects merge.
        assert!(decode(br#"{"method":1,"method":"GET"}"#).is_none());
        assert_eq!(decode(br#"{"method":"GET","method":null}"#).unwrap().method, "GET");
        let merged = decode(br#"{"header":{"a":"1","b":"x"},"header":{"b":"2"}}"#).unwrap();
        assert_eq!(
            merged.header.unwrap(),
            [("a".to_owned(), "1".to_owned()), ("b".to_owned(), "2".to_owned())]
        );
        assert!(
            decode(br#"{"header":{"a":"1"},"header":null}"#)
                .unwrap()
                .header
                .is_none()
        );
        assert_eq!(
            decode(br#"{"auth_index":"x","auth_index":null}"#).unwrap().auth_index[0],
            None
        );
    }
}
