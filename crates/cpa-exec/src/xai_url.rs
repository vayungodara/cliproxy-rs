//! The part of Go's `net/url.Parse` that xAI endpoint validation depends on: the same
//! accept/reject decisions, the same `Hostname()` and the same error text. Rust's `url`
//! crate follows WHATWG and disagrees with Go on inputs such as `https://%61uth.x.ai`,
//! `https://auth.x.ai\@evil.com`, embedded tabs, `https:auth.x.ai` and port 99999.

use cpa_common::gostr::quote;

/// What validation reads from a parsed URL.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoUrl {
    /// Lowercased scheme, empty for a relative reference.
    pub scheme: String,
    /// `URL.Hostname()`: host without port or IPv6 brackets.
    pub hostname: String,
    /// `URL.Port()`: the numeric port, empty when absent.
    pub port: String,
    /// `URL.Host`: host and port as parsed (unescaped, IPv6 in brackets).
    pub host: String,
    /// `escape(URL.Host, encodeHost)`, as `URL.String()` writes it; exact where `host`
    /// is not UTF-8.
    pub escaped_host: String,
    /// `URL.OmitHost`: a scheme followed by a single `/` (`https:/x`).
    pub omit_host: bool,
    /// `URL.Opaque`: the text after `scheme:` when it does not start with `/`.
    pub opaque: String,
    /// `URL.User != nil`: the authority carried user info.
    pub has_user: bool,
    /// The user info as written (before `@`), when present.
    pub userinfo: String,
    /// `URL.RawQuery`.
    pub raw_query: String,
    /// `URL.ForceQuery`: a `?` with nothing after it.
    pub force_query: bool,
    /// The fragment as written (`URL.EscapedFragment` for valid input).
    pub fragment: String,
    /// The path as written (`URL.EscapedPath` for valid input); empty for an opaque
    /// URL.
    pub raw_path: String,
}

