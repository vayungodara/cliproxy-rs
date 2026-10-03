//! Go `encoding/json` (v1, the Go 1.26 default) decoding into Go's config structs,
//! driven by the generated shape table and working on the validated bytes:
//! - field names match exactly, else under `strings.EqualFold`; unknown members are
//!   skipped without converting their values;
//! - null leaves a non-nilable value as it was;
//! - a repeated member decodes into the earlier value (struct fields and slice elements
//!   merge, map elements start fresh);
//! - numbers keep their token (`strconv.ParseInt` decides an int, so `-0` passes and
//!   `1.0` fails); strings decode as Go does (invalid UTF-8 and lone surrogates become
//!   U+FFFD); a `json.RawMessage` keeps its bytes;
//! - any type mismatch fails the whole decode, as Go's `Unmarshal` returns the error.
use cpa_common::gostr::GoStr;
use cpa_common::json::{go_unquote, std_valid};
use serde_json::{Map, Value};

use super::view::{self, Shape};

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

fn skip_space(b: &[u8], mut i: usize) -> usize {
    while b.get(i).copied().is_some_and(is_space) {
        i += 1;
    }
    i
}

/// End (exclusive) of the value starting at `b[i]` in valid JSON. Iterative, so the
/// input's nesting never reaches the Rust stack.
fn value_end(b: &[u8], i: usize) -> usize {
    let string_end = |mut j: usize| {
        while j < b.len() {
            match b[j] {
                b'\\' => j += 2,
                b'"' => return j + 1,
                _ => j += 1,
            }
        }
        b.len()
    };
    match b.get(i) {
        Some(b'"') => string_end(i + 1),
        Some(b'{' | b'[') => {
            let (mut j, mut depth) = (i, 0usize);
            while j < b.len() {
                match b[j] {
                    b'"' => {
                        j = string_end(j + 1);
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return j + 1;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            b.len()
        }
        _ => {
            let mut j = i;
            while j < b.len() && !matches!(b[j], b',' | b':' | b']' | b'}') && !is_space(b[j]) {
                j += 1;
            }
            j
        }
    }
}

/// The elements of a valid array, or the members (key token, value) of a valid object.
fn children(b: &[u8]) -> Vec<(Option<&[u8]>, &[u8])> {
    let mut out = Vec::new();
    let object = b.first() == Some(&b'{');
    let mut i = skip_space(b, 1);
    while i < b.len() && !matches!(b[i], b']' | b'}') {
        let key = if object {
            let end = value_end(b, i);
            let key = &b[i..end];
            i = skip_space(b, skip_space(b, end) + 1);
            Some(key)
        } else {
            None
        };
        let end = value_end(b, i);
        out.push((key, &b[i..end]));
        i = skip_space(b, end);
        if b.get(i) == Some(&b',') {
            i = skip_space(b, i + 1);
        }
    }
    out
}

/// A whole body as Go `json.Unmarshal` reads it: one valid value, whitespace around.
pub(super) fn whole(body: &[u8]) -> Option<&[u8]> {
    if !std_valid(body) {
        return None;
    }
    let start = skip_space(body, 0);
    let end = body.len() - body.iter().rev().take_while(|c| is_space(**c)).count();
    Some(&body[start..end])
}

/// The first value of a body, as gin `ShouldBindJSON` (a `json.Decoder`) reads it; what
/// follows a closing bracket is not read. Every v0 request body is a struct, so a
/// top-level scalar fails the bind either way.
pub(super) fn first(body: &[u8]) -> Option<&[u8]> {
    let start = skip_space(body, 0);
    if start == body.len() {
        return None;
    }
    let value = &body[start..value_end(body, start)];
    std_valid(value).then_some(value)
}

/// A `strconv.ParseInt` integer token (what Go accepts for an `int` field).
pub(super) fn int_token(raw: &[u8]) -> Option<i64> {
    match raw.first() {
        Some(b'-' | b'0'..=b'9') => std::str::from_utf8(raw).ok()?.parse().ok(),
        _ => None,
    }
}

/// `raw` (one valid JSON value) decoded into a value of `shape`, starting from `into`
/// (Go decodes into the existing value). Kind `raw` is a `json.RawMessage`: its bytes
/// as a string. `None` is a Go decode error.
pub(super) fn decode(shape: &Shape, raw: &[u8], into: Value) -> Option<Value> {
    if shape.kind == "raw" {
        return Some(Value::String(String::from_utf8_lossy(raw).into_owned()));
    }
    if raw == b"null" {
        return Some(if view::nilable(shape) { Value::Null } else { into });
    }
    let into = if shape.ptr && into.is_null() {
        view::pointee_zero(shape)
    } else {
        into
    };
    let first = *raw.first()?;
    match (shape.kind.as_str(), first) {
        ("bool", b't' | b'f') => Some(Value::Bool(raw == b"true")),
        ("int", _) => int_token(raw).map(Value::from),
        ("string", b'"') => go_unquote(raw).map(Value::String),
        // ponytail: generic values go through serde (recursion limit 128, where Go
        // allows 10000); no v0 write shape holds one.
        ("any", _) => serde_json::from_slice(raw).ok(),
        ("slice", b'[') => {
            let elem = shape.elem.as_deref()?;
            let old = match into {
                Value::Array(a) => a,
                _ => Vec::new(),
            };
            // ponytail: Go reuses the backing array, so a third repeat of a member can
            // resurrect elements a shorter second repeat truncated; here elements past
            // the earlier length start from zero.
            children(raw)
                .into_iter()
                .enumerate()
                .map(|(i, (_, item))| decode(elem, item, old.get(i).cloned().unwrap_or_else(|| view::zero(elem))))
                .collect::<Option<Vec<_>>>()
                .map(Value::Array)
        }
        ("map", b'{') => {
            let elem = shape.elem.as_deref()?;
            let mut out = match into {
                Value::Object(m) => m,
                _ => Map::new(),
            };
            for (key, value) in children(raw) {
                let key = go_unquote(key?)?;
                out.insert(key, decode(elem, value, view::zero(elem))?);
            }
            Some(Value::Object(out))
        }
        ("struct", b'{') => {
            let mut out = match into {
                Value::Object(m) => m,
                _ => Map::new(),
            };
            for (key, value) in children(raw) {
                let key = go_unquote(key?)?;
                let field = shape
                    .fields
                    .iter()
                    .find(|f| f.json == key)
                    .or_else(|| shape.fields.iter().find(|f| f.json.go_eq_fold(&key)));
                let Some(f) = field else { continue };
                let old = out.get(&f.json).cloned().unwrap_or_else(|| view::zero(f));
                out.insert(f.json.clone(), decode(f, value, old)?);
            }
            Some(Value::Object(out))
        }
        _ => None,
    }
}

/// Go's PUT bodies: `json.Unmarshal` into the collection, else into
/// `struct { Items T }`; `need_items` also requires a non-empty `items`.
pub(super) fn put_collection(shape: &Shape, body: &[u8], need_items: bool) -> Option<Value> {
    let raw = whole(body)?;
    if let Some(v) = decode(shape, raw, Value::Null) {
        return Some(v);
    }
    let wrapper = record(vec![Shape {
        json: "items".into(),
        ..shape.clone()
    }]);
    let mut obj = decode(&wrapper, raw, view::zero(&wrapper))?;
    let items = obj.get_mut("items").map(Value::take).unwrap_or_default();
    if need_items && items.as_array().is_none_or(Vec::is_empty) {
        return None;
    }
    Some(items)
}

/// gin `ShouldBindJSON` into a request struct.
pub(super) fn bind(shape: &Shape, body: &[u8]) -> Option<Map<String, Value>> {
    match decode(shape, first(body)?, view::zero(shape))? {
        Value::Object(o) => Some(o),
        _ => None,
    }
}

/// A Go struct shape built from named fields (patch and request bodies).
pub(super) fn record(fields: Vec<Shape>) -> Shape {
    Shape {
        json: String::new(),
        yaml: String::new(),
        omit: false,
        kind: "struct".into(),
        ptr: false,
        fields,
        elem: None,
    }
}

/// One field of a request body: `kind` with `elem`, behind a pointer when `ptr`.
pub(super) fn field(json: &str, kind: &str, ptr: bool, elem: Option<Shape>) -> Shape {
    Shape {
        json: json.into(),
        yaml: json.into(),
        omit: false,
        kind: kind.into(),
        ptr,
        fields: Vec::new(),
        elem: elem.map(Box::new),
    }
}

/// `shape` renamed to `json` and put behind a pointer (Go `*T` patch fields).
pub(super) fn pointer_to(json: &str, shape: &Shape) -> Shape {
    Shape {
        json: json.into(),
        ptr: true,
        ..shape.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn shape() -> Shape {
        record(vec![
            field("api-key", "string", false, None),
            field("priority", "int", false, None),
            field("weight", "int", true, None),
            field("raw", "raw", false, None),
            field("tags", "slice", false, Some(field("", "string", false, None))),
            field(
                "models",
                "slice",
                false,
                Some(record(vec![
                    field("name", "string", false, None),
                    field("alias", "string", false, None),
                ])),
            ),
            field("headers", "map", false, Some(field("", "string", false, None))),
        ])
    }

    fn go(body: &str) -> Option<Value> {
        decode(&shape(), whole(body.as_bytes())?, view::zero(&shape()))
    }

    // Expected values: Go 1.26 json.Unmarshal into the equivalent struct.
    #[test]
    fn decodes_like_go_unmarshal() {
        let v = go(r#"{"API-KEY":"k","Weight":-0,"raw":{"a" : [1, null]},"tags":["a",null],"x":1e400}"#).unwrap();
        assert_eq!(v["api-key"], "k");
        assert_eq!(v["weight"], 0);
        assert_eq!(v["raw"], r#"{"a" : [1, null]}"#);
        assert_eq!(v["tags"], json!(["a", ""]));
        assert_eq!(
            go(r#"{"api-key":"k\ud800\u00e9"}"#).unwrap()["api-key"],
            "k\u{fffd}\u{e9}"
        );
        assert_eq!(go(r#"{"api-key":"k","api-key":null}"#).unwrap()["api-key"], "k");
        assert_eq!(go(r#"{"weight":1,"weight":null}"#).unwrap()["weight"], Value::Null);
        // EqualFold: U+017F folds with s and S; dotless U+0131 does not fold with i.
        assert_eq!(go(r#"{"tag\u017f":["long-s"]}"#).unwrap()["tags"], json!(["long-s"]));
        assert_eq!(go(r#"{"pr\u0131ority":7}"#).unwrap()["priority"], 0);
        // Repeats merge slice elements and maps; map elements start fresh.
        let v = go(r#"{"models":[{"name":"a","alias":"b"}],"models":[{"name":"c"}]}"#).unwrap();
        assert_eq!(v["models"], json!([{"name": "c", "alias": "b"}]));
        let v = go(r#"{"tags":["a","b"],"tags":[null]}"#).unwrap();
        assert_eq!(v["tags"], json!(["a"]));
        let v = go(r#"{"headers":{"A":"1"},"headers":{"B":"2","A":null}}"#).unwrap();
        assert_eq!(v["headers"], json!({"A": "", "B": "2"}));
        for bad in [
            r#"{"weight":1.0}"#,
            r#"{"weight":1e2}"#,
            r#"{"weight":"1"}"#,
            r#"{"weight":9223372036854775808}"#,
            r#"{"api-key":1}"#,
            r#"{"tags":"a"}"#,
            r#"{"models":{}}"#,
            r#"[1]"#,
            r#"{"a":1} x"#,
            r#"{"a":01}"#,
            "",
        ] {
            assert_eq!(go(bad), None, "{bad}");
        }
        assert_eq!(go("null"), Some(view::zero(&shape())));
        // Go's scanner allows 10000 levels in a skipped member.
        let deep = format!(r#"{{"x":{}{},"api-key":"k"}}"#, "[".repeat(200), "]".repeat(200));
        assert_eq!(go(&deep).unwrap()["api-key"], "k");
    }

    #[test]
    fn first_value_is_what_a_go_decoder_reads() {
        assert_eq!(first(br#"  {"a":[1,"]"]} trailing"#), Some(&br#"{"a":[1,"]"]}"#[..]));
        assert_eq!(first(b"  "), None);
        assert_eq!(first(br#"{"a":}"#), None);
    }
}
