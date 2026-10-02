//! Go `SanitizeUpstreamErrorSummary` (sdk/cliproxy/auth/selector.go): strips credentials,
//! tokens and filesystem paths from upstream error text before it reaches a client, and
//! bounds the result to 256 runes.

use std::sync::LazyLock;

use regex::Regex;

/// Compiles a Go RE2 pattern. Go's `\s` is `[\t\n\f\r ]` and its `\b` is an ASCII word
/// boundary; Rust's are Unicode-aware, so both are rewritten.
fn go(pattern: &str) -> Regex {
    let mut out = String::with_capacity(pattern.len() + 16);
    let mut in_class = false;
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('s') if in_class => out.push_str(r"\t\n\f\r "),
                Some('s') => out.push_str(r"[\t\n\f\r ]"),
                Some('b') if !in_class => out.push_str(r"(?-u:\b)"),
                Some(next) => {
                    out.push('\\');
                    out.push(next);
                }
                None => out.push('\\'),
            },
            '[' if !in_class => {
                in_class = true;
                out.push(c);
            }
            ']' if in_class => {
                in_class = false;
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    Regex::new(&out).expect("valid Go sanitizer pattern")
}

macro_rules! pattern {
    ($name:ident, $re:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| go($re));
    };
}

pattern!(
    SCHEME_AUTH,
    r"(?i)((?:[A-Za-z0-9.+_\-]+:)?//)(?:[^:\s/@]+:[^@\s]+|[^@\s/]+)@"
);
pattern!(
    QUERY_PARAM,
    r"(?i)([?&][A-Za-z0-9_.-]*(?:key|token|secret|password|auth|sig|signature)=)[^&\s,\r\n;]+"
);
pattern!(COOKIE, r"(?i)\b(?:set-)?cookie\s*:[^\r\n]+");
pattern!(AUTH_HEADER, r"(?i)\bauthorization\s*[:=]\s*[^\r\n]+");
pattern!(
    NATURAL_SECRET,
    r#"(?i)\b([A-Za-z0-9_.-]*(?:api[ _-]?key|access[ _-]?token|client[ _-]?secret|private[ _-]?key|secret[ _-]?key|password|secret|token|credentials?|sessionid))\s*(?:(?:is|was|provided|used)?\s*[:= ]\s*|\s+is\s+|\s+was\s+|\s+provided\s+|\s+)(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|(?:[^\r\n;,|]+?(?:\s+(?:and|with|for|via)\s+|[,;|]|\r|\n|$)|[^\r\n;,|]+))"#
);
pattern!(
    KV,
    r#"(?i)((?:'|")?(?:[A-Za-z0-9_.-]*(?:key|token|secret|password|credential|credentials|bearer|sessionid|auth|signature|sig))(?:'|")?\s*[=:]\s*)(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|(?:[^\r\n;,|]+?(?:\s+(?:and|with|for|via)\s+|[,;|]|\r|\n|$)|[^\r\n;,|]+))"#
);
pattern!(
    INVALID_TOKEN,
    r#"(?i)\b(invalid|bad|expired|unknown)\s+(?:api\s+key|access\s+token|refresh\s+token|token|key|secret|password|credentials?|bearer)\s*(?:[:= ]\s*)?(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|[^\s,\r\n;]+)"#
);
pattern!(SK_KEY, r"\b(?:sk-[A-Za-z0-9._~+/=-]{6,}|ghp_[A-Za-z0-9._~+/=-]{6,})\b");
pattern!(BEARER, r"(?i)\b(?:bearer|basic)\s+[A-Za-z0-9._~+/=-]+");
pattern!(DOUBLE_QUOTED_PATH, r#""/[^"\r\n]+""#);
pattern!(SINGLE_QUOTED_PATH, r"'/[^'\r\n]+'");
pattern!(BACKTICK_QUOTED_PATH, r"`/[^`\r\n]+`");
pattern!(PATH_CONNECTOR, r"(?i)\s+(to|from|into|onto|for|via|with|and)\s+/");
pattern!(
    UNIX_PATH_STANDARD,
    r#"(^|[\s\(\[\{<"';,=])(/(?:[^/\s\r\n"',;?#()<>{}\[\]]+(?:\s+[^/\s\r\n"',;?#()<>{}\[\]]+)*/)*[^/:\s\r\n"',;?#()<>{}\[\]]+(?::[^/:\s\r\n"',;?#()<>{}\[\]]+)?)"#
);
pattern!(
    FILE_EXT_PATH,
    r#"(^|[\s"'`(\[,;=])(/[^\s:\r\n"'`,;\])>]+(?:\s+[^\s:\r\n"'`,;\])>]+)*\.(?:json|yaml|yml|key|pem|txt|log|toml|conf|env|crt|cer))"#
);
pattern!(WINDOWS_PATH, r#"(?i)\b[A-Za-z]:\\[^\r\n:,;'"<>]+"#);
pattern!(WINDOWS_UNC_PATH, r#"\\\\[^\r\n:,;'"<>]+\\[^\r\n:,;'"<>]+"#);

const KNOWN_ERRORS: [&str; 17] = [
    "permission denied",
    "no such file",
    "file not found",
    "access denied",
    "operation not permitted",
    "denied",
    "read-only",
    "is a directory",
    "not a directory",
    "cannot find",
    "no space",
    "connection refused",
    "timeout",
    "failed",
    "error",
    "not supported",
    "invalid argument",
];

/// Go `SanitizeUpstreamErrorSummary`.
pub fn summary(s: &str) -> String {
    let s = no_truncate(s);
    let runes = s.chars().count();
    if runes > 256 {
        return s.chars().take(253).collect::<String>() + "...";
    }
    s
}

/// Go `sanitizeUpstreamErrorSummaryNoTruncate`.
fn no_truncate(s: &str) -> String {
    let s = s.trim_matches(|c: char| c.is_whitespace());
    if s.is_empty() {
        return String::new();
    }
    let mut s = SCHEME_AUTH.replace_all(s, "${1}[REDACTED_AUTH]@").into_owned();
    s = QUERY_PARAM.replace_all(&s, "${1}[REDACTED]").into_owned();
    s = DOUBLE_QUOTED_PATH.replace_all(&s, r#""[REDACTED_PATH]""#).into_owned();
    s = SINGLE_QUOTED_PATH.replace_all(&s, "'[REDACTED_PATH]'").into_owned();
    s = BACKTICK_QUOTED_PATH.replace_all(&s, "`[REDACTED_PATH]`").into_owned();
    s = WINDOWS_PATH.replace_all(&s, "[REDACTED_PATH]").into_owned();
    s = WINDOWS_UNC_PATH.replace_all(&s, "[REDACTED_PATH]").into_owned();

    // Connector-separated paths such as "copy /tmp/a to /tmp/b: denied".
    if let Some(m) = PATH_CONNECTOR.find(&s) {
        let first = &s[..m.start()];
        let connector = &s[m.start()..m.end() - 1];
        let second = format!("/{}", &s[m.end()..]);
        return no_truncate(first) + connector + &no_truncate(&second);
    }

    // A path before the error delimiter: the first known error, else the first ": ".
    // ponytail: ASCII lowercasing keeps byte offsets aligned; Go lowers Unicode too.
    let lower = s.to_ascii_lowercase();
    let colon = KNOWN_ERRORS
        .iter()
        .filter_map(|word| lower.find(&format!(": {word}")))
        .min()
        .or_else(|| s.find(": "));
    if let Some(colon) = colon {
        let (prefix, suffix) = s.split_at(colon);
        if let Some(slash) = path_start(prefix.as_bytes()) {
            let lead = &prefix[..slash];
            let mut path = &prefix[slash..];
            let trail_start = path.trim_end_matches([')', ']', '}', '>']).len();
            let trail = &path[trail_start..];
            path = &path[..trail_start];
            let path = if path.contains(" /") {
                vec!["[REDACTED_PATH]"; path.split(" /").count()].join(" ")
            } else {
                "[REDACTED_PATH]".to_owned()
            };
            s = format!("{lead}{path}{trail}{suffix}");
        }
    }

    for _ in 0..3 {
        let next = UNIX_PATH_STANDARD.replace_all(&s, "${1}[REDACTED_PATH]").into_owned();
        if next == s {
            break;
        }
        s = next;
    }
    s = FILE_EXT_PATH.replace_all(&s, "${1}[REDACTED_PATH]").into_owned();
    s = COOKIE.replace_all(&s, "Cookie: [REDACTED]").into_owned();
    s = AUTH_HEADER.replace_all(&s, "Authorization: [REDACTED]").into_owned();
    s = SK_KEY.replace_all(&s, "sk-[REDACTED]").into_owned();
    s = BEARER.replace_all(&s, "Bearer [REDACTED]").into_owned();
    s = INVALID_TOKEN.replace_all(&s, "${1} token [REDACTED]").into_owned();
    s = NATURAL_SECRET.replace_all(&s, "${1}: [REDACTED]").into_owned();
    KV.replace_all(&s, "${1}[REDACTED]").into_owned()
}

/// The first `/` that starts a path: not part of `//` or a URL scheme, and at the start
/// or after whitespace or an opening delimiter.
fn path_start(prefix: &[u8]) -> Option<usize> {
    (0..prefix.len()).find(|&i| {
        if prefix[i] != b'/' {
            return false;
        }
        if i > 0 && prefix[i - 1] == b'/' {
            return false;
        }
        let head = &prefix[..i];
        if i >= 6 && (head.ends_with(b"http:/") || head.ends_with(b"https:/") || head.ends_with(b"://")) {
            return false;
        }
        i == 0
            || matches!(
                prefix[i - 1],
                b' ' | b'\t' | b'(' | b'[' | b'{' | b'<' | b'"' | b'\'' | b'`' | b'='
            )
    })
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    /// Goldens from Go `SanitizeUpstreamErrorSummary` and `ExtractUpstreamErrorSummary`
    /// (tests/reference/server/main.go).
    fn golden(section: &str) -> Vec<(String, String)> {
        let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/server_go.json")).unwrap();
        fixture[section]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p["in"].as_str().unwrap().to_owned(),
                    p["out"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn sanitize_matches_go() {
        let cases = golden("sanitize");
        assert!(cases.len() > 40);
        for (input, expected) in cases {
            assert_eq!(super::summary(&input), expected, "input: {input:?}");
        }
    }

    #[test]
    fn extract_matches_go() {
        for (input, expected) in golden("extract") {
            assert_eq!(crate::dispatch::upstream_summary(&input), expected, "input: {input:?}");
        }
    }
}