/// `url.Parse`. The error is Go's `*url.Error` text.
pub fn parse(raw: &str) -> Result<GoUrl, String> {
    let (u, frag) = raw.split_once('#').unwrap_or((raw, ""));
    let mut url = parse_inner(u).map_err(|e| format!("parse {}: {e}", quote_bytes(u.as_bytes())))?;
    if !frag.is_empty() {
        unescape(frag.as_bytes(), Mode::Other).map_err(|e| format!("parse {}: {e}", quote_bytes(raw.as_bytes())))?;
    }
    url.fragment = frag.to_owned();
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
            raw_path: "*".into(),
            ..GoUrl::default()
        });
    }
    let (scheme, rest) = scheme(raw)?;
    let scheme = scheme.to_ascii_lowercase();
    let force_query = rest.ends_with('?') && rest.matches('?').count() == 1;
    let (rest, raw_query) = if force_query {
        (&rest[..rest.len() - 1], "")
    } else {
        rest.split_once('?').unwrap_or((rest, ""))
    };
    let raw_query = raw_query.to_owned();
    if !rest.starts_with('/') {
        if !scheme.is_empty() {
            // A rootless path is opaque: no host.
            return Ok(GoUrl {
                scheme,
                raw_query,
                force_query,
                opaque: rest.to_owned(),
                ..GoUrl::default()
            });
        }
        if rest.split('/').next().unwrap_or_default().contains(':') {
            return Err("first path segment in URL cannot contain colon".into());
        }
    }
    let mut host = Vec::new();
    let mut userinfo = None;
    let mut path = rest;
    let mut omit_host = false;
    if (!scheme.is_empty() || !rest.starts_with("///")) && rest.starts_with("//") {
        let authority = &rest[2..];
        let (authority, tail) = match authority.find('/') {
            Some(i) => (&authority[..i], &authority[i..]),
            None => (authority, ""),
        };
        (host, userinfo) = parse_authority(&scheme, authority)?;
        path = tail;
    } else if !scheme.is_empty() && rest.starts_with('/') {
        omit_host = true;
    }
    unescape(path.as_bytes(), Mode::Other)?;
    let escaped_host = escape(&host, Encode::Host);
    let host = String::from_utf8_lossy(&host).into_owned();
    let (hostname, port) = split_host_port(&host);
    Ok(GoUrl {
        scheme,
        hostname,
        port,
        host,
        escaped_host,
        omit_host,
        opaque: String::new(),
        has_user: userinfo.is_some(),
        userinfo: userinfo.unwrap_or_default(),
        raw_query,
        force_query,
        fragment: String::new(),
        raw_path: path.to_owned(),
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

/// `parseAuthority`: the host, and the user info as written when present (it is only
/// validated).
fn parse_authority(scheme: &str, authority: &str) -> Result<(Vec<u8>, Option<String>), String> {
    let at = authority.rfind('@');
    let host = parse_host(scheme, at.map_or(authority, |i| &authority[i + 1..]))?;
    let Some(i) = at else { return Ok((host, None)) };
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
    Ok((host, Some(userinfo.to_owned())))
}

/// `validOptionalPort`: empty or `:` followed by digits.
fn valid_port(port: &str) -> bool {
    port.is_empty()
        || port
            .strip_prefix(':')
            .is_some_and(|p| p.bytes().all(|b| b.is_ascii_digit()))
}

/// `parseHost` with the default `urlstrictcolons=1` (Go 1.26.4): the unescaped bytes.
fn parse_host(scheme: &str, host: &str) -> Result<Vec<u8>, String> {
    if let Some(open) = host.rfind('[') {
        // An IP literal must start the host.
        if open != 0 {
            return Err("invalid IP-literal".into());
        }
        let Some(close) = host.rfind(']') else {
            return Err("missing ']' in host".into());
        };
        let colon_port = &host[close + 1..];
        if !valid_port(colon_port) {
            return Err(format!("invalid port {} after host", quote(colon_port)));
        }
        let port = unescape(colon_port.as_bytes(), Mode::Host)?;
        // The host starts with its only `[`, so the last `]` follows it.
        let inner = &host[1..close];
        let unescaped = match inner.find("%25") {
            Some(zone) => {
                let mut out = unescape(&inner.as_bytes()[..zone], Mode::Host)?;
                out.extend(unescape(&inner.as_bytes()[zone..], Mode::Zone)?);
                out
            }
            None => unescape(inner.as_bytes(), Mode::Host)?,
        };
        let text = String::from_utf8_lossy(&unescaped).into_owned();
        // ponytail: netip.ParseAddr's detailed error text is not ported.
        let addr = text.split_once('%').map_or(text.as_str(), |(a, _)| a);
        match addr.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V6(_)) => {}
            Ok(std::net::IpAddr::V4(_)) => return Err("invalid IP-literal".into()),
            _ => return Err(format!("invalid host: ParseAddr({}): unable to parse IP", quote(&text))),
        }
        return Ok([b"[".as_slice(), &unescaped, b"]", &port].concat());
    }
    // Strict colons: the port starts at the first colon, so a second one is invalid,
    // except for PostgreSQL's comma-separated host lists.
    if let Some(first) = host.find(':') {
        let i = match host.rfind(':') {
            Some(last) if last != first && matches!(scheme, "postgresql" | "postgres") => last,
            _ => first,
        };
        if !valid_port(&host[i..]) {
            return Err(format!(
                "invalid port {} after host",
                quote_bytes(&host.as_bytes()[i..])
            ));
        }
    }
    unescape(host.as_bytes(), Mode::Host)
}

/// `splitHostPort`: `URL.Hostname()` and `URL.Port()`.
fn split_host_port(host: &str) -> (String, String) {
    let (mut h, mut port) = (host, "");
    if let Some(colon) = h.rfind(':')
        && valid_port(&h[colon..])
    {
        port = &h[colon + 1..];
        h = &h[..colon];
    }
    if h.starts_with('[') && h.ends_with(']') && h.len() >= 2 {
        h = &h[1..h.len() - 1];
    }
    (h.to_owned(), port.to_owned())
}

fn unhex(c: u8) -> u8 {
    9 * (c >> 6) + (c & 15)
}

/// The `net/url` encodings `URL.String()` and validation use (`encodeZone` escapes as
/// `encodeHost`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Encode {
    Path,
    UserPassword,
    Host,
    Fragment,
}

/// `shouldEscape`.
fn should_escape(c: u8, mode: Encode) -> bool {
    if c.is_ascii_alphanumeric() {
        return false;
    }
    if mode == Encode::Host
        && matches!(
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
        )
    {
        return false;
    }
    match c {
        b'-' | b'_' | b'.' | b'~' => false,
        b'$' | b'&' | b'+' | b',' | b'/' | b':' | b';' | b'=' | b'?' | b'@' => match mode {
            Encode::Path => c == b'?',
            Encode::UserPassword => matches!(c, b'@' | b'/' | b'?' | b':'),
            Encode::Fragment => false,
            Encode::Host => true,
        },
        b'!' | b'(' | b')' | b'*' => mode != Encode::Fragment,
        _ => true,
    }
}

