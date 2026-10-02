//! Byte-preserving JSON edits for the Claude executor: tidwall/sjson v1.2.5 and Go
//! encoding/json through the shared `cpa_common::json` port, behind `&str` signatures.
//!
//! Claude request rewriting must reproduce Go's bytes exactly: CCH signs the final
//! body, and prompt caching keys on it. Lookups use the gjson crate (same path syntax as
//! Go's gjson for the simple dotted paths these rules use).

use cpa_common::json;
use gjson::Value;

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
    let mut out = Vec::with_capacity(s.len() + 2);
    json::marshal_str(&mut out, s.as_bytes(), html);
    text(out)
}

/// The edits below never break UTF-8 in a UTF-8 document; the fallback is unreachable.
fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// `sjson.SetRawBytes`; an sjson error leaves the document unchanged.
pub(crate) fn set_raw(json: &str, path: &str, raw: &str) -> String {
    json::try_set_raw(json.as_bytes(), path, raw).map_or_else(|_| json.to_owned(), text)
}

/// `sjson.SetBytes` with a Go string value.
pub(crate) fn set_str(json: &str, path: &str, value: &str) -> String {
    json::try_set_str(json.as_bytes(), path, value).map_or_else(|_| json.to_owned(), text)
}

/// `sjson.DeleteBytes`.
pub(crate) fn delete(json: &str, path: &str) -> String {
    json::try_delete(json.as_bytes(), path).map_or_else(|_| json.to_owned(), text)
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
