//! In-place edits of top-level JSON members, like sjson: everything outside the edited
//! member keeps its bytes. Routes use these only where Go rewrites a request body.

/// Byte offsets of a top-level member: `(member_start, value_start, value_end)`.
fn member(body: &[u8], key: &str) -> Option<(usize, usize, usize)> {
    let mut i = skip_ws(body, 0);
    if body.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    loop {
        i = skip_ws(body, i);
        match body.get(i)? {
            b'}' => return None,
            b',' => {
                i += 1;
                continue;
            }
            b'"' => {}
            _ => return None,
        }
        let start = i;
        let key_end = skip_string(body, i)?;
        let name: String = serde_json::from_slice(&body[start..key_end]).ok()?;
        i = skip_ws(body, key_end);
        if body.get(i) != Some(&b':') {
            return None;
        }
        let value_start = skip_ws(body, i + 1);
        let value_end = skip_value(body, value_start)?;
        if name == key {
            return Some((start, value_start, value_end));
        }
        i = value_end;
    }
}

fn skip_ws(body: &[u8], mut i: usize) -> usize {
    while body.get(i).is_some_and(|b| b.is_ascii_whitespace()) {
        i += 1;
    }
    i
}

fn skip_string(body: &[u8], mut i: usize) -> Option<usize> {
    i += 1;
    loop {
        match body.get(i)? {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
}

fn skip_value(body: &[u8], i: usize) -> Option<usize> {
    match body.get(i)? {
        b'"' => skip_string(body, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut j = i;
            loop {
                match body.get(j)? {
                    b'"' => {
                        j = skip_string(body, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
        }
        _ => {
            let mut j = i;
            while body
                .get(j)
                .is_some_and(|b| !matches!(b, b',' | b'}' | b']') && !b.is_ascii_whitespace())
            {
                j += 1;
            }
            (j > i).then_some(j)
        }
    }
}

/// sjson's string encoding: plain quoting unless a byte needs escaping, then
/// `json.Marshal` (which also HTML-escapes).
pub fn sjson_string(s: &str) -> String {
    if s.bytes()
        .any(|b| !(b' '..=0x7f).contains(&b) || b == b'"' || b == b'\\')
    {
        crate::gojson::string(s)
    } else {
        format!("\"{s}\"")
    }
}

/// `sjson.SetRawBytes(body, key, raw)` for a top-level key of an object body.
pub fn set_raw(body: &[u8], key: &str, raw: &str) -> Option<Vec<u8>> {
    if let Some((_, start, end)) = member(body, key) {
        let mut out = Vec::with_capacity(body.len() + raw.len());
        out.extend_from_slice(&body[..start]);
        out.extend_from_slice(raw.as_bytes());
        out.extend_from_slice(&body[end..]);
        return Some(out);
    }
    let close = body.iter().rposition(|&b| b == b'}')?;
    let head = &body[..close];
    let empty = head
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .is_some_and(|i| head[i] == b'{');
    let mut out = head.to_vec();
    if !empty {
        out.push(b',');
    }
    out.extend_from_slice(sjson_string(key).as_bytes());
    out.push(b':');
    out.extend_from_slice(raw.as_bytes());
    out.extend_from_slice(&body[close..]);
    Some(out)
}

/// `sjson.SetBytes(body, key, value)` for a string value.
pub fn set_string(body: &[u8], key: &str, value: &str) -> Option<Vec<u8>> {
    set_raw(body, key, &sjson_string(value))
}

/// `sjson.DeleteBytes(body, key)` for a top-level key: the member and one comma go.
pub fn delete(body: &[u8], key: &str) -> Option<Vec<u8>> {
    let (start, _, end) = member(body, key)?;
    let after = skip_ws(body, end);
    let (cut_start, cut_end) = if body.get(after) == Some(&b',') {
        (start, after + 1)
    } else {
        let before = body[..start].iter().rposition(|b| !b.is_ascii_whitespace())?;
        if body[before] == b',' {
            (before, end)
        } else {
            (start, end)
        }
    };
    let mut out = body[..cut_start].to_vec();
    out.extend_from_slice(&body[cut_end..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_keep_other_bytes() {
        let body = br#"{ "model" : "a", "stream":true , "x":{"stream":1} }"#;
        assert_eq!(
            String::from_utf8(delete(body, "stream").unwrap()).unwrap(),
            r#"{ "model" : "a",  "x":{"stream":1} }"#
        );
        assert_eq!(
            String::from_utf8(set_string(body, "model", "b<c").unwrap()).unwrap(),
            r#"{ "model" : "b<c", "stream":true , "x":{"stream":1} }"#
        );
        assert_eq!(
            String::from_utf8(set_raw(b"{}", "k", "1").unwrap()).unwrap(),
            r#"{"k":1}"#
        );
        assert_eq!(
            String::from_utf8(set_raw(br#"{"a":1}"#, "k", "2").unwrap()).unwrap(),
            r#"{"a":1,"k":2}"#
        );
        assert_eq!(
            String::from_utf8(delete(br#"{"a":1,"stream":true}"#, "stream").unwrap()).unwrap(),
            r#"{"a":1}"#
        );
        assert!(delete(b"[1]", "a").is_none());
        assert_eq!(sjson_string("q\"é"), r#""q\"é""#);
    }
}
