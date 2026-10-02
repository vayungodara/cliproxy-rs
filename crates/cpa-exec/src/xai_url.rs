//! The part of Go's `net/url.Parse` that xAI endpoint validation depends on: the same
//! accept/reject decisions, the same `Hostname()` and the same error text. Rust's `url`
//! crate follows WHATWG and disagrees with Go on inputs such as `https://%61uth.x.ai`,
//! `https://auth.x.ai\@evil.com`, embedded tabs, `https:auth.x.ai` and port 99999.

use cpa_common::gostr::quote;

/// What validation reads from a parsed URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GoUrl {
    /// Lowercased scheme, empty for a relative reference.
    pub scheme: String,
    /// `URL.Hostname()`: host without port or IPv6 brackets.
    pub hostname: String,
}

/// `url.Parse`. The error is Go's `*url.Error` text.
pub(crate) fn parse(raw: &str) -> Result<GoUrl, String> {
    let (u, frag) = raw.split_once('#').unwrap_or((raw, ""));
    let url = parse_inner(u).map_err(|e| format!("parse {}: {e}", quote_bytes(u.as_bytes())))?;
    if !frag.is_empty() {
        unescape(frag.as_bytes(), Mode::Other).map_err(|e| format!("parse {}: {e}", quote_bytes(raw.as_bytes())))?;
    }
    Ok(url)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Host,
    Zone,
    /// Path, fragment and userinfo: only `%` escapes are checked.
    Other,
}

fn parse_inner(raw: &str) -> Result<GoUrl, String> {
    if raw.bytes().any(|b| b < b' ' || b == 0x7f) {
        return Err("net/url: invalid control character in URL".into());
    }
    if raw == "*" {
        return Ok(GoUrl {
            scheme: String::new(),
            hostname: String::new(),
        });
    }
    let (scheme, rest) = scheme(raw)?;
    let scheme = scheme.to_ascii_lowercase();
    let rest = if rest.ends_with('?') && rest.matches('?').count() == 1 {
        &rest[..rest.len() - 1]
    } else {
        rest.split_once('?').map_or(rest, |(r, _)| r)
    };
    if !rest.starts_with('/') {
        if !scheme.is_empty() {
            // A rootless path is opaque: no host.
            return Ok(GoUrl {
                scheme,
                hostname: String::new(),
            });
        }
        if rest.split('/').next().unwrap_or_default().contains(':') {
            return Err("first path segment in URL cannot contain colon".into());
        }
    }
    let mut host = String::new();
    let mut path = rest;
    if (!scheme.is_empty() || !rest.starts_with("///")) && rest.starts_with("//") {
        let authority = &rest[2..];
        let (authority, tail) = match authority.find('/') {
            Some(i) => (&authority[..i], &authority[i..]),
            None => (authority, ""),
        };
        host = parse_authority(authority)?;
        path = tail;
    }
    unescape(path.as_bytes(), Mode::Other)?;
    Ok(GoUrl {
        scheme,
        hostname: hostname(&host),
    })
}

/// `getScheme`.
fn scheme(raw: &str) -> Result<(&str, &str), String> {
    for (i, c) in raw.bytes().enumerate() {
        match c {
            b'a'..=b'z' | b'A'..=b'Z' => {}
            b'0'..=b'9' | b'+' | b'-' | b'.' if i == 0 => return Ok(("", raw)),
            b'0'..=b'9' | b'+' | b'-' | b'.' => {}
            b':' if i == 0 => return Err("missing protocol scheme".into()),
            b':' => return Ok((&raw[..i], &raw[i + 1..])),
            _ => return Ok(("", raw)),
        }
    }
    Ok(("", raw))
}

/// `parseAuthority`: the host; userinfo is only validated.
fn parse_authority(authority: &str) -> Result<String, String> {
    let at = authority.rfind('@');
    let host = parse_host(at.map_or(authority, |i| &authority[i + 1..]))?;
    let Some(i) = at else { return Ok(host) };
    let userinfo = &authority[..i];
    let valid = userinfo.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '-' | '.'
                    | '_'
                    | ':'
                    | '~'
                    | '!'
                    | '$'
                    | '&'
                    | '\''
                    | '('
                    | ')'
                    | '*'
                    | '+'
                    | ','
                    | ';'
                    | '='
                    | '%'
                    | '@'
            )
    });
    if !valid {
        return Err("net/url: invalid userinfo".into());
    }
    match userinfo.split_once(':') {
        Some((user, password)) => {
            unescape(user.as_bytes(), Mode::Other)?;
            unescape(password.as_bytes(), Mode::Other)?;
        }
        None => {
            unescape(userinfo.as_bytes(), Mode::Other)?;
        }
    }
    Ok(host)
}

/// `validOptionalPort`: empty or `:` followed by digits.
fn valid_port(port: &str) -> bool {
    port.is_empty()
        || port
            .strip_prefix(':')
            .is_some_and(|p| p.bytes().all(|b| b.is_ascii_digit()))
}

