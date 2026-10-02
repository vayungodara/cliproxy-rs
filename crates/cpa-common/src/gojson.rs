//! Raw JSON reads and edits with Go's tidwall/gjson and tidwall/sjson v1.2.5 semantics,
//! so rewritten bodies match CLIProxyAPI byte for byte: untouched bytes, key order and
//! number spelling are kept, new keys are appended before the closing brace, and
//! deletes take the neighbouring comma exactly as sjson does.
//!
//! Every edit returns the input unchanged where sjson returns an error (Go callers
//! ignore those errors and keep the original bytes).
//!
//! ponytail: simple dotted paths only (`a.b.0.c`, `\` escapes, `-1` append). sjson's
//! complex-path writes (`#`, `*`, `?`, `|`, `@`) are not used by the ported callers;
//! port `setComplexPath` if one needs them. Bodies are `&str`: callers holding
//! non-UTF-8 bytes must decide before editing (gjson.rs is UTF-8 only).

use crate::gostr::GoStr;
use gjson::{Kind, Value};

/// gjson `Result.String()`: numbers keep their raw spelling when it is an optional `-`
/// followed only by digits (even none), otherwise they are reformatted the way
/// `strconv.FormatFloat(f, 'f', -1, 64)` does; null is empty; objects and arrays are raw.
///
/// ponytail: malformed number tokens (`1_024`, `-`) are scanned by gjson.rs, which can
/// stop at different bytes than Go's scanner. Inputs that pass `valid()` are unaffected;
/// every thinking/signature path that reads numbers validates first.
pub fn go_str(v: &Value<'_>) -> String {
    match v.kind() {
        Kind::Number => {
            let raw = v.json();
            let digits = raw.strip_prefix('-').unwrap_or(raw);
            if digits.bytes().all(|b| b.is_ascii_digit()) {
                raw.to_owned()
            } else {
                format_float(go_num(raw))
            }
        }
        _ => v.str().to_owned(),
    }
}

/// `strconv.FormatFloat(f, 'f', -1, 64)`: shortest round-trip digits, no exponent.
pub fn format_float(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "+Inf".into() } else { "-Inf".into() }
    } else {
        format!("{f}")
    }
}

/// gjson's number parse of a raw literal (`strconv.ParseFloat`, 0 on failure).
fn go_num(raw: &str) -> f64 {
    raw.parse::<f64>().unwrap_or(0.0)
}

