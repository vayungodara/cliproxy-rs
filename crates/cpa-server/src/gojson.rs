//! Server-side helpers for the bodies CLIProxyAPI builds itself (error envelopes, model
//! lists): Go struct-ordered objects ([`Obj`]), status texts and durations. Go's JSON
//! encoding itself comes from `cpa_common::json`.

use serde_json::Value;

/// A Go JSON string literal (`json.Marshal(string)`).
pub fn string(s: &str) -> String {
    String::from_utf8(cpa_common::json::quote(s)).expect("valid UTF-8 in, valid UTF-8 out")
}

/// sjson's string encoding: plain quoting unless a byte needs escaping, then
/// `json.Marshal` (which also HTML-escapes).
pub fn sjson_string(s: &str) -> String {
    if s.bytes()
        .any(|b| !(b' '..=0x7f).contains(&b) || b == b'"' || b == b'\\')
    {
        string(s)
    } else {
        format!("\"{s}\"")
    }
}

/// `json.Marshal` of a value decoded into `map[string]any`: object keys sorted.
pub fn sorted(v: &Value) -> String {
    String::from_utf8(cpa_common::json::GoValue::from_json(v).marshal()).expect("valid UTF-8 in, valid UTF-8 out")
}

/// An ordered object, for Go structs: fields render in insertion order.
#[derive(Default)]
pub struct Obj(String);

impl Obj {
    pub fn new() -> Self {
        Self::default()
    }

    /// A field whose value is already JSON.
    pub fn raw(mut self, key: &str, json: &str) -> Self {
        self.0.push(if self.0.is_empty() { '{' } else { ',' });
        self.0.push_str(&string(key));
        self.0.push(':');
        self.0.push_str(json);
        self
    }

    pub fn str(self, key: &str, value: &str) -> Self {
        let value = string(value);
        self.raw(key, &value)
    }

    pub fn finish(mut self) -> String {
        if self.0.is_empty() {
            self.0.push('{');
        }
        self.0.push('}');
        self.0
    }
}

/// gjson `Result.String()` for a decoded value: strings verbatim, other scalars as
/// their JSON text, objects and arrays as compact JSON, null or missing as empty.
pub fn gjson_string(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// Go `strings.TrimSpace`.
pub fn trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace())
}

/// Go `http.StatusText`.
pub fn status_text(status: u16) -> &'static str {
    match status {
        499 => "",
        _ => axum::http::StatusCode::from_u16(status)
            .ok()
            .and_then(|s| s.canonical_reason())
            .unwrap_or_default(),
    }
}

/// Go `time.Duration.String()` for non-negative durations.
pub fn duration(d: std::time::Duration) -> String {
    let nanos = d.as_nanos();
    if nanos == 0 {
        return "0s".into();
    }
    if nanos < 1_000_000_000 {
        let (unit, div) = if nanos < 1_000 {
            ("ns", 1u128)
        } else if nanos < 1_000_000 {
            ("µs", 1_000)
        } else {
            ("ms", 1_000_000)
        };
        return format!("{}{unit}", fraction(nanos, div));
    }
    let hours = nanos / 3_600_000_000_000;
    let minutes = nanos / 60_000_000_000 % 60;
    let seconds = nanos % 60_000_000_000;
    let mut out = String::new();
    if hours > 0 {
        out.push_str(&format!("{hours}h"));
    }
    if hours > 0 || minutes > 0 {
        out.push_str(&format!("{minutes}m"));
    }
    out.push_str(&format!("{}s", fraction(seconds, 1_000_000_000)));
    out
}

fn fraction(value: u128, div: u128) -> String {
    let whole = value / div;
    let mut rest = value % div;
    if rest == 0 {
        return whole.to_string();
    }
    let mut digits = String::new();
    let mut scale = div / 10;
    while rest > 0 && scale > 0 {
        digits.push(char::from(b'0' + (rest / scale) as u8));
        rest %= scale;
        scale /= 10;
    }
    format!("{whole}.{digits}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn strings_and_maps_match_go_marshal() {
        // Values checked against Go 1.26 json.Marshal.
        assert_eq!(
            string("a<b>&\"\u{2028}\u{1}"),
            r#""a\u003cb\u003e\u0026\"\u2028\u0001""#
        );
        let v: Value = serde_json::from_str(r#"{"z":1,"a":{"y":[true,null],"b":"x"}}"#).unwrap();
        assert_eq!(sorted(&v), r#"{"a":{"b":"x","y":[true,null]},"z":1}"#);
        assert_eq!(sjson_string("q\"é"), r#""q\"é""#);
        assert_eq!(sjson_string("plain <b>"), r#""plain <b>""#);
        assert_eq!(
            Obj::new().str("message", "m").str("type", "t").finish(),
            r#"{"message":"m","type":"t"}"#
        );
    }

    #[test]
    fn durations_match_go_string() {
        for (d, s) in [
            (Duration::ZERO, "0s"),
            (Duration::from_secs(1), "1s"),
            (Duration::from_secs(60), "1m0s"),
            (Duration::from_secs(3600), "1h0m0s"),
            (Duration::from_secs(5400), "1h30m0s"),
            (Duration::from_millis(1500), "1.5s"),
            (Duration::from_millis(250), "250ms"),
            (Duration::from_micros(3), "3µs"),
            (Duration::from_secs(43200), "12h0m0s"),
        ] {
            assert_eq!(duration(d), s);
        }
    }

    #[test]
    fn gjson_string_matches_result_string() {
        let v: Value = serde_json::from_str(r#"{"a":"x","b":5,"c":true,"d":null,"e":{"k":1}}"#).unwrap();
        let get = |k| gjson_string(v.get(k));
        assert_eq!(
            (get("a"), get("b"), get("c"), get("d")),
            ("x".into(), "5".into(), "true".into(), String::new())
        );
        assert_eq!(get("e"), r#"{"k":1}"#);
        assert_eq!(get("missing"), "");
    }
}
