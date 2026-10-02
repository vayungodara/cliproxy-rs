//! Byte-preserving JSON edits with tidwall/sjson v1.2.5 semantics.
//!
//! Claude request rewriting must reproduce Go's bytes exactly: CCH signs the final
//! body, and prompt caching keys on it. Untouched members keep their order, spacing
//! and number spelling; new members are appended before the parent's last `}`.
//! Lookups use the gjson crate (same path syntax as Go's gjson).
//!
//! ponytail: simple dotted paths only (keys and array indexes, no escapes,
//! wildcards or modifiers). Every Claude rule uses simple paths.

use gjson::{Kind, Value};

/// Looks up `path` in `json`.
pub(crate) fn get<'a>(json: &'a str, path: &'a str) -> Value<'a> {
    gjson::get(json, path)
}

/// Byte offset of a value that `get` borrowed from `json`.
pub(crate) fn offset(json: &str, value: &Value<'_>) -> Option<usize> {
    let raw = value.json();
    let start = (raw.as_ptr() as usize).checked_sub(json.as_ptr() as usize)?;
    (!raw.is_empty() && start + raw.len() <= json.len() && &json[start..start + raw.len()] == raw).then_some(start)
}

/// Go's `encoding/json` string marshalling (HTML-escaping, U+2028/9 escaped).
pub(crate) fn go_string(s: &str) -> String {
    marshal(s, true)
}

/// `json.Encoder` with `SetEscapeHTML(false)`, i.e. `JSON.stringify` for these inputs.
pub(crate) fn js_string(s: &str) -> String {
    marshal(s, false)
}

fn marshal(s: &str, html: bool) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' | '>' | '&' if html => out.push_str(&format!("\\u{:04x}", c as u32)),
            '\u{2028}' | '\u{2029}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// sjson's `appendStringify`: only strings that need escaping go through Go's marshaller.
pub(crate) fn sjson_string(s: &str) -> String {
    if s.bytes()
        .any(|b| !(b' '..=0x7f).contains(&b) || b == b'"' || b == b'\\')
    {
        go_string(s)
    } else {
        format!("\"{s}\"")
    }
}

/// `sjson.SetRawBytes`.
pub(crate) fn set_raw(json: &str, path: &str, raw: &str) -> String {
    let parts: Vec<&str> = path.split('.').collect();
    let mut out = String::with_capacity(json.len() + raw.len() + path.len() + 4);
    match append_paths(&mut out, json, &parts, raw, false) {
        Ok(()) => out,
        Err(()) => json.to_owned(),
    }
}

/// `sjson.SetBytes` with a Go string value.
pub(crate) fn set_str(json: &str, path: &str, value: &str) -> String {
    set_raw(json, path, &sjson_string(value))
}

/// `sjson.DeleteBytes`. Missing paths leave the input unchanged.
pub(crate) fn delete(json: &str, path: &str) -> String {
    let parts: Vec<&str> = path.split('.').collect();
    let mut out = String::with_capacity(json.len());
    match append_paths(&mut out, json, &parts, "", true) {
        Ok(()) => out,
        Err(()) => json.to_owned(),
    }
}

/// Go's `gjson.Result.Index > 0` lookup of one path component inside `json`.
fn component<'a>(json: &'a str, part: &'a str, delete: bool) -> Option<(usize, &'a str)> {
    let value = if delete && part == "-1" {
        let count = gjson::get(json, "#").i64();
        if count <= 0 {
            return None;
        }
        // The index string must outlive the lookup; resolve by iteration instead.
        let parsed = gjson::parse(json);
        let items = parsed.array();
        let last = items.into_iter().last()?;
        let start = offset(json, &last)?;
        return Some((start, &json[start..start + last.json().len()]));
    } else {
        gjson::get(json, part)
    };
    let start = offset(json, &value).filter(|start| *start > 0)?;
    Some((start, &json[start..start + value.json().len()]))
}