/// gjson `parseInt`: optional `-` then ASCII digits only.
fn parse_int(s: &str) -> Option<i64> {
    let (neg, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    if digits.is_empty() {
        return None;
    }
    let mut n: i64 = 0;
    for b in digits.bytes() {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.wrapping_mul(10).wrapping_add(i64::from(b - b'0'));
    }
    Some(if neg { n.wrapping_neg() } else { n })
}

/// gjson `Result.Int()`.
pub fn go_int(v: &Value<'_>) -> i64 {
    match v.kind() {
        Kind::True => 1,
        Kind::String => parse_int(v.str()).unwrap_or(0),
        Kind::Number => {
            let num = go_num(v.json());
            const MAX_SAFE: f64 = 9_007_199_254_740_991.0;
            if (-MAX_SAFE..=MAX_SAFE).contains(&num) && num == (num as i64) as f64 {
                return num as i64;
            }
            if let Some(n) = parse_int(v.json()) {
                return n;
            }
            if num.is_nan() || num >= i64::MAX as f64 || num < i64::MIN as f64 {
                // amd64 CVTTSD2SQ yields the "integer indefinite" value.
                i64::MIN
            } else {
                num as i64
            }
        }
        _ => 0,
    }
}

/// gjson `Result.Bool()`.
pub fn go_bool(v: &Value<'_>) -> bool {
    match v.kind() {
        Kind::True => true,
        Kind::String => matches!(v.str().go_lower().as_str(), "1" | "t" | "true"),
        Kind::Number => go_num(v.json()) != 0.0,
        _ => false,
    }
}

/// gjson `Result.IsBool()`.
pub fn is_bool(v: &Value<'_>) -> bool {
    matches!(v.kind(), Kind::True | Kind::False)
}

/// `len(result.Map())` for an object: the number of distinct keys.
pub fn map_len(v: &Value<'_>) -> usize {
    let mut keys = std::collections::HashSet::new();
    if v.kind() == Kind::Object {
        v.each(|k, _| {
            keys.insert(k.str().to_owned());
            true
        });
    }
    keys.len()
}

/// `result.IsObject() && len(result.Map()) == 0` at `path`.
pub fn is_empty_object(json: &str, path: &str) -> bool {
    let v = gjson::get(json, path);
    v.kind() == Kind::Object && map_len(&v) == 0
}

/// gjson `ValidBytes`.
pub fn valid(json: &str) -> bool {
    gjson::valid(json)
}

/// A path segment after sjson's `parsePath`.
#[derive(Clone)]
struct Part {
    /// Key with escapes removed (written into new objects).
    part: String,
    /// Key as gjson reads it (escapes kept).
    gpart: String,
    force: bool,
}

fn simple_char(ch: u8) -> bool {
    !matches!(ch, b'|' | b'#' | b'@' | b'*' | b'?')
}

/// sjson `parsePath`, applied repeatedly. `None` for complex paths.
fn parse_path(mut path: &str) -> Option<Vec<Part>> {
    let mut out = Vec::new();
    loop {
        let mut force = false;
        if let Some(rest) = path.strip_prefix(':') {
            force = true;
            path = rest;
        }
        let bytes = path.as_bytes();
        let mut i = 0;
        let mut done = None;
        while i < bytes.len() {
            if bytes[i] == b'.' {
                done = Some((path[..i].to_owned(), path[..i].to_owned(), Some(&path[i + 1..])));
                break;
            }
            if !simple_char(bytes[i]) {
                return None;
            }
            if bytes[i] == b'\\' {
                let mut epart = bytes[..i].to_vec();
                let mut gpart = bytes[..=i].to_vec();
                i += 1;
                let mut rest = None;
                if i < bytes.len() {
                    epart.push(bytes[i]);
                    gpart.push(bytes[i]);
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == b'\\' {
                            gpart.push(b'\\');
                            i += 1;
                            if i < bytes.len() {
                                epart.push(bytes[i]);
                                gpart.push(bytes[i]);
                            }
                        } else if bytes[i] == b'.' {
                            rest = Some(&path[i + 1..]);
                            break;
                        } else if !simple_char(bytes[i]) {
                            return None;
                        } else {
                            epart.push(bytes[i]);
                            gpart.push(bytes[i]);
                        }
                        i += 1;
                    }
                }
                done = Some((String::from_utf8(epart).ok()?, String::from_utf8(gpart).ok()?, rest));
                break;
            }
            i += 1;
        }
        let (part, gpart, rest) = done.unwrap_or_else(|| (path.to_owned(), path.to_owned(), None));
        out.push(Part { part, gpart, force });
        match rest {
            Some(rest) => path = rest,
            None => return Some(out),
        }
    }
}

/// Go `encoding/json` string encoding (HTML-escaped, U+2028/2029 escaped).
pub fn marshal_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' | '>' | '&' | '\u{2028}' | '\u{2029}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// sjson `appendStringify`.
fn stringify(s: &str) -> String {
    if s.bytes()
        .any(|b| !(b' '..=0x7f).contains(&b) || b == b'"' || b == b'\\')
    {
        marshal_string(s)
    } else {
        format!("\"{s}\"")
    }
}

/// sjson `atoui`: digits only (empty is 0), accumulated in Go's 64-bit `int` with
/// wrapping, so huge indexes turn negative exactly as in Go.
fn atoui(p: &Part) -> Option<i64> {
    if p.force {
        return None;
    }
    let mut n = 0i64;
    for b in p.part.bytes() {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.wrapping_mul(10).wrapping_add(i64::from(b - b'0'));
    }
    Some(n)
}

/// sjson `appendRepeat`: a non-positive count appends nothing.
fn repeat(buf: &mut String, s: &str, n: i64) {
    for _ in 0..n.max(0) {
        buf.push_str(s);
    }
}

/// gjson's array element lookup for a simple path part: `parseUint` digits only,
/// wrapping, then `int(n)`. Returns the element's byte range in `json`.
fn array_element(json: &str, part: &str) -> Option<(usize, usize)> {
    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut n = 0u64;
    for b in part.bytes() {
        n = n.wrapping_mul(10).wrapping_add(u64::from(b - b'0'));
    }
    let index = usize::try_from(n as i64).ok()?;
    let root = gjson::parse(json);
    let items = root.array();
    let item = items.get(index)?;
    offset(json, item.json()).map(|o| (o, item.json().len()))
}

fn append_build(buf: &mut String, array: bool, paths: &[Part], raw: &str, quote: bool) {
    if !array {
        buf.push_str(&stringify(&paths[0].part));
        buf.push(':');
    }
    if paths.len() > 1 {
        let numeric = atoui(&paths[1]);
        if numeric.is_some() || (!paths[1].force && paths[1].part == "-1") {
            buf.push('[');
            repeat(buf, "null,", numeric.unwrap_or(0));
            append_build(buf, true, &paths[1..], raw, quote);
            buf.push(']');
        } else {
            buf.push('{');
            append_build(buf, false, &paths[1..], raw, quote);
            buf.push('}');
        }
    } else if quote {
        buf.push_str(&stringify(raw));
    } else {
        buf.push_str(raw);
    }
}

