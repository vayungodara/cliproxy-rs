//! Raw JSON edits with tidwall/sjson semantics and Go `encoding/json` output.
//!
//! Shared by the Kimi, Meta and Devin executors. Upstream bodies must match Go byte for
//! byte, so edits splice the original text exactly as sjson does (key order kept, new
//! keys appended before the closing brace, deletes take one neighbouring comma) instead
//! of re-serializing. Re-marshalled values (schema inlining, canonical comparisons) use
//! Go's rules: sorted object keys, HTML-escaped strings, number literals kept verbatim.
//!
//! ponytail: provider-neutral; hoist next to cpa-translate's private `json` helpers once
//! a shared raw-JSON module exists. Paths are plain dotted keys and array indexes; sjson's
//! escape and wildcard syntax is not needed by these providers.

use std::collections::BTreeMap;

use gjson::Kind;

pub(crate) fn valid(text: &str) -> bool {
    gjson::valid(text)
}

/// sjson `Set` with a Go string value: plain ASCII is quoted raw, anything else goes
/// through `encoding/json` (appendStringify).
pub(crate) fn stringify(s: &str) -> String {
    let must_marshal = s
        .bytes()
        .any(|b| !(b' '..=0x7f).contains(&b) || b == b'"' || b == b'\\');
    if must_marshal { go_quote(s) } else { format!("\"{s}\"") }
}

/// Go `encoding/json` string encoding with HTML escaping.
pub(crate) fn go_quote(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
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
            '<' | '>' | '&' | '\0'..='\u{1f}' => {
                let b = c as u8;
                out.push_str("\\u00");
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 15) as usize] as char);
            }
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Byte range of the value at one path component, as gjson's `Result.Index`. Array
/// elements are found by iteration because gjson copies indexed values.
fn locate(json: &str, part: &str) -> Option<(usize, usize)> {
    let root = gjson::parse(json);
    if root.kind() == Kind::Array {
        let n = numeric(part)?;
        let items = root.array();
        let item = items.get(n)?;
        return offset(json, item).filter(|&i| i > 0).map(|i| (i, item.json().len()));
    }
    let found = gjson::get(json, part);
    if !found.exists() {
        return None;
    }
    offset(json, &found).filter(|&i| i > 0).map(|i| (i, found.json().len()))
}

/// Byte offset of `value` inside `json`, when gjson returned a borrowed slice.
fn offset(json: &str, value: &gjson::Value<'_>) -> Option<usize> {
    let raw = value.json();
    let start = (raw.as_ptr() as usize).checked_sub(json.as_ptr() as usize)?;
    (start + raw.len() <= json.len() && &json[start..start + raw.len()] == raw).then_some(start)
}

fn numeric(part: &str) -> Option<usize> {
    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    part.parse().ok()
}

/// sjson deleteTailItem: drop the key (and its leading comma) that precedes a value.
/// Mirrors Go's index arithmetic exactly, including its escaped-quote skip.
fn delete_tail_item(buf: &mut String) -> bool {
    let b = buf.as_bytes();
    let at = |i: isize| b[i as usize];
    let mut i = b.len() as isize - 1;
    while i >= 0 {
        match at(i) {
            b'[' => return true,
            b',' => {
                buf.truncate(i as usize);
                return false;
            }
            b':' => {
                i -= 1;
                while i >= 0 {
                    if at(i) == b'"' {
                        i -= 1;
                        while i >= 0 {
                            if at(i) == b'"' {
                                i -= 1;
                                if i >= 0 && at(i) == b'\\' {
                                    i -= 2;
                                    continue;
                                }
                                while i >= 0 {
                                    match at(i) {
                                        b'{' => {
                                            buf.truncate(i as usize + 1);
                                            return true;
                                        }
                                        b',' => {
                                            buf.truncate(i as usize);
                                            return false;
                                        }
                                        _ => {}
                                    }
                                    i -= 1;
                                }
                            }
                            i -= 1;
                        }
                        break;
                    }
                    i -= 1;
                }
                return false;
            }
            _ => {}
        }
        i -= 1;
    }
    false
}