fn append_paths(buf: &mut String, json: &str, parts: &[&str], raw: &str, delete: bool) -> Result<(), ()> {
    if let Some((start, found)) = component(json, parts[0], delete) {
        let end = start + found.len();
        if parts.len() > 1 {
            buf.push_str(&json[..start]);
            append_paths(buf, found, &parts[1..], raw, delete)?;
            buf.push_str(&json[end..]);
            return Ok(());
        }
        buf.push_str(&json[..start]);
        let mut skip = 0;
        if delete {
            if delete_tail_item(buf) {
                // The member was first: drop the comma that follows it instead.
                for (i, b) in json[end..].bytes().enumerate() {
                    if b <= b' ' {
                        continue;
                    }
                    if b == b',' {
                        skip = i + 1;
                    }
                    break;
                }
            }
        } else {
            buf.push_str(raw);
        }
        buf.push_str(&json[end + skip..]);
        return Ok(());
    }
    if delete {
        return Err(());
    }
    let numeric = parts[0]
        .parse::<usize>()
        .ok()
        .filter(|_| parts[0].bytes().all(|b| b.is_ascii_digit()));
    let mut json = json;
    if json.bytes().all(|b| b <= b' ') {
        json = if numeric.is_some() { "[]" } else { "{}" };
    }
    let mut parsed = gjson::parse(json);
    if !matches!(parsed.kind(), Kind::Object | Kind::Array) {
        json = if numeric.is_some() { "[]" } else { "{}" };
        parsed = gjson::parse(json);
    }
    let whole = parsed.json();
    let comma = whole[1..]
        .bytes()
        .find(|b| *b > b' ')
        .is_some_and(|b| b != b'}' && b != b']');
    match whole.as_bytes()[0] {
        b'{' => {
            let end = whole.rfind('}').ok_or(())?;
            buf.push_str(&whole[..end]);
            if comma {
                buf.push(',');
            }
            build(buf, false, parts, raw);
            buf.push('}');
            Ok(())
        }
        b'[' => {
            let Some(n) = numeric else {
                if parts[0] != "-1" {
                    return Err(());
                }
                let trimmed = whole.trim_matches(|c: char| c <= ' ');
                buf.push_str(trimmed.strip_suffix(']').unwrap_or(trimmed));
                if comma {
                    buf.push(',');
                }
                build(buf, true, parts, raw);
                buf.push(']');
                return Ok(());
            };
            buf.push('[');
            let items = parsed.array();
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    buf.push(',');
                }
                buf.push_str(item.json());
            }
            if items.is_empty() {
                buf.push_str(&"null,".repeat(n));
            } else {
                buf.push_str(&",null".repeat(n.saturating_sub(items.len())));
                if comma {
                    buf.push(',');
                }
            }
            build(buf, true, parts, raw);
            buf.push(']');
            Ok(())
        }
        _ => Err(()),
    }
}

/// sjson `appendBuild`.
fn build(buf: &mut String, array: bool, parts: &[&str], raw: &str) {
    if !array {
        buf.push_str(&sjson_string(parts[0]));
        buf.push(':');
    }
    if parts.len() > 1 {
        let next = parts[1];
        if let Some(n) = next
            .parse::<usize>()
            .ok()
            .filter(|_| next.bytes().all(|b| b.is_ascii_digit()))
        {
            buf.push('[');
            buf.push_str(&"null,".repeat(n));
            build(buf, true, &parts[1..], raw);
            buf.push(']');
        } else if next == "-1" {
            buf.push('[');
            build(buf, true, &parts[1..], raw);
            buf.push(']');
        } else {
            buf.push('{');
            build(buf, false, &parts[1..], raw);
            buf.push('}');
        }
    } else {
        buf.push_str(raw);
    }
}

