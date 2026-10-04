//! The `net/url` reads the store makes, over cpa-exec's port of Go's `url.Parse`.

pub use cpa_exec::xai_url::GoUrl;

/// `url.Parse`; `None` where Go returns an error.
pub fn parse(raw: &str) -> Option<GoUrl> {
    cpa_exec::xai_url::parse(raw).ok()
}

/// `url.Parse` with Go's error text.
pub fn parse_err(raw: &str) -> Result<GoUrl, String> {
    cpa_exec::xai_url::parse(raw)
}

/// `URL.Query()` keys: `&`-separated pairs, a pair holding `;` skipped, keys
/// query-unescaped (`+` is a space), undecodable ones skipped.
pub fn query_keys(raw_query: &str) -> Vec<String> {
    raw_query
        .split('&')
        .filter(|pair| !pair.is_empty() && !pair.contains(';'))
        .filter_map(|pair| query_unescape(pair.split_once('=').map_or(pair, |(k, _)| k)))
        .collect()
}

/// `url.QueryUnescape`.
pub fn query_unescape(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hex = b.get(i + 1..i + 3)?;
                let text = std::str::from_utf8(hex).ok()?;
                out.push(u8::from_str_radix(text, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// `url.PathUnescape`.
pub fn path_unescape(s: &str) -> Result<String, String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let escape = b.get(i..(i + 3).min(b.len())).unwrap_or_default();
            let ok = escape.len() == 3 && escape[1].is_ascii_hexdigit() && escape[2].is_ascii_hexdigit();
            if !ok {
                return Err(format!("invalid URL escape {}", cpa_common::gostr::quote(escape)));
            }
            let text = std::str::from_utf8(&escape[1..]).unwrap_or("00");
            out.push(u8::from_str_radix(text, 16).unwrap_or_default());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// `url.PathEscape`.
pub fn path_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &c in s.as_bytes() {
        let keep = c.is_ascii_alphanumeric()
            || matches!(
                c,
                b'-' | b'_' | b'.' | b'~' | b'$' | b'&' | b'+' | b',' | b':' | b'=' | b'@'
            );
        if keep {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
    }
    out
}

/// Go `hasSensitiveQueryParameter`.
pub fn has_sensitive_query_parameter(u: &GoUrl) -> bool {
    !u.raw_query.is_empty()
        && query_keys(&u.raw_query).iter().any(|key| {
            matches!(
                key.trim().to_lowercase().as_str(),
                "token" | "access_token" | "access_key" | "secret" | "secret_key" | "api_key"
            )
        })
}

/// `URL.Path`: the written path, unescaped.
pub fn path(u: &GoUrl) -> String {
    path_unescape(&u.raw_path).unwrap_or_else(|_| u.raw_path.clone())
}

/// Go `URL.String()` of a non-opaque URL, from its parts as written.
// ponytail: the parts are re-joined as written; Go re-escapes the path, host and
// user info, which only differs for unusual escapes.
pub fn string(u: &GoUrl) -> String {
    let mut out = String::new();
    if !u.scheme.is_empty() {
        out.push_str(&u.scheme);
        out.push(':');
    }
    if !u.scheme.is_empty() || !u.host.is_empty() || u.has_user {
        if !u.host.is_empty() || !u.raw_path.is_empty() || u.has_user {
            out.push_str("//");
        }
        if u.has_user {
            out.push_str(&u.userinfo);
            out.push('@');
        }
        out.push_str(&u.host);
    }
    if !u.raw_path.is_empty() && !u.raw_path.starts_with('/') && !u.host.is_empty() {
        out.push('/');
    }
    out.push_str(&u.raw_path);
    if u.force_query || !u.raw_query.is_empty() {
        out.push('?');
        out.push_str(&u.raw_query);
    }
    if !u.fragment.is_empty() {
        out.push('#');
        out.push_str(&u.fragment);
    }
    out
}

/// Go `URL.Parse(ref)`: `url.Parse` of the reference, then `ResolveReference` against
/// `base` (a non-opaque URL).
pub fn resolve(base: &GoUrl, reference: &str) -> Result<GoUrl, String> {
    let r = parse_err(reference)?;
    let mut url = r.clone();
    if r.scheme.is_empty() {
        url.scheme = base.scheme.clone();
    }
    // The "absoluteURI" or "net_path" cases.
    if !r.scheme.is_empty() || !r.host.is_empty() || r.has_user {
        url.raw_path = resolve_path(&r.raw_path, "");
        return Ok(url);
    }
    if r.raw_path.is_empty() && !r.force_query && r.raw_query.is_empty() {
        url.raw_query = base.raw_query.clone();
        if r.fragment.is_empty() {
            url.fragment = base.fragment.clone();
        }
    }
    // The "abs_path" or "rel_path" cases.
    url.host = base.host.clone();
    url.hostname = base.hostname.clone();
    url.port = base.port.clone();
    url.has_user = base.has_user;
    url.userinfo = base.userinfo.clone();
    url.raw_path = resolve_path(&base.raw_path, &r.raw_path);
    Ok(url)
}

/// Go `resolvePath`: the merged path with dot segments removed, always rooted.
fn resolve_path(base: &str, reference: &str) -> String {
    let full = if reference.is_empty() {
        base.to_owned()
    } else if !reference.starts_with('/') {
        let dir = base.rfind('/').map_or("", |i| &base[..=i]);
        format!("{dir}{reference}")
    } else {
        reference.to_owned()
    };
    if full.is_empty() {
        return String::new();
    }
    let mut dst = String::from("/");
    let mut first = true;
    let mut elem = "";
    for segment in full.split('/') {
        elem = segment;
        match segment {
            "." => first = false,
            ".." => {
                match dst[1..].rfind('/') {
                    Some(i) => dst.truncate(i + 1),
                    None => dst.truncate(1),
                }
                first = dst.len() == 1;
            }
            _ => {
                if !first {
                    dst.push('/');
                }
                dst.push_str(segment);
                first = false;
            }
        }
    }
    if elem == "." || elem == ".." {
        dst.push('/');
    }
    match dst.strip_prefix("//") {
        Some(rest) => format!("/{rest}"),
        None => dst,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_and_path_follow_go() {
        assert_eq!(query_keys("a=1&b%20c=2&x;y=3&&bad%zz=4&token"), ["a", "b c", "token"]);
        assert_eq!(path_escape("a b/c"), "a%20b%2Fc");
        assert_eq!(path_unescape("a%2Fb"), Ok("a/b".into()));
        assert!(path_unescape("%zz").is_err());
        let base = parse("https://h.example/a/b/c?q=1").unwrap();
        assert_eq!(string(&resolve(&base, "../d?x").unwrap()), "https://h.example/a/d?x");
        assert_eq!(string(&resolve(&base, "/e").unwrap()), "https://h.example/e");
        assert_eq!(
            string(&resolve(&base, "https://o.example/f").unwrap()),
            "https://o.example/f"
        );
        assert_eq!(string(&resolve(&base, "//o.example/g").unwrap()), "https://o.example/g");
        // Go normalises dot segments of absolute references too, keeps user info until
        // validation, and roots a relative path against an empty base path.
        assert_eq!(
            string(&resolve(&base, "https://h.example/private/../public/x").unwrap()),
            "https://h.example/public/x"
        );
        assert_eq!(
            string(&resolve(&base, "https://u:p@h.example/x").unwrap()),
            "https://u:p@h.example/x"
        );
        let bare = parse("https://h.example").unwrap();
        assert_eq!(string(&resolve(&bare, "r.json").unwrap()), "https://h.example/r.json");
        assert_eq!(
            string(&resolve(&base, "a/./b/../").unwrap()),
            "https://h.example/a/b/a/"
        );
        assert_eq!(string(&resolve(&base, "..").unwrap()), "https://h.example/a/");
        assert_eq!(string(&resolve(&base, "?").unwrap()), "https://h.example/a/b/c?");
        assert_eq!(string(&resolve(&base, "#f").unwrap()), "https://h.example/a/b/c?q=1#f");
        assert_eq!(
            string(&resolve(&bare, "https://o.example").unwrap()),
            "https://o.example"
        );
    }
}