fn append_build(buf: &mut String, array: bool, paths: &[&str], raw: &str, stringify_value: bool) {
    if !array {
        buf.push_str(&stringify(paths[0]));
        buf.push(':');
    }
    if paths.len() > 1 {
        match numeric(paths[1]) {
            Some(n) => {
                buf.push('[');
                buf.push_str(&"null,".repeat(n));
                append_build(buf, true, &paths[1..], raw, stringify_value);
                buf.push(']');
            }
            None if paths[1] == "-1" => {
                buf.push('[');
                append_build(buf, true, &paths[1..], raw, stringify_value);
                buf.push(']');
            }
            None => {
                buf.push('{');
                append_build(buf, false, &paths[1..], raw, stringify_value);
                buf.push('}');
            }
        }
    } else if stringify_value {
        buf.push_str(&stringify(raw));
    } else {
        buf.push_str(raw);
    }
}

enum Outcome {
    Changed,
    NoChange,
    Invalid(String),
}

fn append_raw_paths(
    buf: &mut String,
    json: &str,
    paths: &[&str],
    raw: &str,
    stringify_value: bool,
    del: bool,
) -> Outcome {
    // Deleting "-1" removes the last array element (sjson resolves it to length-1).
    let last;
    let lookup = if del && paths[0] == "-1" {
        let root = gjson::parse(json);
        let count = if root.kind() == Kind::Array {
            root.array().len()
        } else {
            0
        };
        if count > 0 {
            last = (count - 1).to_string();
            last.as_str()
        } else {
            paths[0]
        }
    } else {
        paths[0]
    };
    if let Some((index, len)) = locate(json, lookup) {
        let end = index + len;
        buf.push_str(&json[..index]);
        if paths.len() > 1 {
            let outcome = append_raw_paths(buf, &json[index..end], &paths[1..], raw, stringify_value, del);
            if !matches!(outcome, Outcome::Changed) {
                return outcome;
            }
            buf.push_str(&json[end..]);
            return Outcome::Changed;
        }
        let mut skip = 0;
        if del {
            if delete_tail_item(buf) {
                for (j, b) in json.as_bytes()[end..].iter().enumerate() {
                    if *b <= b' ' {
                        continue;
                    }
                    if *b == b',' {
                        skip = j + 1;
                    }
                    break;
                }
            }
        } else if stringify_value {
            buf.push_str(&stringify(raw));
        } else {
            buf.push_str(raw);
        }
        buf.push_str(&json[end + skip..]);
        return Outcome::Changed;
    }
    if del {
        return Outcome::NoChange;
    }
    let index = numeric(paths[0]);
    // gjson.Parse: the value starts at the first '{' or '['; anything else is replaced.
    let start = json.find(|c: char| c > ' ');
    let mut doc: &str = match start.map(|s| (s, json.as_bytes()[s])) {
        Some((s, b'{' | b'[')) => &json[s..],
        _ => "",
    };
    if doc.is_empty() {
        doc = if index.is_some() { "[]" } else { "{}" };
    }
    let comma = doc[1..]
        .bytes()
        .find(|b| *b > b' ')
        .is_some_and(|b| b != b'}' && b != b']');
    match doc.as_bytes()[0] {
        b'{' => {
            let end = doc.rfind('}').unwrap_or(0);
            buf.push_str(&doc[..end]);
            if comma {
                buf.push(',');
            }
            append_build(buf, false, paths, raw, stringify_value);
            buf.push('}');
            Outcome::Changed
        }
        _ => {
            let Some(n) = index else {
                if paths[0] != "-1" {
                    return Outcome::Invalid(format!("cannot set array element for non-numeric key '{}'", paths[0]));
                }
                let trimmed = doc.trim_matches(|c: char| c <= ' ');
                buf.push_str(trimmed.strip_suffix(']').unwrap_or(trimmed));
                if comma {
                    buf.push(',');
                }
                append_build(buf, true, paths, raw, stringify_value);
                buf.push(']');
                return Outcome::Changed;
            };
            let items: Vec<String> = gjson::parse(doc).array().iter().map(|v| v.json().to_owned()).collect();
            buf.push('[');
            buf.push_str(&items.join(","));
            if items.is_empty() {
                buf.push_str(&"null,".repeat(n));
            } else {
                buf.push_str(&",null".repeat(n.saturating_sub(items.len())));
                if comma {
                    buf.push(',');
                }
            }
            append_build(buf, true, paths, raw, stringify_value);
            buf.push(']');
            Outcome::Changed
        }
    }
}

