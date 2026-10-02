//! Raw JSON edits with tidwall/sjson semantics, so request rewrites touch only the
//! bytes Go touches (sjson@v1.2.5 `appendRawPaths`, `deleteTailItem`). Untouched members
//! keep their order, spacing and number spelling.
//!
//! Only simple dot paths are supported (`a.b`, `tools.-1`, escaped `\.`); every path
//! the Codex executor and the Responses WebSocket handler edit is simple. Reads go through
//! the `gjson` crate. Owner: the Codex thread.

use gjson::Kind;

struct Part {
    /// Unescaped key, as written into new JSON.
    key: String,
    /// The same segment in gjson path syntax.
    gpath: String,
    /// `:` prefix: always an object key, never an array index.
    force: bool,
}

fn parse_path(path: &str) -> Option<Vec<Part>> {
    let mut parts = Vec::new();
    let mut rest = path;
    loop {
        let mut force = false;
        if let Some(stripped) = rest.strip_prefix(':') {
            force = true;
            rest = stripped;
        }
        let (mut key, mut gpath) = (String::new(), String::new());
        let mut chars = rest.char_indices();
        let mut next = None;
        while let Some((i, c)) = chars.next() {
            match c {
                '|' | '#' | '@' | '*' | '?' => return None,
                '.' => {
                    next = Some(&rest[i + 1..]);
                    break;
                }
                '\\' => {
                    gpath.push('\\');
                    if let Some((_, escaped)) = chars.next() {
                        key.push(escaped);
                        gpath.push(escaped);
                    }
                }
                _ => {
                    key.push(c);
                    gpath.push(c);
                }
            }
        }
        parts.push(Part { key, gpath, force });
        match next {
            Some(more) => rest = more,
            None => return Some(parts),
        }
    }
}

/// sjson `atoui`: an empty key counts as index 0, as in Go.
fn atoui(part: &Part) -> Option<usize> {
    if part.force {
        return None;
    }
    let mut n = 0usize;
    for b in part.key.bytes() {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.saturating_mul(10).saturating_add(usize::from(b - b'0'));
    }
    Some(n)
}