/// `parseHost` with the default `urlstrictcolons=1` (Go 1.26).
fn parse_host(host: &str) -> Result<String, String> {
    if let Some(open) = host.rfind('[') {
        let Some(close) = host.rfind(']') else {
            return Err("missing ']' in host".into());
        };
        let colon_port = &host[close + 1..];
        if !valid_port(colon_port) {
            return Err(format!("invalid port {} after host", quote(colon_port)));
        }
        let port = unescape(colon_port.as_bytes(), Mode::Host)?;
        // A `]` before the last `[` makes Go slice backwards and panic; nothing valid
        // can follow, so it is reported as an invalid host here.
        let Some(inner) = host.get(open + 1..close) else {
            return Err("invalid host".into());
        };
        let unescaped = match inner.find("%25") {
            Some(zone) => {
                let mut out = unescape(&inner.as_bytes()[..zone], Mode::Host)?;
                out.extend(unescape(&inner.as_bytes()[zone..], Mode::Zone)?);
                out
            }
            None => unescape(inner.as_bytes(), Mode::Host)?,
        };
        let text = String::from_utf8_lossy(&unescaped).into_owned();
        // ponytail: netip.ParseAddr's detailed error text is not ported; IP literals can
        // never pass the x.ai host check, so only this message differs from Go.
        let addr = text.split_once('%').map_or(text.as_str(), |(a, _)| a);
        match addr.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V6(_)) => {}
            Ok(std::net::IpAddr::V4(_)) => return Err("invalid IP-literal".into()),
            _ => return Err(format!("invalid host: ParseAddr({}): unable to parse IP", quote(&text))),
        }
        return Ok(format!("[{text}]{}", String::from_utf8_lossy(&port)));
    }
    // Strict colons: the port starts at the first colon, so a second one is invalid.
    if let Some(i) = host.find(':')
        && !valid_port(&host[i..])
    {
        return Err(format!(
            "invalid port {} after host",
            quote_bytes(&host.as_bytes()[i..])
        ));
    }
    let out = unescape(host.as_bytes(), Mode::Host)?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// `URL.Hostname()` (`splitHostPort`).
fn hostname(host: &str) -> String {
    let mut h = host;
    if let Some(colon) = h.rfind(':')
        && valid_port(&h[colon..])
    {
        h = &h[..colon];
    }
    if h.starts_with('[') && h.ends_with(']') && h.len() >= 2 {
        h = &h[1..h.len() - 1];
    }
    h.to_owned()
}

fn unhex(c: u8) -> u8 {
    9 * (c >> 6) + (c & 15)
}

/// `shouldEscape(c, encodeHost)` (identical for `encodeZone` over ASCII).
fn host_escapes(c: u8) -> bool {
    !(c.is_ascii_alphanumeric()
        || matches!(
            c,
            b'!' | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'['
                | b']'
                | b'<'
                | b'>'
                | b'"'
                | b'-'
                | b'_'
                | b'.'
                | b'~'
        ))
}

/// `unescape` for the modes validation reaches (`+` is never a space here).
fn unescape(s: &[u8], mode: Mode) -> Result<Vec<u8>, String> {
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'%' => {
                if i + 2 >= s.len() || !s[i + 1].is_ascii_hexdigit() || !s[i + 2].is_ascii_hexdigit() {
                    let end = s.len().min(i + 3);
                    return Err(format!("invalid URL escape {}", quote_bytes(&s[i..end])));
                }
                let escape = &s[i..i + 3];
                if mode == Mode::Host && unhex(s[i + 1]) < 8 && escape != b"%25" {
                    return Err(format!("invalid URL escape {}", quote_bytes(escape)));
                }
                if mode == Mode::Zone {
                    let v = (unhex(s[i + 1]) << 4) | unhex(s[i + 2]);
                    if escape != b"%25" && v != b' ' && host_escapes(v) {
                        return Err(format!("invalid URL escape {}", quote_bytes(escape)));
                    }
                }
                i += 3;
            }
            c => {
                if matches!(mode, Mode::Host | Mode::Zone) && c < 0x80 && host_escapes(c) {
                    return Err(format!("invalid character {} in host name", quote_bytes(&[c])));
                }
                i += 1;
            }
        }
    }
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%' {
            out.push((unhex(s[i + 1]) << 4) | unhex(s[i + 2]));
            i += 3;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    Ok(out)
}

/// `strconv.Quote` of Go string bytes: invalid UTF-8 bytes print as `\xNN`.
pub(crate) fn quote_bytes(b: &[u8]) -> String {
    let mut out = String::from('"');
    for chunk in b.utf8_chunks() {
        let valid = quote(chunk.valid());
        out.push_str(&valid[1..valid.len() - 1]);
        for byte in chunk.invalid() {
            out.push_str(&format!("\\x{byte:02x}"));
        }
    }
    out.push('"');
    out
}
