//! Go `encoding/json` (v1, the Go 1.26 default) decoding into Go's config structs,
//! driven by the generated shape table. Field names match case-folded, unknown members
//! are ignored, null leaves a non-nilable value as it was, a repeated member decodes
//! into the earlier value (struct fields and slice elements merge, map elements start
//! fresh), and any type mismatch fails the whole decode, as Go's `Unmarshal` returns
//! the error.
use std::fmt::Write;

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

use super::view::{self, Shape};

/// A JSON value with object members in order and duplicates kept.
#[derive(Clone, Debug)]
pub(super) enum Raw {
    Null,
    Bool(bool),
    Num(Number),
    Str(String),
    Arr(Vec<Raw>),
    Obj(Vec<(String, Raw)>),
}

impl<'de> Deserialize<'de> for Raw {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visit;
        impl<'de> Visitor<'de> for Visit {
            type Value = Raw;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON value")
            }
            fn visit_unit<E>(self) -> Result<Raw, E> {
                Ok(Raw::Null)
            }
            fn visit_bool<E>(self, b: bool) -> Result<Raw, E> {
                Ok(Raw::Bool(b))
            }
            fn visit_i64<E>(self, n: i64) -> Result<Raw, E> {
                Ok(Raw::Num(n.into()))
            }
            fn visit_u64<E>(self, n: u64) -> Result<Raw, E> {
                Ok(Raw::Num(n.into()))
            }
            fn visit_f64<E>(self, n: f64) -> Result<Raw, E> {
                Ok(Number::from_f64(n).map_or(Raw::Null, Raw::Num))
            }
            fn visit_str<E>(self, s: &str) -> Result<Raw, E> {
                Ok(Raw::Str(s.to_owned()))
            }
            fn visit_string<E>(self, s: String) -> Result<Raw, E> {
                Ok(Raw::Str(s))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Raw, A::Error> {
                let mut out = Vec::new();
                while let Some(v) = seq.next_element()? {
                    out.push(v);
                }
                Ok(Raw::Arr(out))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Raw, A::Error> {
                let mut out = Vec::new();
                while let Some(entry) = map.next_entry::<String, Raw>()? {
                    out.push(entry);
                }
                Ok(Raw::Obj(out))
            }
        }
        d.deserialize_any(Visit)
    }
}

impl Raw {
    /// A whole body as Go `json.Unmarshal` reads it: one value, then whitespace only.
    pub(super) fn parse(body: &[u8]) -> Option<Raw> {
        serde_json::from_slice(body).ok()
    }

    /// The first value of a body, as gin `ShouldBindJSON` (a `json.Decoder`) reads it;
    /// whatever follows is ignored.
    pub(super) fn first(body: &[u8]) -> Option<Raw> {
        serde_json::Deserializer::from_slice(body)
            .into_iter::<Raw>()
            .next()?
            .ok()
    }

    /// JSON text: the bytes a Go `json.RawMessage` field holds, whitespace aside.
    pub(super) fn text(&self) -> String {
        fn put(r: &Raw, out: &mut String) {
            match r {
                Raw::Null => out.push_str("null"),
                Raw::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
                Raw::Num(n) => {
                    let _ = write!(out, "{n}");
                }
                Raw::Str(s) => out.push_str(&Value::from(s.as_str()).to_string()),
                Raw::Arr(items) => {
                    out.push('[');
                    for (i, item) in items.iter().enumerate() {
                        if i > 0 {
                            out.push(',');
                        }
                        put(item, out);
                    }
                    out.push(']');
                }
                Raw::Obj(members) => {
                    out.push('{');
                    for (i, (k, v)) in members.iter().enumerate() {
                        if i > 0 {
                            out.push(',');
                        }
                        out.push_str(&Value::from(k.as_str()).to_string());
                        out.push(':');
                        put(v, out);
                    }
                    out.push('}');
                }
            }
        }
        let mut out = String::new();
        put(self, &mut out);
        out
    }

    /// Go's `any` view (a later duplicate member wins).
    fn value(&self) -> Value {
        match self {
            Raw::Null => Value::Null,
            Raw::Bool(b) => Value::Bool(*b),
            Raw::Num(n) => Value::Number(n.clone()),
            Raw::Str(s) => Value::from(s.as_str()),
            Raw::Arr(items) => Value::Array(items.iter().map(Raw::value).collect()),
            Raw::Obj(members) => Value::Object(members.iter().map(|(k, v)| (k.clone(), v.value())).collect()),
        }
    }
}

/// Go `foldName`: ASCII upper-cased, other runes `ToUpper(ToLower(r))`.
fn fold(name: &str) -> String {
    use cpa_common::gostr::{to_lower_rune, to_upper_rune};
    name.chars()
        .map(|c| {
            if c.is_ascii() {
                c.to_ascii_uppercase()
            } else {
                to_upper_rune(to_lower_rune(c))
            }
        })
        .collect()
}

