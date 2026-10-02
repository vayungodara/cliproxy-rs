//! Byte-compatible `encoding/json.Marshal` of values decoded into Go `any`: object keys
//! sorted, `<`, `>`, `&`, U+2028 and U+2029 escaped, numbers as float64.
//!
//! ponytail: private stand-in until `cpa_common::json` (translator thread, the shared
//! Go-exact gjson/sjson/encoding-json module) reaches this branch; then session.rs
//! switches to it and this file goes.

use serde_json::Value;

/// A Go JSON string literal (Go 1.22+ escapes `\b` and `\f` by name).
pub(crate) fn string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Go's float64 encoding: shortest round-trip digits, exponent form outside
/// [1e-6, 1e21) with a two-digit minimum exponent cleaned to Go's `e-7` / `e+21`.
pub(crate) fn float(f: f64) -> String {
    let abs = f.abs();
    if abs != 0.0 && !(1e-6..1e21).contains(&abs) {
        let s = format!("{f:e}");
        match s.split_once('e') {
            Some((mantissa, exp)) if exp.starts_with('-') => format!("{mantissa}e{exp}"),
            Some((mantissa, exp)) => format!("{mantissa}e+{exp}"),
            None => s,
        }
    } else {
        format!("{f}")
    }
}

/// `json.Marshal(v)` for `v` produced by `json.Unmarshal` into `any`.
pub(crate) fn marshal(v: &Value) -> String {
    let mut out = String::new();
    write(v, &mut out);
    out
}

fn write(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&float(n.as_f64().unwrap_or_default())),
        Value::String(s) => string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                string(key, out);
                out.push(':');
                write(&map[key], out);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_follow_go_encoding() {
        for (f, go) in [
            (1.0, "1"),
            (0.5, "0.5"),
            (1e21, "1e+21"),
            (1e20, "100000000000000000000"),
            (1e-7, "1e-7"),
            (0.000001, "0.000001"),
            (-2.5e-8, "-2.5e-8"),
            (12345678901234567890.0, "12345678901234567000"),
        ] {
            assert_eq!(float(f), go, "{f}");
        }
    }

    #[test]
    fn marshal_sorts_keys_and_escapes_like_go() {
        let v: Value = serde_json::from_str(r#"{"b":1,"a":["<&>",true,null,2.50],"\u0008":"\f"}"#).unwrap();
        assert_eq!(
            marshal(&v),
            r#"{"\b":"\f","a":["\u003c\u0026\u003e",true,null,2.5],"b":1}"#
        );
    }
}
