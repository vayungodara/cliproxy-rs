//! Client-key auth, matching CLIProxyAPI's config access provider
//! (internal/access/config_access/provider.go, sdk/access/manager.go).
//!
//! Candidates, in order: `Authorization` (bearer token or the raw value), `X-Goog-Api-Key`,
//! `X-Api-Key`, then the first `key` and `auth_token` query values. Comparison is on raw
//! bytes, like Go strings. With no keys configured the provider is unregistered and
//! access is open.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use cpa_core::exec::Caller;

use crate::runtime::Runtime;

/// Go `AuthMiddleware`: client keys, then plugin frontend auth providers.
pub async fn require_client_key(State(rt): State<Arc<Runtime>>, req: Request, next: Next) -> Response {
    let (mut req, access) = crate::plugins::authenticate(&rt, req).await;
    let caller = match access {
        Ok(crate::plugins::Access::Open) => Caller {
            principal: String::new(),
            source: "",
        },
        Ok(crate::plugins::Access::Granted { caller, .. }) => caller,
        // gin `AbortWithStatusJSON(status, gin.H{"error": msg})`.
        Err(denied) => {
            let body = crate::gojson::Obj::new().str("error", denied.message).finish();
            return crate::respond::gin_json(denied.status, body);
        }
    };
    req.extensions_mut().insert(caller);
    let query = req.uri().query().unwrap_or_default().to_owned();
    crate::plugins::execution::with_query(&query, next.run(req)).await
}

pub(crate) fn authenticate(keys: &[String], headers: &HeaderMap, query: &str) -> Result<Caller, &'static str> {
    if keys.is_empty() {
        return Ok(Caller {
            principal: String::new(),
            source: "",
        });
    }
    let header = |name: &str| headers.get(name).map(|v| v.as_bytes()).unwrap_or_default();
    let authorization = header("authorization");
    let query_key = query_get(query, "key").unwrap_or_default();
    let query_token = query_get(query, "auth_token").unwrap_or_default();
    let bearer = bearer(authorization);
    let candidates: [(&[u8], &'static str); 5] = [
        (bearer, "authorization"),
        (header("x-goog-api-key"), "x-goog-api-key"),
        (header("x-api-key"), "x-api-key"),
        (&query_key, "query-key"),
        (&query_token, "query-auth-token"),
    ];
    // "Missing" is judged on the raw values, so `Authorization: Bearer ` is invalid, not missing.
    if authorization.is_empty() && candidates[1..].iter().all(|(v, _)| v.is_empty()) {
        return Err("Missing API key");
    }
    candidates
        .into_iter()
        .filter(|(value, _)| !value.is_empty())
        .find_map(|(value, source)| {
            keys.iter().find(|k| k.as_bytes() == value).map(|k| Caller {
                principal: k.clone(),
                source,
            })
        })
        .ok_or("Invalid API key")
}

/// Go's extractBearerToken: "Bearer x" yields "x"; anything else is used verbatim.
fn bearer(header: &[u8]) -> &[u8] {
    match header.iter().position(|&b| b == b' ') {
        Some(i) if header[..i].eq_ignore_ascii_case(b"bearer") => header[i + 1..].trim_ascii(),
        _ => header,
    }
}

/// `url.ParseQuery(q).Get(name)`: the first value whose decoded name matches. Pairs with
/// a `;` or a malformed escape in the name or value are skipped, as Go does.
pub(crate) fn query_get(query: &str, name: &str) -> Option<Vec<u8>> {
    query
        .split('&')
        .filter(|p| !p.is_empty() && !p.contains(';'))
        .find_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let (k, v) = (unescape(k)?, unescape(v)?);
            (k == name.as_bytes()).then_some(v)
        })
}

/// Go's QueryUnescape: `+` is a space, `%XX` must be two hex digits, any bytes allowed.
pub(crate) fn unescape(s: &str) -> Option<Vec<u8>> {
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
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth_with(keys: &[&str], query: &str, headers: &[(&'static str, &str)]) -> Result<Caller, &'static str> {
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            map.insert(*k, v.parse().unwrap());
        }
        let keys: Vec<String> = keys.iter().map(|k| (*k).to_owned()).collect();
        authenticate(&keys, &map, query)
    }

    fn auth(query: &str, headers: &[(&'static str, &str)]) -> Result<Caller, &'static str> {
        auth_with(&["good"], query, headers)
    }

    #[test]
    fn query_matches_go_parse_query() {
        let get = |q, n| query_get(q, n).map(|v| String::from_utf8(v).unwrap());
        assert_eq!(
            get("key=wrong&key=good", "key").as_deref(),
            Some("wrong"),
            "first value wins"
        );
        assert_eq!(get("%6bey=good", "key").as_deref(), Some("good"), "names are decoded");
        assert_eq!(
            get("key=%zz&key=good", "key").as_deref(),
            Some("good"),
            "malformed pair skipped"
        );
        assert_eq!(
            get("key=a;b&key=good", "key").as_deref(),
            Some("good"),
            "semicolon pair skipped"
        );
        assert_eq!(get("key=a+b%2Bc", "key").as_deref(), Some("a b+c"));
        assert_eq!(get("key", "key").as_deref(), Some(""));
        assert_eq!(
            query_get("key=%FF&key=good", "key"),
            Some(vec![0xFF]),
            "invalid UTF-8 still wins first"
        );
    }

    #[test]
    fn candidates_and_errors_match_go() {
        assert_eq!(auth("key=wrong&key=good", &[]), Err("Invalid API key"));
        assert_eq!(auth("key=%FF&key=good", &[]), Err("Invalid API key"));
        assert_eq!(auth("%6bey=good", &[]).unwrap().source, "query-key");
        assert_eq!(auth("", &[]), Err("Missing API key"));
        assert_eq!(auth("", &[("authorization", "Bearer ")]), Err("Invalid API key"));
        assert_eq!(auth("", &[("authorization", "good")]).unwrap().source, "authorization");
        let caller = auth("", &[("x-api-key", "nope"), ("x-goog-api-key", "good")]).unwrap();
        assert_eq!((caller.principal.as_str(), caller.source), ("good", "x-goog-api-key"));
        assert_eq!(auth_with(&[], "", &[]).unwrap().principal, "", "no keys means open");
    }

    #[test]
    fn replacement_char_key_does_not_match_raw_ff() {
        assert_eq!(auth_with(&["a\u{FFFD}"], "key=a%FF", &[]), Err("Invalid API key"));
        assert_eq!(
            auth_with(&["a\u{FFFD}"], "key=a%EF%BF%BD", &[]).unwrap().principal,
            "a\u{FFFD}"
        );
    }
}