/// `raw` decoded into a value of `shape`, starting from `into` (Go decodes into the
/// existing value). Kind `raw` is a `json.RawMessage`: its JSON text as a string.
/// `None` is a Go decode error.
pub(super) fn decode(shape: &Shape, raw: &Raw, into: Value) -> Option<Value> {
    if shape.kind == "raw" {
        return Some(Value::String(raw.text()));
    }
    if let Raw::Null = raw {
        return Some(if view::nilable(shape) { Value::Null } else { into });
    }
    let into = if shape.ptr && into.is_null() {
        view::pointee_zero(shape)
    } else {
        into
    };
    match (shape.kind.as_str(), raw) {
        ("bool", Raw::Bool(b)) => Some(Value::Bool(*b)),
        // strconv.ParseInt on the literal: fractions, exponents and overflow fail.
        ("int", Raw::Num(n)) => n.as_i64().map(Value::from),
        ("string", Raw::Str(s)) => Some(Value::from(s.as_str())),
        ("any", r) => Some(r.value()),
        ("slice", Raw::Arr(items)) => {
            let elem = shape.elem.as_deref()?;
            let old = match into {
                Value::Array(a) => a,
                _ => Vec::new(),
            };
            // ponytail: Go reuses the backing array, so a third repeat can resurrect
            // elements a shorter second repeat truncated; elements past the earlier
            // length start from zero here.
            items
                .iter()
                .enumerate()
                .map(|(i, item)| decode(elem, item, old.get(i).cloned().unwrap_or_else(|| view::zero(elem))))
                .collect::<Option<Vec<_>>>()
                .map(Value::Array)
        }
        ("map", Raw::Obj(members)) => {
            let elem = shape.elem.as_deref()?;
            let mut out = match into {
                Value::Object(m) => m,
                _ => Map::new(),
            };
            for (k, v) in members {
                out.insert(k.clone(), decode(elem, v, view::zero(elem))?);
            }
            Some(Value::Object(out))
        }
        ("struct", Raw::Obj(members)) => {
            let mut out = match into {
                Value::Object(m) => m,
                _ => Map::new(),
            };
            for (k, v) in members {
                let key = fold(k);
                let Some(f) = shape.fields.iter().find(|f| fold(&f.json) == key) else {
                    continue;
                };
                let old = out.get(&f.json).cloned().unwrap_or_else(|| view::zero(f));
                out.insert(f.json.clone(), decode(f, v, old)?);
            }
            Some(Value::Object(out))
        }
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

    fn shape() -> Shape {
        record(vec![
            field("api-key", "string", false, None),
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
        decode(&shape(), &Raw::parse(body.as_bytes())?, view::zero(&shape()))
    }

    // Expected values: Go 1.26 json.Unmarshal into the equivalent struct.
    #[test]
    fn decodes_like_go_unmarshal() {
        let v = go(r#"{"API-KEY":"k","Weight":3,"raw":{"a" : [1, null]},"tags":["a",null],"x":1}"#).unwrap();
        assert_eq!(v["api-key"], "k");
        assert_eq!(v["weight"], 3);
        assert_eq!(v["raw"], r#"{"a":[1,null]}"#);
        assert_eq!(v["tags"], serde_json::json!(["a", ""]));
        assert_eq!(go(r#"{"api-key":"k","api-key":null}"#).unwrap()["api-key"], "k");
        assert_eq!(go(r#"{"weight":1,"weight":null}"#).unwrap()["weight"], Value::Null);
        // Repeats merge slice elements and maps; map elements start fresh.
        let v = go(r#"{"models":[{"name":"a","alias":"b"}],"models":[{"name":"c"}]}"#).unwrap();
        assert_eq!(v["models"], serde_json::json!([{"name": "c", "alias": "b"}]));
        let v = go(r#"{"headers":{"A":"1"},"headers":{"B":"2","A":null}}"#).unwrap();
        assert_eq!(v["headers"], serde_json::json!({"A": "", "B": "2"}));
        for bad in [
            r#"{"weight":1.0}"#,
            r#"{"weight":"1"}"#,
            r#"{"weight":9223372036854775808}"#,
            r#"{"api-key":1}"#,
            r#"{"tags":"a"}"#,
            r#"{"models":{}}"#,
            r#"[1]"#,
            r#"{"a":1} x"#,
        ] {
            assert_eq!(go(bad), None, "{bad}");
        }
        assert_eq!(go("null"), Some(view::zero(&shape())));
    }

    #[test]
    fn folds_names_like_go() {
        assert_eq!(fold("api-key"), "API-KEY");
        // U+017F LATIN SMALL LETTER LONG S folds to S, U+212A KELVIN SIGN to K.
        assert_eq!(fold("\u{17f}\u{212a}"), "SK");
    }
}
