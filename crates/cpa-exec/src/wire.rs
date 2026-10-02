//! Ordered/cased HTTP/1 headers from the pinned native Claude Code captures.

pub(crate) const MESSAGES: &[&str] = &[
    "Accept",
    "Authorization",
    "Content-Type",
    "User-Agent",
    "X-Claude-Code-Session-Id",
    "X-Stainless-Arch",
    "X-Stainless-Lang",
    "X-Stainless-OS",
    "X-Stainless-Package-Version",
    "X-Stainless-Retry-Count",
    "X-Stainless-Runtime",
    "X-Stainless-Runtime-Version",
    "X-Stainless-Timeout",
    "anthropic-beta",
    "anthropic-dangerous-direct-browser-access",
    "anthropic-version",
    "x-app",
    "x-client-request-id",
    "Connection",
    "Host",
    "Accept-Encoding",
    "Content-Length",
];

pub(crate) const OAUTH_TOKEN: &[&str] = &[
    "Accept",
    "Content-Type",
    "User-Agent",
    "Content-Length",
    "Accept-Encoding",
    "Host",
    "Connection",
];
pub(crate) const OAUTH_INSPECT: &[&str] = &[
    "Accept",
    "Content-Type",
    "Authorization",
    "Cache-Control",
    "User-Agent",
    "Accept-Encoding",
    "Host",
    "Connection",
];

pub(crate) fn order(names: &[&'static str]) -> wreq::header::OrigHeaderMap {
    let mut map = wreq::header::OrigHeaderMap::new();
    for name in names {
        map.insert(*name);
    }
    map
}
