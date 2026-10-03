//! Custom upstream headers from credential attributes (Go
//! internal/util/header_helpers.go `ApplyCustomHeadersFromAttrs`).
//!
//! Every `header:<Name>` attribute names an upstream header. Its value is used as is,
//! except:
//! - `$CPA-SESSION-ID` (exactly, or embedded anywhere, case-insensitively) becomes the
//!   request's session ID; without one the header is omitted.
//! - `$<Client-Header>` copies the first value of that client header; when the client
//!   did not send it the header is omitted.
//!
//! File credentials carry their `headers` map as `header:` attributes already
//! (`cpa_core::config::credentials`), as Go's `ApplyCustomHeadersFromMetadata` does.

use std::collections::BTreeMap;

use http::HeaderMap;

const SESSION_VAR: &str = "$CPA-SESSION-ID";

/// The resolved custom headers, in attribute order. Callers set each one, replacing any
/// default of the same name; a `Host` header also sets the request's authority (Go
/// mirrors it into `req.Host`).
///
/// `session_id` is Go's `$CPA-SESSION-ID`:
/// `crate::session::cpa_session_id(req.session.as_deref())`.
pub fn custom_headers(
    attributes: &BTreeMap<String, String>,
    client: &HeaderMap,
    session_id: Option<&str>,
) -> Vec<(String, String)> {
    let session = session_id.map(str::trim).filter(|s| !s.is_empty());
    let mut out: Vec<(String, String)> = Vec::new();
    for (key, value) in attributes {
        let Some(name) = key.strip_prefix("header:").map(str::trim).filter(|n| !n.is_empty()) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let resolved = if value
            .strip_prefix('$')
            .is_some_and(|var| var.trim().eq_ignore_ascii_case("CPA-SESSION-ID"))
        {
            match session {
                Some(id) => id.to_owned(),
                None => continue,
            }
        } else if value.to_ascii_uppercase().contains(SESSION_VAR) {
            match session {
                Some(id) => replace_session(value, id),
                None => continue,
            }
        } else if let Some(var) = value.strip_prefix('$') {
            let var = var.trim();
            if var.is_empty() {
                continue;
            }
            // Go `Header.Get`, then a case-insensitive scan: the first value.
            match client
                .get(var)
                .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
            {
                Some(found) if !found.is_empty() => found,
                _ => continue,
            }
        } else {
            value.to_owned()
        };
        // Go collects into a map keyed by the trimmed name: the last attribute wins.
        out.retain(|(n, _)| n != name);
        out.push((name.to_owned(), resolved));
    }
    out
}

/// Go `replaceCPASessionID`: every case-insensitive `$CPA-SESSION-ID` becomes `session`.
fn replace_session(value: &str, session: &str) -> String {
    let bytes = value.as_bytes();
    let n = SESSION_VAR.len();
    let mut out = String::with_capacity(value.len());
    let (mut start, mut i) = (0, 0);
    while i + n <= bytes.len() {
        if bytes[i] == b'$' && bytes[i..i + n].eq_ignore_ascii_case(SESSION_VAR.as_bytes()) {
            out.push_str(&value[start..i]);
            out.push_str(session);
            i += n;
            start = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&value[start..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect()
    }

    /// Cases from Go header_helpers_test.go and the rules in ApplyCustomHeadersFromAttrs.
    #[test]
    fn resolves_literals_references_and_session() {
        let mut client = HeaderMap::new();
        client.insert("x-claude-code-session-id", "client-session".parse().unwrap());
        client.append("x-multi", "first".parse().unwrap());
        client.append("x-multi", "second".parse().unwrap());
        let a = attrs(&[
            ("header:X-Literal", "  plain  "),
            ("header:X-Copy", "$X-Claude-Code-Session-Id"),
            ("header:X-Multi-Copy", "$x-multi"),
            ("header:X-Missing", "$X-Not-Sent"),
            ("header:X-Empty-Var", "$ "),
            ("header:X-Session", "$cpa-session-id"),
            ("header:X-Embedded", "pre-$CPA-Session-ID-mid-$CPA-SESSION-ID"),
            ("header: ", "ignored"),
            ("header:X-Blank", "   "),
            ("api_key", "fake-not-a-header"),
        ]);
        let got = custom_headers(&a, &client, Some(" s-1 "));
        let got: Vec<(&str, &str)> = got.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            got,
            [
                ("X-Copy", "client-session"),
                ("X-Embedded", "pre-s-1-mid-s-1"),
                ("X-Literal", "plain"),
                ("X-Multi-Copy", "first"),
                ("X-Session", "s-1"),
            ]
        );
        let without = custom_headers(&a, &HeaderMap::new(), None);
        let names: Vec<&str> = without.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["X-Literal"], "session and client references need their source");
    }

    #[test]
    fn session_replacement_handles_short_and_adjacent_values() {
        assert_eq!(replace_session("$cpa", "x"), "$cpa");
        assert_eq!(replace_session("$CPA-SESSION-ID$cpa-session-id", "x"), "xx");
        assert_eq!(replace_session("a$CPA-SESSION-IDb", "é"), "aéb");
    }
}
