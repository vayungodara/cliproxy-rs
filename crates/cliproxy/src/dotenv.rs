//! `.env` loading as Go's `godotenv.Load` (joho/godotenv v1.5.1) does it: the whole file
//! parses or nothing is set, existing variables win, `$VAR` expands from keys earlier
//! in the same file only.
use std::collections::HashMap;

/// godotenv `isSpace`: spaces without line breaks.
fn is_space(c: char) -> bool {
    matches!(c, '\t' | '\x0B' | '\x0C' | '\r' | ' ' | '\u{85}' | '\u{A0}')
}

/// godotenv `parseBytes`; keys keep file order.
pub fn parse(src: &str) -> Result<Vec<(String, String)>, String> {
    let src = src.replace("\r\n", "\n");
    let mut out: Vec<(String, String)> = Vec::new();
    let mut vars: HashMap<String, String> = HashMap::new();
    let mut rest: &str = &src;
    loop {
        // getStatementStart: skip whitespace and comment lines.
        loop {
            match rest.find(|c: char| !c.is_whitespace()) {
                None => return Ok(out),
                Some(pos) => rest = &rest[pos..],
            }
            if !rest.starts_with('#') {
                break;
            }
            match rest.find('\n') {
                None => return Ok(out),
                Some(pos) => rest = &rest[pos..],
            }
        }
        let (key, after) = locate_key(rest)?;
        let (value, after) = extract_value(after, &vars)?;
        vars.insert(key.clone(), value.clone());
        match out.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => out.push((key, value)),
        }
        rest = after;
    }
}

/// godotenv `locateKeyName`: bytes are judged one at a time as Latin-1 runes, as Go's
/// byte loop does.
fn locate_key(src: &str) -> Result<(String, &str), String> {
    let mut src = src.trim_start_matches(is_space);
    if let Some(trimmed) = src.strip_prefix("export")
        && trimmed.starts_with(is_space)
    {
        src = trimmed.trim_start_matches(is_space);
    }
    let mut key = "";
    let mut offset = 0;
    for (i, &b) in src.as_bytes().iter().enumerate() {
        let c = char::from(b);
        if is_space(c) {
            continue;
        }
        match b {
            b'=' | b':' => {
                key = &src[..i];
                offset = i + 1;
                break;
            }
            b'_' => {}
            _ if c.is_alphabetic() || c.is_numeric() || b == b'.' => {}
            _ => {
                return Err(format!(
                    "unexpected character {} in variable name near {}",
                    // Go's string(byte) is the rune U+00XX, not the raw byte.
                    cpa_common::gostr::quote(c.to_string()),
                    cpa_common::gostr::quote(src)
                ));
            }
        }
    }
    if src.is_empty() {
        return Err("zero length string".into());
    }
    let key = key.trim_end_matches(char::is_whitespace).to_owned();
    // `offset` follows an ASCII byte, so it is a character boundary.
    Ok((key, src[offset..].trim_start_matches(is_space)))
}

/// godotenv `extractVarValue`.
fn extract_value<'a>(src: &'a str, vars: &HashMap<String, String>) -> Result<(String, &'a str), String> {
    let quote = match src.as_bytes().first() {
        Some(&q @ (b'"' | b'\'')) => q,
        _ => {
            let end = src.find(['\n', '\r']).unwrap_or(src.len());
            let line: Vec<char> = src[..end].chars().collect();
            let mut end_of_var = line.len();
            for i in (1..line.len()).rev() {
                if line[i] == '#' && is_space(line[i - 1]) {
                    end_of_var = i;
                    break;
                }
            }
            let value: String = line[..end_of_var].iter().collect();
            return Ok((expand_variables(value.trim_matches(is_space), vars), &src[end..]));
        }
    };
    let bytes = src.as_bytes();
    for i in 1..bytes.len() {
        if bytes[i] != quote || bytes[i - 1] == b'\\' {
            continue;
        }
        let q = char::from(quote);
        let value = src[..i].trim_end_matches(q).trim_start_matches(q);
        let value = if quote == b'"' {
            expand_variables(&expand_escapes(value), vars)
        } else {
            value.to_owned()
        };
        return Ok((value, &src[i + 1..]));
    }
    let end = src.find('\n').unwrap_or(src.len());
    Err(format!("unterminated quoted value {}", &src[..end]))
}