/// sjson `deleteTailItem`: drops the key (and its leading comma) that precedes a value.
/// Returns whether the following comma must be removed instead.
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
                // Walk back over the key string to the ',' or '{' before it.
                let mut j = i;
                while j > 0 {
                    j -= 1;
                    if b[j] != b'"' {
                        continue;
                    }
                    while j > 0 {
                        j -= 1;
                        if b[j] != b'"' {
                            continue;
                        }
                        if j > 0 && b[j - 1] == b'\\' {
                            j -= 1;
                            continue;
                        }
                        while j > 0 {
                            j -= 1;
                            match b[j] {
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

/// Span of the value at `part` inside `json`, when it borrows the input.
// ponytail: sjson's delete of "-1" (last array element) is not ported; no Codex rule uses it.
fn locate(json: &str, part: &Part) -> Option<(usize, usize)> {
    let value = gjson::get(json, &part.gpath);
    let raw = value.json();
    let base = json.as_ptr() as usize;
    let at = raw.as_ptr() as usize;
    (value.exists() && at > base && at + raw.len() <= base + json.len()).then(|| (at - base, at - base + raw.len()))
}

/// Sentinel for sjson's `errNoChange`.
struct NoChange;

fn append_raw_paths(buf: &mut String, json: &str, parts: &[Part], raw: &str, del: bool) -> Result<(), NoChange> {
    if let Some((start, end)) = locate(json, &parts[0]) {
        buf.push_str(&json[..start]);
        if parts.len() > 1 {
            append_raw_paths(buf, &json[start..end], &parts[1..], raw, del)?;
            buf.push_str(&json[end..]);
            return Ok(());
        }
        let mut skip = 0;
        if !del {
            buf.push_str(raw);
        } else if delete_tail_item(buf) {
            // The key followed '{': drop the comma after the value instead.
            let tail = json.as_bytes()[end..].iter();
            if let Some((j, &b',')) = tail.enumerate().find(|(_, b)| **b > b' ') {
                skip = j + 1;
            }
        }
        buf.push_str(&json[end + skip..]);
        return Ok(());
    }
    if del {
        return Err(NoChange);
    }
    let numeric = atoui(&parts[0]);
    // gjson.Parse: everything from the first '{' or '[' to the end of the input. Empty or
    // scalar input becomes a fresh container, as in sjson.
    let doc = match json.bytes().position(|b| b > b' ') {
        Some(i) if matches!(json.as_bytes()[i], b'{' | b'[') => &json[i..],
        _ if numeric.is_some() => "[]",
        _ => "{}",
    };
    let comma = doc[1..]
        .bytes()
        .find(|b| *b > b' ')
        .is_some_and(|b| b != b'}' && b != b']');
    if doc.starts_with('{') {
        let end = doc.rfind('}').unwrap_or(0);
        buf.push_str(&doc[..end]);
        if comma {
            buf.push(',');
        }
        append_build(buf, false, parts, raw);
        buf.push('}');
        return Ok(());
    }
    // Array parent.
    if numeric.is_none() {
        if parts[0].key == "-1" && !parts[0].force {
            let trimmed = doc.trim_matches(|c: char| c <= ' ');
            buf.push_str(trimmed.strip_suffix(']').unwrap_or(trimmed));
            if comma {
                buf.push(',');
            }
            append_build(buf, true, parts, raw);
            buf.push(']');
            return Ok(());
        }
        return Err(NoChange);
    }
    let n = numeric.unwrap_or(0);
    let parsed = gjson::parse(doc);
    let items = parsed.array();
    buf.push('[');
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
    append_build(buf, true, parts, raw);
    buf.push(']');
    Ok(())
}

fn append_build(buf: &mut String, array: bool, parts: &[Part], raw: &str) {
    if !array {
        buf.push_str(&stringify(&parts[0].key));
        buf.push(':');
    }
    if parts.len() > 1 {
        let next = &parts[1];
        match atoui(next) {
            Some(n) => {
                buf.push('[');
                buf.push_str(&"null,".repeat(n));
                append_build(buf, true, &parts[1..], raw);
                buf.push(']');
            }
            None if !next.force && next.key == "-1" => {
                buf.push('[');
                append_build(buf, true, &parts[1..], raw);
                buf.push(']');
            }
            None => {
                buf.push('{');
                append_build(buf, false, &parts[1..], raw);
                buf.push('}');
            }
        }
    } else {
        buf.push_str(raw);
    }
}

fn edit(json: &str, path: &str, raw: &str, del: bool) -> String {
    let Some(parts) = parse_path(path) else {
        return json.to_owned();
    };
    if parts.is_empty() || path.is_empty() {
        return json.to_owned();
    }
    let mut buf = String::with_capacity(json.len() + raw.len() + path.len() + 4);
    match append_raw_paths(&mut buf, json, &parts, raw, del) {
        Ok(()) => buf,
        Err(NoChange) => json.to_owned(),
    }
}

/// `sjson.SetRawBytes`.
pub fn set_raw(json: &str, path: &str, raw: &str) -> String {
    edit(json, path, raw, false)
}

/// `sjson.SetBytes` with a Go string value.
pub fn set_str(json: &str, path: &str, value: &str) -> String {
    edit(json, path, &stringify(value), false)
}

/// `sjson.DeleteBytes`. A missing path leaves the input unchanged.
pub fn delete(json: &str, path: &str) -> String {
    edit(json, path, "", true)
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

/// sjson `appendStringify`: plain quoting for printable ASCII, `json.Marshal` otherwise.
pub fn stringify(s: &str) -> String {
    if s.bytes()
        .any(|b| !(b' '..=0x7f).contains(&b) || b == b'"' || b == b'\\')
    {
        go_quote(s, true)
    } else {
        format!("\"{s}\"")
    }
}

/// Go `encoding/json` string encoding.
pub fn go_quote(s: &str, escape_html: bool) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    push_escaped(&mut out, s, escape_html);
    out.push('"');
    out
}

fn push_escaped(out: &mut String, s: &str, escape_html: bool) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' | '>' | '&' if escape_html => {
                out.push_str("\\u00");
                out.push(HEX[(c as usize) >> 4] as char);
                out.push(HEX[(c as usize) & 15] as char);
            }
            c if (c as u32) < 0x20 => {
                out.push_str("\\u00");
                out.push(HEX[(c as usize) >> 4] as char);
                out.push(HEX[(c as usize) & 15] as char);
            }
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
}

/// Go `json.Compact` with HTML escaping inside strings, as `json.Marshal` applies to a
/// `json.RawMessage`. Existing escapes are kept as written.
fn compact_html(raw: &str, out: &mut String) {
    let mut in_string = false;
    let mut escaped = false;
    for c in raw.chars() {
        if in_string {
            match c {
                _ if escaped => {
                    escaped = false;
                    out.push(c);
                }
                '\\' => {
                    escaped = true;
                    out.push(c);
                }
                '"' => {
                    in_string = false;
                    out.push(c);
                }
                '<' => out.push_str("\\u003c"),
                '>' => out.push_str("\\u003e"),
                '&' => out.push_str("\\u0026"),
                '\u{2028}' => out.push_str("\\u2028"),
                '\u{2029}' => out.push_str("\\u2029"),
                c => out.push(c),
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if !matches!(c, ' ' | '\t' | '\n' | '\r') {
            out.push(c);
        }
    }
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
        compact_html(value, &mut out);
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
    fn stringify_uses_go_marshal_only_when_needed() {
        assert_eq!(stringify("a<b"), r#""a<b""#, "printable ASCII is never escaped");
        assert_eq!(stringify("a<b\n"), r#""a\u003cb\n""#);
        assert_eq!(stringify("é\u{2028}"), "\"é\\u2028\"");
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
