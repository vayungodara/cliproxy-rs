//! Client-key auth, matching CLIProxyAPI's config access provider
//! (internal/access/config_access/provider.go, sdk/access/manager.go).
//!
//! Candidates, in order: `Authorization` (bearer token or the raw value), `X-Goog-Api-Key`,
//! `X-Api-Key`, then the first `key` and `auth_token` query values. Comparison is on raw
//! bytes, like Go strings. With no keys configured the provider is unregistered. Go is
//! then open to everyone; cliproxy-rs serves local clients only unless
//! `access.allow-open-access` is set ([`refuse_nonlocal`], docs/DIFFERENCES-FROM-GO.md).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, Uri, header};
use axum::middleware::Next;
use axum::response::Response;
use cpa_core::config::Config;
use cpa_core::exec::Caller;

use crate::runtime::Runtime;

/// Headers a reverse proxy or tunnel adds. A loopback peer that sends one is relaying
/// another machine's request.
const FORWARDING: [&str; 4] = ["forwarded", "x-forwarded-for", "x-real-ip", "cf-connecting-ip"];

/// With no client-key or plugin auth provider, whether every client is still served:
/// `access.allow-open-access`, or Home mode, where Home checks each request's key
/// (Home's config never carries `access.api-keys`).
pub(crate) fn open_access(rt: &Runtime, cfg: &Config) -> bool {
    cfg.allow_open_access() || rt.remote_dispatch().is_some()
}

/// The keyless local-only mode: no auth provider and no opt-in.
pub(crate) fn local_only(rt: &Runtime) -> bool {
    let cfg = rt.config();
    !open_access(rt, &cfg) && crate::plugins::no_providers(rt, &cfg)
}

/// Why a keyless local-only proxy refuses this request, if it does: the peer is not a
/// loopback address or relays one through forwarding headers, or a browser sent it from
/// another site.
pub(crate) fn refuse_nonlocal(req: &Request) -> Option<&'static str> {
    let headers = req.headers();
    // ponytail: the listener records every peer; only in-process callers (tests) have
    // none and count as local. Fail closed here once every test router sets a peer.
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip());
    if !peer.is_none_or(loopback) || FORWARDING.iter().any(|h| headers.contains_key(*h)) {
        return Some("no client keys are configured, so only clients on this machine may use the proxy");
    }
    let foreign = headers
        .get(header::ORIGIN)
        .is_some_and(|o| !loopback_origin(o.as_bytes()));
    (cross_site(headers) || foreign)
        .then_some("no client keys are configured, so requests from other websites are refused")
}

/// 127.0.0.0/8, `::1` and IPv4-mapped loopback (a dual-stack listener's IPv4 peers).
fn loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
    }
}

/// A browser marks a request from another site: `Sec-Fetch-Site` `cross-site`, or
/// `same-site` (another port or subdomain of the same site).
pub(crate) fn cross_site(headers: &HeaderMap) -> bool {
    headers.get("sec-fetch-site").is_some_and(|v| {
        let v = v.as_bytes();
        v.eq_ignore_ascii_case(b"cross-site") || v.eq_ignore_ascii_case(b"same-site")
    })
}

/// An `Origin` of http(s) on `localhost`, a `.localhost` name or a loopback address.
pub(crate) fn loopback_origin(origin: &[u8]) -> bool {
    origin_authority(origin).is_some_and(|(_, host, _)| {
        let host = host.to_ascii_lowercase();
        host == "localhost"
            || host.ends_with(".localhost")
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<IpAddr>()
                .is_ok_and(loopback)
    })
}

/// Whether an `Origin` names the request's own host: its `Host` (or HTTP/2 authority),
/// or the `X-Forwarded-Host` a reverse proxy set, with the scheme's default port filled
/// in. A browser cannot set `X-Forwarded-Host` on a cross-site request without a
/// preflight, which management routes never grant.
pub(crate) fn same_origin(origin: &[u8], headers: &HeaderMap, uri: &Uri) -> bool {
    let Some((https, host, port)) = origin_authority(origin) else {
        return false;
    };
    let forwarded = headers
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim);
    let own = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| uri.authority().map(|a| a.as_str()));
    let default = if https { 443 } else { 80 };
    [own, forwarded].into_iter().flatten().any(|authority| {
        split_authority(authority)
            .is_some_and(|(h, p)| h.eq_ignore_ascii_case(host) && p.unwrap_or(default) == port.unwrap_or(default))
    })
}

/// `scheme://host[:port]` with an http or https scheme: (https, host, port).
fn origin_authority(origin: &[u8]) -> Option<(bool, &str, Option<u16>)> {
    let origin = std::str::from_utf8(origin).ok()?;
    let (https, rest) = match origin.split_once("://")? {
        (scheme, rest) if scheme.eq_ignore_ascii_case("http") => (false, rest),
        (scheme, rest) if scheme.eq_ignore_ascii_case("https") => (true, rest),
        _ => return None,
    };
    let (host, port) = split_authority(rest)?;
    Some((https, host, port))
}

/// `host[:port]`, with an IPv6 host in brackets (kept). Paths and user info are refused.
fn split_authority(authority: &str) -> Option<(&str, Option<u16>)> {
    if authority.is_empty() || authority.contains(['/', '@', '?', '#']) {
        return None;
    }
    let (host, port) = match authority.rfind(':') {
        Some(i) if !authority[i..].contains(']') => (&authority[..i], Some(authority[i + 1..].parse().ok()?)),
        _ => (authority, None),
    };
    (!host.is_empty()).then_some((host, port))
}

/// Go `AuthMiddleware`: client keys, then plugin frontend auth providers.
pub async fn require_client_key(State(rt): State<Arc<Runtime>>, req: Request, next: Next) -> Response {
    let (mut req, access) = crate::plugins::authenticate(&rt, req).await;
    let caller = match access {
        Ok(crate::plugins::Access::Open) => Caller {
            principal: String::new(),
            source: "",
            provider: String::new(),
        },
        Ok(crate::plugins::Access::Granted(caller)) => caller,
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
            provider: String::new(),
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
                provider: String::new(),
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

/// The error `url.ParseQuery(query)` returns, if any: a `;` in any pair outranks the
/// first malformed escape, and more than 10,000 pairs fail before parsing.
pub(crate) fn query_error(query: &[u8]) -> Option<String> {
    if query.iter().filter(|&&b| b == b'&').count() >= 10_000 {
        return Some("number of URL query parameters exceeded limit".into());
    }
    let mut err = None;
    for pair in query.split(|&b| b == b'&') {
        if pair.contains(&b';') {
            return Some("invalid semicolon separator in query".into());
        }
        if err.is_none() {
            let (key, value) = match pair.iter().position(|&b| b == b'=') {
                Some(i) => (&pair[..i], &pair[i + 1..]),
                None => (pair, &[][..]),
            };
            err = escape_error(key).or_else(|| escape_error(value));
        }
    }
    err
}

/// Go's `url.EscapeError` for the first `%` without two hex digits: up to three bytes
/// from it, quoted.
fn escape_error(s: &[u8]) -> Option<String> {
    let hex = |i: usize| s.get(i + 1..i + 3).is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit));
    let i = (0..s.len()).find(|&i| s[i] == b'%' && !hex(i))?;
    let end = s.len().min(i + 3);
    Some(format!("invalid URL escape {}", cpa_common::gostr::quote(&s[i..end])))
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
