//! Client-key auth, matching CLIProxyAPI's config access provider
//! (internal/access/config_access/provider.go, sdk/access/manager.go).
//!
//! Candidates, in order: `Authorization` (bearer token or the raw value), `X-Goog-Api-Key`,
//! `X-Api-Key`, then the first `key` and `auth_token` query values. With no keys
//! configured the provider is unregistered and access is open.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use cpa_core::exec::Caller;

use crate::runtime::Runtime;

pub async fn require_client_key(
    State(rt): State<Arc<Runtime>>,
    mut req: Request,
    next: Next,
) -> Response {
    let config = rt.config();
    let caller = match authenticate(
        &config.api_keys,
        req.headers(),
        req.uri().query().unwrap_or_default(),
    ) {
        Ok(caller) => caller,
        Err(message) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": message })),
            )
                .into_response();
        }
    };
    req.extensions_mut().insert(caller);
    next.run(req).await
}

fn authenticate(keys: &[String], headers: &HeaderMap, query: &str) -> Result<Caller, &'static str> {
    if keys.is_empty() {
        return Ok(Caller {
            principal: String::new(),
            source: "",
        });
    }
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    };
    let authorization = header("authorization");
    let (google, anthropic) = (header("x-goog-api-key"), header("x-api-key"));
    let query_key = query_get(query, "key").unwrap_or_default();
    let query_token = query_get(query, "auth_token").unwrap_or_default();
    // "Missing" is judged on the raw values, so `Authorization: Bearer ` is invalid, not missing.
    if [authorization, google, anthropic, &query_key, &query_token]
        .iter()
        .all(|v| v.is_empty())
    {
        return Err("Missing API key");
    }
    let bearer = bearer(authorization);
    let candidates = [
        (bearer.as_str(), "authorization"),
        (google, "x-goog-api-key"),
        (anthropic, "x-api-key"),
        (query_key.as_str(), "query-key"),
        (query_token.as_str(), "query-auth-token"),
    ];
    candidates
        .into_iter()
        .find(|(value, _)| !value.is_empty() && keys.iter().any(|k| k == value))
        .map(|(value, source)| Caller {
            principal: value.to_owned(),
            source,
        })
        .ok_or("Invalid API key")
}

/// Go's extractBearerToken: "Bearer x" yields "x"; anything else is used verbatim.
fn bearer(header: &str) -> String {
    match header.split_once(' ') {
        Some((scheme, token)) if scheme.eq_ignore_ascii_case("bearer") => token.trim().to_owned(),
        _ => header.to_owned(),
    }
}

/// `url.ParseQuery(q).Get(name)`: first value whose decoded name matches. Pairs with a
/// `;` or a malformed escape in the name or value are skipped, as Go does.
fn query_get(query: &str, name: &str) -> Option<String> {
    query
        .split('&')
        .filter(|p| !p.is_empty() && !p.contains(';'))
        .find_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let (k, v) = (unescape(k)?, unescape(v)?);
            (k == name).then_some(v)
        })
}

/// Go's QueryUnescape: `+` is a space, `%XX` must be two hex digits.
fn unescape(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes
                    .get(i + 1..i + 3)
                    .filter(|h| h.iter().all(u8::is_ascii_hexdigit))?;
                out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(query: &str, headers: &[(&'static str, &str)]) -> Result<Caller, &'static str> {
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            map.insert(*k, v.parse().unwrap());
        }
        authenticate(&["good".to_owned()], &map, query)
    }

    #[test]
    fn query_matches_go_parse_query() {
        assert_eq!(
            query_get("key=wrong&key=good", "key").as_deref(),
            Some("wrong"),
            "first value wins"
        );
        assert_eq!(
            query_get("%6bey=good", "key").as_deref(),
            Some("good"),
            "names are decoded"
        );
        assert_eq!(
            query_get("key=%zz&key=good", "key").as_deref(),
            Some("good"),
            "malformed pair skipped"
        );
        assert_eq!(
            query_get("key=a;b&key=good", "key").as_deref(),
            Some("good"),
            "semicolon pair skipped"
        );
        assert_eq!(query_get("key=a+b%2Bc", "key").as_deref(), Some("a b+c"));
        assert_eq!(query_get("key", "key").as_deref(), Some(""));
    }

    #[test]
    fn candidates_and_errors_match_go() {
        assert_eq!(auth("key=wrong&key=good", &[]), Err("Invalid API key"));
        assert_eq!(auth("%6bey=good", &[]).unwrap().source, "query-key");
        assert_eq!(auth("", &[]), Err("Missing API key"));
        assert_eq!(
            auth("", &[("authorization", "Bearer ")]),
            Err("Invalid API key")
        );
        assert_eq!(
            auth("", &[("authorization", "good")]).unwrap().source,
            "authorization"
        );
        let caller = auth("", &[("x-api-key", "nope"), ("x-goog-api-key", "good")]).unwrap();
        assert_eq!(
            (caller.principal.as_str(), caller.source),
            ("good", "x-goog-api-key")
        );
        assert_eq!(
            authenticate(&[], &HeaderMap::new(), "").unwrap().principal,
            "",
            "no keys means open"
        );
    }
}