/// An sjson error, with sjson's message.
pub(crate) type Edit = Result<String, String>;

fn set(json: &str, path: &str, raw: &str, stringify_value: bool, del: bool) -> Edit {
    if path.is_empty() {
        return Err("path cannot be empty".into());
    }
    let paths: Vec<&str> = path.split('.').collect();
    let mut buf = String::with_capacity(json.len() + raw.len() + path.len() + 4);
    match append_raw_paths(&mut buf, json, &paths, raw, stringify_value, del) {
        Outcome::Changed => Ok(buf),
        Outcome::NoChange => Ok(json.to_owned()),
        Outcome::Invalid(message) => Err(message),
    }
}

/// sjson `SetRawBytes`.
pub(crate) fn set_raw(json: &str, path: &str, raw: &str) -> Edit {
    set(json, path, raw, false, false)
}

/// sjson `SetBytes` with a string value.
pub(crate) fn set_str(json: &str, path: &str, value: &str) -> Edit {
    set(json, path, value, true, false)
}

/// sjson `DeleteBytes`; a missing path leaves the body unchanged.
pub(crate) fn delete(json: &str, path: &str) -> String {
    set(json, path, "", false, true).unwrap_or_else(|_| json.to_owned())
}

/// gjson `Result.String()`: integers keep their literal, other numbers use Go's shortest
/// decimal form without an exponent; null is empty.
pub(crate) fn gstr(value: &gjson::Value<'_>) -> String {
    if value.kind() != Kind::Number {
        return value.str().to_owned();
    }
    let raw = value.json();
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        return raw.to_owned();
    }
    format!("{}", value.f64())
}

/// A decoded JSON value in Go's `map[string]any` model (decoder.UseNumber()).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GoValue {
    Null,
    Bool(bool),
    /// The literal number text (json.Number).
    Number(String),
    String(String),
    Array(Vec<GoValue>),
    /// Go maps marshal with sorted keys; duplicate keys keep the last value.
    Object(BTreeMap<String, GoValue>),
}

impl GoValue {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        if !valid(text) {
            return None;
        }
        Some(Self::from_gjson(&gjson::parse(text)))
    }

    fn from_gjson(value: &gjson::Value<'_>) -> Self {
        match value.kind() {
            Kind::Null => Self::Null,
            Kind::True => Self::Bool(true),
            Kind::False => Self::Bool(false),
            Kind::Number => Self::Number(value.json().to_owned()),
            Kind::String => Self::String(value.str().to_owned()),
            Kind::Array => Self::Array(value.array().iter().map(Self::from_gjson).collect()),
            Kind::Object => {
                let mut map = BTreeMap::new();
                value.each(|key, item| {
                    map.insert(key.str().to_owned(), Self::from_gjson(&item));
                    true
                });
                Self::Object(map)
            }
        }
    }

    /// Go `json.Marshal`.
    pub(crate) fn marshal(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    /// Go `json.Encoder` with `SetIndent("", "  ")`, including the trailing newline.
    pub(crate) fn encode_indented(&self) -> String {
        let mut out = String::new();
        self.write_indented(&mut out, 0);
        out.push('\n');
        out
    }

    fn write_indented(&self, out: &mut String, depth: usize) {
        let pad = |out: &mut String, depth: usize| {
            out.push('\n');
            out.push_str(&"  ".repeat(depth));
        };
        match self {
            Self::Array(items) if !items.is_empty() => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    pad(out, depth + 1);
                    item.write_indented(out, depth + 1);
                }
                pad(out, depth);
                out.push(']');
            }
            Self::Object(map) if !map.is_empty() => {
                out.push('{');
                for (i, (key, item)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    pad(out, depth + 1);
                    out.push_str(&go_quote(key));
                    out.push_str(": ");
                    item.write_indented(out, depth + 1);
                }
                pad(out, depth);
                out.push('}');
            }
            _ => self.write(out),
        }
    }

    /// Converts serde JSON metadata into Go's model (numbers keep their literal text).
    pub(crate) fn from_json(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(b) => Self::Bool(*b),
            serde_json::Value::Number(n) => Self::Number(n.to_string()),
            serde_json::Value::String(s) => Self::String(s.clone()),
            serde_json::Value::Array(items) => Self::Array(items.iter().map(Self::from_json).collect()),
            serde_json::Value::Object(map) => {
                Self::Object(map.iter().map(|(k, v)| (k.clone(), Self::from_json(v))).collect())
            }
        }
    }

    fn write(&self, out: &mut String) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Self::Number(n) => out.push_str(n),
            Self::String(s) => out.push_str(&go_quote(s)),
            Self::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Self::Object(map) => {
                out.push('{');
                for (i, (key, item)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&go_quote(key));
                    out.push(':');
                    item.write(out);
                }
                out.push('}');
            }
        }
    }
}