/// sjson `deleteTailItem`: removes the preceding `,"key":` or `"key":`. Returns true
/// when the deleted member was the first one (the following comma must go instead).
fn delete_tail_item(buf: &mut String) -> bool {
    let bytes = buf.as_bytes();
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b'[' => return true,
            b',' => {
                buf.truncate(i);
                return false;
            }
            b':' => {
                // Walk back over the key string.
                let mut j = i;
                while j > 0 {
                    j -= 1;
                    if bytes[j] != b'"' {
                        continue;
                    }
                    while j > 0 {
                        j -= 1;
                        if bytes[j] != b'"' {
                            continue;
                        }
                        if j > 0 && bytes[j - 1] == b'\\' {
                            j -= 1;
                            continue;
                        }
                        while j > 0 {
                            j -= 1;
                            match bytes[j] {
                                b'{' => {
                                    buf.truncate(j + 1);
                                    return true;
                                }
                                b',' => {
                                    buf.truncate(j);
                                    return false;
                                }
                                _ => {}
                            }
                        }
                        return false;
                    }
                    return false;
                }
                return false;
            }
            _ => {}
        }
    }
    false
}

/// `gjson.Result.String()` on a lookup, owned.
pub(crate) fn string(json: &str, path: &str) -> String {
    get(json, path).str().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sjson_cases_generated_by_go_match() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("claude/testdata/go_executor.json")).unwrap();
        for case in fixture["sjson"].as_array().unwrap() {
            let (json, path) = (case["json"].as_str().unwrap(), case["path"].as_str().unwrap());
            let value = case["value"].as_str().unwrap_or_default();
            let out = match case["op"].as_str().unwrap() {
                "raw" => set_raw(json, path, value),
                "delete" => delete(json, path),
                _ => set_str(json, path, value),
            };
            assert_eq!(out, case["out"].as_str().unwrap(), "{case}");
        }
    }

    #[test]
    fn set_and_delete_match_sjson_byte_layout() {
        // Expected bytes from sjson v1.2.5 (see tests/reference fixtures for the
        // generated cross-check; these are the hand-traced core cases).
        assert_eq!(set_raw(r#"{"a":1 }"#, "b", "2"), r#"{"a":1 ,"b":2}"#);
        assert_eq!(set_raw("{}", "b", "2"), r#"{"b":2}"#);
        assert_eq!(set_raw(r#"{"a":{"x":1}}"#, "a.x", "true"), r#"{"a":{"x":true}}"#);
        assert_eq!(
            set_raw(r#"{"a":1}"#, "m.user_id", r#""u""#),
            r#"{"a":1,"m":{"user_id":"u"}}"#
        );
        assert_eq!(set_raw(r#"{"a":[1,2]}"#, "a.3", "9"), r#"{"a":[1,2,null,9]}"#);
        assert_eq!(set_raw(r#"{"a":[]}"#, "a.1", "9"), r#"{"a":[null,9]}"#);
        assert_eq!(delete(r#"{"a":1, "b":2,"c":3}"#, "b"), r#"{"a":1,"c":3}"#);
        assert_eq!(delete(r#"{"a":1, "b":2}"#, "a"), r#"{ "b":2}"#);
        assert_eq!(delete(r#"{ "a" : 1 }"#, "a"), r#"{ }"#);
        assert_eq!(delete(r#"{"a":[1,2,3]}"#, "a.0"), r#"{"a":[2,3]}"#);
        assert_eq!(delete(r#"{"a":[1,2,3]}"#, "a.2"), r#"{"a":[1,2]}"#);
        assert_eq!(delete(r#"{"a":1}"#, "zz"), r#"{"a":1}"#);
        assert_eq!(set_str(r#"{"a":1}"#, "t", "x<y"), r#"{"a":1,"t":"x<y"}"#);
        assert_eq!(set_str(r#"{"a":1}"#, "t", "é<"), r#"{"a":1,"t":"é\u003c"}"#);
        assert_eq!(js_string("a\u{2028}<\u{1}"), "\"a\\u2028<\\u0001\"");
    }
}