/// sjson `deleteTailItem`: drops the key (or comma) before a deleted value.
fn delete_tail_item(buf: &mut String) -> bool {
    let b = buf.as_bytes();
    let mut i = b.len();
    while i > 0 {
        i -= 1;
        match b[i] {
            b'[' => return true,
            b',' => {
                buf.truncate(i);
                return false;
            }
            b':' => {
                // Walk back over the key string, then to the preceding ',' or '{'.
                let mut j = i as isize - 1;
                while j >= 0 {
                    if b[j as usize] == b'"' {
                        j -= 1;
                        while j >= 0 {
                            if b[j as usize] == b'"' {
                                j -= 1;
                                if j >= 0 && b[j as usize] == b'\\' {
                                    j -= 1;
                                    continue;
                                }
                                while j >= 0 {
                                    match b[j as usize] {
                                        b'{' => {
                                            buf.truncate(j as usize + 1);
                                            return true;
                                        }
                                        b',' => {
                                            buf.truncate(j as usize);
                                            return false;
                                        }
                                        _ => j -= 1,
                                    }
                                }
                            }
                            j -= 1;
                        }
                        break;
                    }
                    j -= 1;
                }
                return false;
            }
            _ => {}
        }
    }
    false
}

/// Byte offset of `value` inside `json` when gjson returned a borrowed slice.
fn offset(json: &str, value: &str) -> Option<usize> {
    let start = (value.as_ptr() as usize).checked_sub(json.as_ptr() as usize)?;
    (start + value.len() <= json.len()).then_some(start)
}

enum Edit<'a> {
    Set { raw: &'a str, quote: bool },
    Delete,
}

/// sjson `appendRawPaths`. `None` is an sjson error (including "no change").
fn append_raw_paths(buf: &mut String, json: &str, paths: &[Part], edit: &Edit<'_>) -> Option<()> {
    let del = matches!(edit, Edit::Delete);
    let mut found: Option<(usize, usize)> = None;
    if del && paths[0].part == "-1" && !paths[0].force {
        let count = gjson::get(json, "#").i64();
        if count > 0 {
            let index = (count - 1).to_string();
            let last = gjson::get(json, &index);
            found = offset(json, last.json()).map(|o| (o, last.json().len()));
        }
    }
    if found.is_none() {
        if json.trim_start_matches(|c: char| c <= ' ').starts_with('[') {
            found = array_element(json, &paths[0].gpart).filter(|(o, _)| *o > 0);
        } else {
            let res = gjson::get(json, &paths[0].gpart);
            if res.exists() {
                found = offset(json, res.json())
                    .filter(|o| *o > 0)
                    .map(|o| (o, res.json().len()));
            }
        }
    }
    if let Some((index, len)) = found {
        if paths.len() > 1 {
            buf.push_str(&json[..index]);
            append_raw_paths(buf, &json[index..index + len], &paths[1..], edit)?;
            buf.push_str(&json[index + len..]);
            return Some(());
        }
        buf.push_str(&json[..index]);
        let mut extra = 0;
        match edit {
            Edit::Delete => {
                if delete_tail_item(buf) {
                    let rest = &json.as_bytes()[index + len..];
                    for (j, &b) in rest.iter().enumerate() {
                        if b <= b' ' {
                            continue;
                        }
                        if b == b',' {
                            extra = j + 1;
                        }
                        break;
                    }
                }
            }
            Edit::Set { raw, quote: true } => buf.push_str(&stringify(raw)),
            Edit::Set { raw, quote: false } => buf.push_str(raw),
        }
        buf.push_str(&json[index + len + extra..]);
        return Some(());
    }
    let Edit::Set { raw, quote } = edit else {
        return None;
    };
    let numeric = atoui(&paths[0]);
    let mut json = json;
    if json.bytes().all(|b| b <= b' ') {
        json = if numeric.is_some() { "[]" } else { "{}" };
    }
    let mut root = gjson::parse(json);
    if !matches!(root.kind(), gjson::Kind::Object | gjson::Kind::Array) {
        json = if numeric.is_some() { "[]" } else { "{}" };
        root = gjson::parse(json);
    }
    let raw_root = root.json();
    let comma = raw_root
        .bytes()
        .skip(1)
        .find(|b| *b > b' ')
        .is_some_and(|b| b != b'}' && b != b']');
    if raw_root.starts_with('{') {
        let end = raw_root.rfind('}').filter(|e| *e > 0).unwrap_or(0);
        buf.push_str(&raw_root[..end]);
        if comma {
            buf.push(',');
        }
        append_build(buf, false, paths, raw, *quote);
        buf.push('}');
        return Some(());
    }
    // Array.
    if numeric.is_none() {
        if paths[0].part != "-1" || paths[0].force {
            return None;
        }
        let trimmed = raw_root.trim_matches(|c: char| c <= ' ');
        buf.push_str(trimmed.strip_suffix(']').unwrap_or(trimmed));
        if comma {
            buf.push(',');
        }
        append_build(buf, true, paths, raw, *quote);
        buf.push(']');
        return Some(());
    }
    let n = numeric.unwrap_or(0);
    buf.push('[');
    let items = root.array();
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            buf.push(',');
        }
        buf.push_str(item.json());
    }
    if items.is_empty() {
        repeat(buf, "null,", n);
    } else {
        repeat(buf, ",null", n.wrapping_sub(items.len() as i64));
        if comma {
            buf.push(',');
        }
    }
    append_build(buf, true, paths, raw, *quote);
    buf.push(']');
    Some(())
}

