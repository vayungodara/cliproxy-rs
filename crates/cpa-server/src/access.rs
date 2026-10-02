//! Client-key auth, matching CLIProxyAPI's config access provider
//! (internal/access/config_access/provider.go).
//!
//! A key may arrive as `Authorization: Bearer`, `X-Goog-Api-Key`, `X-Api-Key`, or the
//! `key` / `auth_token` query parameters. With no keys configured, access is open.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::AppState;

pub async fn require_client_key(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let keys = &state.config.api_keys;
    if keys.is_empty() {
        return next.run(req).await;
    }
    let candidates = candidates(req.headers(), req.uri().query().unwrap_or_default());
    if candidates.is_empty() {
        return reject("Missing API key");
    }
    if candidates.iter().any(|c| keys.iter().any(|k| k == c)) {
        next.run(req).await
    } else {
        reject("Invalid API key")
    }
}

fn reject(message: &str) -> Response {
    (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": message }))).into_response()
}

fn candidates(headers: &HeaderMap, query: &str) -> Vec<String> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let mut out = vec![
        bearer(header("authorization")),
        header("x-goog-api-key").to_owned(),
        header("x-api-key").to_owned(),
    ];
    for (name, value) in query.split('&').filter_map(|pair| pair.split_once('=')) {
        if name == "key" || name == "auth_token" {
            out.push(percent_decode(value));
        }
    }
    out.retain(|c| !c.is_empty());
    out
}

/// Go's extractBearerToken: "Bearer x" yields "x"; anything else is used verbatim.
fn bearer(header: &str) -> String {
    match header.split_once(' ') {
        Some((scheme, token)) if scheme.eq_ignore_ascii_case("bearer") => token.trim().to_owned(),
        _ => header.to_owned(),
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = (bytes[i] == b'%')
            .then(|| s.get(i + 1..i + 3))
            .flatten()
            .filter(|h| h.bytes().all(|c| c.is_ascii_hexdigit()))
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], escaped) {
            (_, Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b'+', None) => {
                out.push(b' ');
                i += 1;
            }
            (b, None) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_matches_go() {
        assert_eq!(bearer("Bearer abc"), "abc");
        assert_eq!(bearer("bearer  abc "), "abc");
        assert_eq!(bearer("Basic abc"), "Basic abc");
        assert_eq!(bearer("abc"), "abc");
    }

    #[test]
    fn query_keys_are_decoded() {
        let got = candidates(&HeaderMap::new(), "x=1&key=a%2Bb&auth_token=c+d");
        assert_eq!(got, ["a+b", "c d"]);
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }
}
