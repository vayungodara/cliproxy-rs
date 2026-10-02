//! Ordered/cased HTTP/1 headers for Claude OAuth acquisition (auth/claude/utls_transport.go).
//! The Messages/count_tokens order lives in claude/headers.rs.

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
