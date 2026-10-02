//! `String` facade over `cpa_common::json` (Go's tidwall/gjson, tidwall/sjson v1.2.5 and
//! encoding/json, owned by the translator thread) for the Codex modules and the
//! Responses WebSocket handler. Edits touch only the bytes Go touches: untouched members
//! keep their order, spacing and number spelling. An edit sjson rejects leaves the input
//! unchanged, as Go callers that ignore the error do.

use cpa_common::json as gj;
use gjson::Kind;

fn edited(json: &str, edit: impl FnOnce(&mut Vec<u8>) -> bool) -> String {
    let mut out = json.as_bytes().to_vec();
    if !edit(&mut out) {
        return json.to_owned();
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// `sjson.SetRawBytes`.
pub fn set_raw(json: &str, path: &str, raw: &str) -> String {
    edited(json, |out| gj::set_raw(out, path, raw))
}

/// `sjson.SetBytes` with a string value.
pub fn set_str(json: &str, path: &str, value: &str) -> String {
    edited(json, |out| gj::set_str(out, path, value))
}

/// `sjson.DeleteBytes`.
pub fn delete(json: &str, path: &str) -> String {
    edited(json, |out| gj::delete(out, path))
}

/// `helps.SetStringIfDifferent`.
pub fn set_str_if_different(json: String, path: &str, value: &str) -> String {
    let current = gjson::get(&json, path);
    if current.kind() == Kind::String && current.str() == value {
        return json;
    }
    set_str(&json, path, value)
}

/// `helps.SetBoolIfDifferent`.
pub fn set_bool_if_different(json: String, path: &str, value: bool) -> String {
    let kind = gjson::get(&json, path).kind();
    if (value && kind == Kind::True) || (!value && kind == Kind::False) {
        return json;
    }
    set_raw(&json, path, if value { "true" } else { "false" })
}

/// encoding/json string encoding; `escape_html` as `json.Marshal` does.
pub fn go_quote(s: &str, escape_html: bool) -> String {
    let mut out = Vec::with_capacity(s.len() + 2);
    gj::marshal_str(&mut out, s.as_bytes(), escape_html);
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// `json.Unmarshal` into `map[string]json.RawMessage` followed by `json.Marshal`: sorted
/// unescaped keys (last duplicate wins), compacted HTML-escaped values. `None` when Go's
/// unmarshal would fail or yield a nil map.
pub fn go_remarshal_object(
    json: &str,
    edit: impl FnOnce(&mut std::collections::BTreeMap<String, String>) -> bool,
) -> Option<String> {
    if !gjson::valid(json) {
        return None;
    }
    let root = gjson::parse(json);
    if root.kind() != Kind::Object {
        return None;
    }
    let mut members = std::collections::BTreeMap::new();
    root.each(|key, value| {
        members.insert(key.str().to_owned(), value.json().to_owned());
        true
    });
    if !edit(&mut members) {
        return None;
    }
    let mut out = String::with_capacity(json.len());
    out.push('{');
    for (i, (key, value)) in members.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&go_quote(key, true));
        out.push(':');
        out.push_str(&String::from_utf8_lossy(&gj::compact(value.as_bytes(), true)));
    }
    out.push('}');
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_matches_sjson_comma_and_whitespace_rules() {
        // Expected strings follow sjson@v1.2.5 deleteTailItem/appendRawPaths.
        assert_eq!(delete(r#"{"a":1, "b":2}"#, "b"), r#"{"a":1}"#);
        assert_eq!(delete(r#"{"a":1, "b":2}"#, "a"), r#"{ "b":2}"#);
        assert_eq!(delete("{\n  \"a\": 1,\n  \"b\": 2\n}", "b"), "{\n  \"a\": 1\n}");
        assert_eq!(delete(r#"{"only":true}"#, "only"), "{}");
        assert_eq!(delete(r#"{"a":1}"#, "missing"), r#"{"a":1}"#);
        assert_eq!(delete(r#"{"x":{"a":1,"b":2}}"#, "x.a"), r#"{"x":{"b":2}}"#);
        assert_eq!(delete(r#"{"x":{"a":1}}"#, "x.missing"), r#"{"x":{"a":1}}"#);
    }

    #[test]
    fn set_replaces_in_place_or_appends_like_sjson() {
        assert_eq!(set_raw(r#"{"a": 1 , "b":2}"#, "a", "true"), r#"{"a": true , "b":2}"#);
        assert_eq!(set_raw(r#"{"a":1}"#, "b", "2"), r#"{"a":1,"b":2}"#);
        assert_eq!(set_raw("{}", "b", "2"), r#"{"b":2}"#);
        assert_eq!(set_raw("{\"a\":1}\n", "b", "2"), r#"{"a":1,"b":2}"#);
        assert_eq!(set_raw(r#"{"a":1}"#, "x.y", "2"), r#"{"a":1,"x":{"y":2}}"#);
        assert_eq!(set_raw(r#"{"t":[1]}"#, "t.-1", "2"), r#"{"t":[1,2]}"#);
        assert_eq!(set_raw(r#"{"t":[]}"#, "t.-1", "2"), r#"{"t":[2]}"#);
        assert_eq!(set_raw(r#"{"t":[1]}"#, "t.0", "9"), r#"{"t":[9]}"#);
        assert_eq!(set_raw(r#"{"t":[1]}"#, "t.3", "9"), r#"{"t":[1,null,null,9]}"#);
        assert_eq!(set_raw(r#"{"a":1}"#, "p.0.q", "2"), r#"{"a":1,"p":[{"q":2}]}"#);
        assert_eq!(set_raw(r#"{"a.b":1}"#, r"a\.b", "2"), r#"{"a.b":2}"#);
    }

    #[test]
    fn set_str_uses_go_marshal_only_when_needed() {
        assert_eq!(
            set_str("{}", "v", "a<b"),
            r#"{"v":"a<b"}"#,
            "printable ASCII is never escaped"
        );
        assert_eq!(set_str("{}", "v", "a<b\n"), r#"{"v":"a\u003cb\n"}"#);
        assert_eq!(set_str("{}", "v", "é\u{2028}"), "{\"v\":\"é\\u2028\"}");
        assert_eq!(go_quote("\u{1}\u{8}\u{c}", false), r#""\u0001\b\f""#);
    }

    #[test]
    fn remarshal_sorts_compacts_and_escapes() {
        let out = go_remarshal_object(r#"{"z": [1, 2], "a\u0062": "<x>", "z": {"k": 1}}"#, |_| true).unwrap();
        assert_eq!(out, r#"{"ab":"\u003cx\u003e","z":{"k":1}}"#);
        assert!(go_remarshal_object("[1]", |_| true).is_none());
        assert!(go_remarshal_object(r#"{"a":1} x"#, |_| true).is_none());
    }
}