/// Go decode-then-marshal of one JSON document, for semantic equality checks.
pub(crate) fn canonical(text: &str) -> Option<String> {
    GoValue::parse(text.trim()).map(|v| v.marshal())
}

/// Joins raw JSON values into an array literal (helps.JoinRawJSONStrings).
pub(crate) fn join_array(items: &[String]) -> String {
    format!("[{}]", items.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_appends_and_replaces_like_sjson() {
        assert_eq!(set_str(r#"{"a":1}"#, "model", "k3").unwrap(), r#"{"a":1,"model":"k3"}"#);
        assert_eq!(
            set_str(r#"{"model":"x","a":1}"#, "model", "k3").unwrap(),
            r#"{"model":"k3","a":1}"#
        );
        assert_eq!(
            set_str("{}", "thinking.type", "enabled").unwrap(),
            r#"{"thinking":{"type":"enabled"}}"#
        );
        assert_eq!(set_str(r#"{ "a": 1 }"#, "b", "x").unwrap(), r#"{ "a": 1 ,"b":"x"}"#);
        assert_eq!(
            set_raw(r#"{"m":[{"a":1},{"b":2}]}"#, "m.1.c", "true").unwrap(),
            r#"{"m":[{"a":1},{"b":2,"c":true}]}"#
        );
        assert_eq!(set_raw("  ", "a.0", "1").unwrap(), r#"{"a":[1]}"#);
        assert_eq!(
            set_raw(r#"{"stream_options":[]}"#, "stream_options.include_usage", "true"),
            Err("cannot set array element for non-numeric key 'include_usage'".to_owned())
        );
        // Non-ASCII and quotes go through encoding/json, which escapes HTML too.
        assert_eq!(stringify("a<b"), "\"a<b\"");
        assert_eq!(stringify("é<\""), "\"é\\u003c\\\"\"");
    }

    #[test]
    fn delete_takes_one_neighbouring_comma() {
        let body = r#"{"a": 1, "b": 2, "c": 3}"#;
        assert_eq!(delete(body, "a"), r#"{ "b": 2, "c": 3}"#);
        assert_eq!(delete(body, "b"), r#"{"a": 1, "c": 3}"#);
        assert_eq!(delete(body, "c"), r#"{"a": 1, "b": 2}"#);
        assert_eq!(delete(r#"{"only":true}"#, "only"), "{}");
        assert_eq!(delete(body, "missing"), body);
        assert_eq!(delete("[1,2]", "-1"), "[1]");
        // Go's backward scan skips a byte after an escaped quote and leaves malformed JSON
        // for a key that itself contains one; parity keeps sjson's exact output.
        assert_eq!(delete(r#"{"\"x":1,"y":2}"#, "\\\"x"), r#"{"\"x":,"y":2}"#);
        assert_eq!(
            delete(r#"{"t":{"type":"x","effort":"y"}}"#, "t.effort"),
            r#"{"t":{"type":"x"}}"#
        );
    }

    #[test]
    fn go_marshal_sorts_keys_and_keeps_number_literals() {
        let v = GoValue::parse(r#"{"z":1.50,"a":{"y":"<&>","b":[true,null]},"a":2}"#).unwrap();
        assert_eq!(v.marshal(), r#"{"a":2,"z":1.50}"#);
        assert_eq!(
            canonical(r#"{"b":"\u2028x","a":1e3}"#).unwrap(),
            r#"{"a":1e3,"b":"\u2028x"}"#
        );
        assert_eq!(go_quote("<&>\u{1}\u{8}"), r#""\u003c\u0026\u003e\u0001\b""#);
    }
}