/// godotenv `expandEscapes`: `\n` and `\r` become line breaks, then a backslash is
/// dropped before any character but `$`.
fn expand_escapes(s: &str) -> String {
    let mut first = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('\\', Some(&n)) if n != '\n' => {
                chars.next();
                match n {
                    'n' => first.push('\n'),
                    'r' => first.push('\r'),
                    n => {
                        first.push('\\');
                        first.push(n);
                    }
                }
            }
            (c, _) => first.push(c),
        }
    }
    let mut out = String::with_capacity(first.len());
    let mut chars = first.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('\\', Some(&n)) if n != '$' => {
                chars.next();
                out.push(n);
            }
            (c, _) => out.push(c),
        }
    }
    out
}

/// godotenv `expandVariables`: `(\\)?(\$)(\()?\{?([A-Z0-9_]+)?\}?`. Go tests
/// submatch 2 (always `$`) for `(`, so a `$(` prefix still expands the name.
fn expand_variables(v: &str, vars: &HashMap<String, String>) -> String {
    let c: Vec<char> = v.chars().collect();
    let mut out = String::with_capacity(v.len());
    let mut i = 0;
    while i < c.len() {
        let escaped = c[i] == '\\' && c.get(i + 1) == Some(&'$');
        let dollar = if escaped { i + 1 } else { i };
        if c[dollar] != '$' {
            out.push(c[i]);
            i += 1;
            continue;
        }
        let mut j = dollar + 1;
        let paren = c.get(j) == Some(&'(');
        if paren {
            j += 1;
        }
        if c.get(j) == Some(&'{') {
            j += 1;
        }
        let name_start = j;
        while c
            .get(j)
            .is_some_and(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || *ch == '_')
        {
            j += 1;
        }
        let name: String = c[name_start..j].iter().collect();
        if c.get(j) == Some(&'}') {
            j += 1;
        }
        let matched: String = c[i..j].iter().collect();
        if escaped {
            out.push_str(&matched[matched.chars().next().map_or(0, char::len_utf8)..]);
        } else if !name.is_empty() {
            out.push_str(vars.get(&name).map_or("", String::as_str));
        } else {
            out.push_str(&matched);
        }
        i = j;
    }
    out
}

/// `godotenv.Load(<cwd>/.env)` as Go's main calls it: a missing file is silent, any
/// other failure warns and sets nothing. Must run before other threads start.
pub fn load_from_working_dir() {
    let Ok(wd) = std::env::current_dir() else { return };
    let path = wd.join(".env");
    // ponytail: the file is decoded as UTF-8 (lossy); Go keeps raw non-UTF-8 bytes in
    // quoted values. Upgrade: a byte-level parser that sets OsString values.
    let text = match std::fs::read(&path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(error = %e, "failed to load .env file");
            return;
        }
    };
    let vars = match parse(&text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "failed to load .env file");
            return;
        }
    };
    for (key, value) in vars {
        // Go's os.Setenv rejects these silently; Rust's set_var would panic.
        if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') || std::env::var_os(&key).is_some() {
            continue;
        }
        // SAFETY: called from `main` before the Tokio runtime or any other thread starts.
        unsafe { std::env::set_var(&key, &value) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inputs and results recorded from godotenv.Unmarshal (main_fixture_test.go).
    #[test]
    fn parses_like_godotenv() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/discovery_main_go.json")).unwrap();
        let cases = fixture["dotenv"].as_array().unwrap();
        assert!(cases.len() > 20);
        for case in cases {
            let input = case["in"].as_str().unwrap();
            match parse(input) {
                Ok(vars) => {
                    let got: std::collections::BTreeMap<_, _> = vars.into_iter().collect();
                    let want: std::collections::BTreeMap<String, String> =
                        serde_json::from_value(case["out"].clone()).unwrap_or_default();
                    assert_eq!(
                        case["err"].as_str(),
                        Some(""),
                        "{input:?}: Go failed, Rust parsed {got:?}"
                    );
                    assert_eq!(got, want, "{input:?}");
                }
                Err(e) => assert_eq!(e, case["err"].as_str().unwrap(), "{input:?}"),
            }
        }
    }
}