/// `escape` for every mode but `encodeQueryComponent`.
fn escape(s: &[u8], mode: Encode) -> String {
    let mut out = String::with_capacity(s.len());
    for &c in s {
        if should_escape(c, mode) {
            out.push('%');
            out.push(char::from(b"0123456789ABCDEF"[usize::from(c >> 4)]));
            out.push(char::from(b"0123456789ABCDEF"[usize::from(c & 15)]));
        } else {
            out.push(char::from(c));
        }
    }
    out
}

/// `validEncoded`.
fn valid_encoded(s: &str, mode: Encode) -> bool {
    s.bytes().all(|c| {
        matches!(
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
                | b'@'
                | b'['
                | b']'
                | b'%'
        ) || !should_escape(c, mode)
    })
}

/// `EscapedPath` and `EscapedFragment` of a part kept as written: the written text
/// when it is validly encoded, else the unescaped value escaped again.
fn escaped(written: &str, mode: Encode) -> String {
    if valid_encoded(written, mode) {
        return written.to_owned();
    }
    match unescape(written.as_bytes(), Mode::Other) {
        Ok(value) => escape(&value, mode),
        Err(_) => written.to_owned(),
    }
}

/// `URL.String()`.
pub fn string(u: &GoUrl) -> String {
    let mut out = String::new();
    if !u.scheme.is_empty() {
        out.push_str(&u.scheme);
        out.push(':');
    }
    if !u.opaque.is_empty() {
        out.push_str(&u.opaque);
    } else {
        if (!u.scheme.is_empty() || !u.host.is_empty() || u.has_user)
            && !(u.omit_host && u.host.is_empty() && !u.has_user)
        {
            if !u.host.is_empty() || !u.raw_path.is_empty() || u.has_user {
                out.push_str("//");
            }
            if u.has_user {
                let (user, password) = match u.userinfo.split_once(':') {
                    Some((user, password)) => (user, Some(password)),
                    None => (u.userinfo.as_str(), None),
                };
                let part = |p: &str| {
                    escape(
                        &unescape(p.as_bytes(), Mode::Other).unwrap_or_default(),
                        Encode::UserPassword,
                    )
                };
                out.push_str(&part(user));
                if let Some(password) = password {
                    out.push(':');
                    out.push_str(&part(password));
                }
                out.push('@');
            }
            out.push_str(&u.escaped_host);
        }
        let path = escaped(&u.raw_path, Encode::Path);
        if !path.is_empty() && !path.starts_with('/') && !u.host.is_empty() {
            out.push('/');
        }
        if out.is_empty() && path.split('/').next().unwrap_or_default().contains(':') {
            out.push_str("./");
        }
        out.push_str(&path);
    }
    if u.force_query || !u.raw_query.is_empty() {
        out.push('?');
        out.push_str(&u.raw_query);
    }
    if !u.fragment.is_empty() {
        out.push('#');
        out.push_str(&escaped(&u.fragment, Encode::Fragment));
    }
    out
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
                    if escape != b"%25" && v != b' ' && should_escape(v, Encode::Host) {
                        return Err(format!("invalid URL escape {}", quote_bytes(escape)));
                    }
                }
                i += 3;
            }
            c => {
                if matches!(mode, Mode::Host | Mode::Zone) && c < 0x80 && should_escape(c, Encode::Host) {
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

/// A Go executor returns `http.NewRequest`'s and `Client.Do`'s URL errors as they are:
/// no status (rendered 500), no retry round, and the model cools for the credential.
pub(crate) fn statusless(message: impl Into<String>) -> cpa_core::exec::ExecError {
    cpa_core::exec::ExecError::local(0, cpa_core::exec::FailureScope::Model, message)
}

/// `http.NewRequest`'s parse of `raw`: Go refuses the URL before anything is sent.
pub(crate) fn new_request(raw: &str) -> Result<(), cpa_core::exec::ExecError> {
    parse(raw).map(drop).map_err(statusless)
}

/// `Client.Do` of a POST to a URL Go parsed but its transport refuses: a scheme other
/// than `http` or `https` (also none, as in `relay.example/v1`), then a missing host
/// (`https:relay.example`, `https:///v1`). The request is logged first and nothing is
/// sent; the url crate would read a host from such URLs and send there.
pub(crate) fn transport_refusal(raw: &str) -> Option<cpa_core::exec::ExecError> {
    let url = parse(raw).ok()?;
    let reason = if !matches!(url.scheme.as_str(), "http" | "https") {
        format!("unsupported protocol scheme {}", quote_bytes(url.scheme.as_bytes()))
    } else if url.host.is_empty() {
        "http: no Host in request URL".to_owned()
    } else {
        return None;
    };
    Some(statusless(format!("Post {}: {reason}", quote_bytes(raw.as_bytes()))))
}

/// Whether the HTTP client, which parses URLs with WHATWG rules (`url`), would contact
/// the same scheme, host and port that Go's `net/url` parses from `raw`, and `false` where
/// Go refuses `raw`, as it does `https://evil.example\[::1%25.x.ai]/` (an invalid
/// IP-literal) where WHATWG reads a host; a request must never go to a host Go would not
/// contact.
pub fn same_authority(raw: &str) -> bool {
    let (Ok(go), Ok(wh)) = (parse(raw), url::Url::parse(raw)) else {
        return false;
    };
    if go.scheme != wh.scheme() {
        return false;
    }
    let host_matches = match wh.host() {
        Some(url::Host::Domain(domain)) => {
            matches!(url::Host::parse(&go.hostname), Ok(url::Host::Domain(d)) if d == domain)
        }
        Some(url::Host::Ipv4(ip)) => go.hostname.parse::<std::net::Ipv4Addr>() == Ok(ip),
        Some(url::Host::Ipv6(ip)) => go.hostname.parse::<std::net::Ipv6Addr>() == Ok(ip),
        None => false,
    };
    let go_port = if go.port.is_empty() {
        url::Url::parse(&format!("{}://h/", go.scheme))
            .ok()
            .and_then(|u| u.port_or_known_default())
    } else {
        go.port.parse::<u16>().ok()
    };
    host_matches && go_port.is_some() && go_port == wh.port_or_known_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `url.Parse` then `URL.String()`, recorded from Go 1.26.4 (tests/reference/go_url).
    #[test]
    fn string_follows_go() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/go_url_string.json")).unwrap();
        for case in cases.as_array().unwrap() {
            let input = case["in"].as_str().unwrap();
            let want = &case["string"];
            match parse(input) {
                Ok(u) => assert_eq!(Some(string(&u).as_str()), want["out"].as_str(), "{input}"),
                Err(e) => assert_eq!(Some(e.as_str()), want["err"].as_str(), "{input}"),
            }
        }
    }

    #[test]
    fn transport_must_agree_with_go_on_the_authority() {
        for ok in [
            "https://auth.x.ai/oauth2/token",
            "https://AUTH.X.AI:443/t",
            "https://auth.x.ai:8443/t?q=1#f",
            "https://u:p@auth.x.ai/t",
            "https://例.x.ai/t",
            "http://127.0.0.1:9/t",
            "https://[::1]:8443/t",
        ] {
            assert!(same_authority(ok), "{ok}");
        }
        for bad in [
            r"https://attacker.example\[::1%25.x.ai]/oauth2/token",
            r"https://auth.x.ai\@evil.example/t",
            "https://auth.x.ai:99999/t",
            "https://%61uth.x.ai/t",
            "https:auth.x.ai/t",
        ] {
            assert!(!same_authority(bad), "{bad}");
        }
    }

    /// Go 1.26.4 `http.DefaultClient.Do` of a POST to each URL.
    #[test]
    fn transport_refusals_match_go() {
        for (raw, want) in [
            (
                "ftp://relay.example/x",
                Some(r#"Post "ftp://relay.example/x": unsupported protocol scheme "ftp""#),
            ),
            (
                "//relay.example/x",
                Some(r#"Post "//relay.example/x": unsupported protocol scheme """#),
            ),
            (
                "https:relay.example/x",
                Some(r#"Post "https:relay.example/x": http: no Host in request URL"#),
            ),
            ("HTTPS://relay.example/x", None),
            ("http://127.0.0.1:9/x", None),
        ] {
            let got = transport_refusal(raw).map(|e| String::from_utf8_lossy(&e.body).into_owned());
            assert_eq!(got.as_deref(), want, "{raw}");
        }
    }

    /// Go 1.26.4 `url.Parse`: an IP literal must start the host.
    #[test]
    fn a_bracket_inside_the_host_is_go_s_invalid_ip_literal() {
        for (raw, want) in [
            ("http://x[::1]", r#"parse "http://x[::1]": invalid IP-literal"#),
            (
                "http://x[::1]:80/p",
                r#"parse "http://x[::1]:80/p": invalid IP-literal"#,
            ),
            (
                "http://[::1]x/",
                r#"parse "http://[::1]x/": invalid port "x" after host"#,
            ),
        ] {
            assert_eq!(parse(raw).err().as_deref(), Some(want), "{raw}");
        }
        assert_eq!(parse("http://[::1]:80/p").map(|u| u.hostname).as_deref(), Ok("::1"));
    }
}