fn apply(json: &str, path: &str, edit: Edit<'_>) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let paths = parse_path(path)?;
    let mut buf = String::with_capacity(json.len() + 16);
    append_raw_paths(&mut buf, json, &paths, &edit)?;
    Some(buf)
}

/// `sjson.SetRawBytes`.
pub fn set_raw(json: &str, path: &str, raw: &str) -> String {
    apply(json, path, Edit::Set { raw, quote: false }).unwrap_or_else(|| json.to_owned())
}

/// `sjson.SetBytes` with a string value.
pub fn set_str(json: &str, path: &str, value: &str) -> String {
    apply(
        json,
        path,
        Edit::Set {
            raw: value,
            quote: true,
        },
    )
    .unwrap_or_else(|| json.to_owned())
}

/// `sjson.DeleteBytes`.
pub fn delete(json: &str, path: &str) -> String {
    apply(json, path, Edit::Delete).unwrap_or_else(|| json.to_owned())
}

/// `helps.SetStringIfDifferent`.
pub fn set_str_if_different(json: &str, path: &str, value: &str) -> String {
    let current = gjson::get(json, path);
    if current.kind() == gjson::Kind::String && current.str() == value {
        return json.to_owned();
    }
    set_str(json, path, value)
}

/// `helps.SetBoolIfDifferent`.
pub fn set_bool_if_different(json: &str, path: &str, value: bool) -> String {
    let current = gjson::get(json, path);
    if current.kind() == if value { gjson::Kind::True } else { gjson::Kind::False } {
        return json.to_owned();
    }
    set_raw(json, path, if value { "true" } else { "false" })
}

/// `sjson.SetBytes` with an integer value.
pub fn set_int(json: &str, path: &str, value: i64) -> String {
    set_raw(json, path, &value.to_string())
}

/// `sjson.SetBytes` with a bool value.
pub fn set_bool(json: &str, path: &str, value: bool) -> String {
    set_raw(json, path, if value { "true" } else { "false" })
}

/// `common.JoinRawArray`.
pub fn join_array<S: AsRef<str>>(items: &[S]) -> String {
    let mut out = String::from("[");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(item.as_ref());
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected strings are tidwall/sjson v1.2.5 and gjson outputs recorded by
    // tests/reference/main.go.
    #[test]
    fn matches_go_sjson_vectors() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!("../tests/fixtures/sjson_go.json")).unwrap();
        let cases = fixture["edits"].as_array().unwrap();
        assert!(cases.len() > 20);
        for case in cases {
            let json = case["json"].as_str().unwrap();
            let path = case["path"].as_str().unwrap();
            let got = match case["op"].as_str().unwrap() {
                "delete" => delete(json, path),
                "set_raw" => set_raw(json, path, case["value"].as_str().unwrap()),
                "set_str" => set_str(json, path, case["value"].as_str().unwrap()),
                op => panic!("{op}"),
            };
            assert_eq!(got, case["out"].as_str().unwrap(), "{case}");
        }
        let reads = fixture["reads"].as_array().unwrap();
        assert!(reads.len() > 10);
        for case in reads {
            let v = gjson::get(case["json"].as_str().unwrap(), case["path"].as_str().unwrap());
            assert_eq!(go_str(&v), case["string"].as_str().unwrap(), "{case}");
            assert_eq!(go_int(&v), case["int"].as_i64().unwrap(), "{case}");
            assert_eq!(go_bool(&v), case["bool"].as_bool().unwrap(), "{case}");
            assert_eq!(v.exists(), case["exists"].as_bool().unwrap(), "{case}");
        }
    }
}
