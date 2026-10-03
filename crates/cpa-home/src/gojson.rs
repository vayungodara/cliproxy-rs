//! A tiny writer for Go `json.Marshal` output of flat structs: fields in declaration
//! order, HTML-escaped strings, sorted map keys. Home receives these as RESP keys and
//! payloads; byte parity keeps sizes (in-flight part limits) identical to Go.

use std::collections::BTreeMap;

pub(crate) struct Object {
    out: Vec<u8>,
    first: bool,
}

impl Object {
    pub(crate) fn new() -> Self {
        Self {
            out: vec![b'{'],
            first: true,
        }
    }

    fn key(&mut self, key: &str) -> &mut Vec<u8> {
        if !self.first {
            self.out.push(b',');
        }
        self.first = false;
        string_into(&mut self.out, key.as_bytes());
        self.out.push(b':');
        &mut self.out
    }

    pub(crate) fn str(mut self, key: &str, value: &str) -> Self {
        string_into(self.key(key), value.as_bytes());
        self
    }

    /// `,omitempty` string.
    pub(crate) fn str_opt(self, key: &str, value: &str) -> Self {
        if value.is_empty() { self } else { self.str(key, value) }
    }

    pub(crate) fn int(mut self, key: &str, value: i64) -> Self {
        let text = value.to_string();
        self.key(key).extend_from_slice(text.as_bytes());
        self
    }

    pub(crate) fn raw(mut self, key: &str, raw: &[u8]) -> Self {
        self.key(key).extend_from_slice(raw);
        self
    }

    /// `map[string]string,omitempty`.
    pub(crate) fn map_opt(self, key: &str, map: &BTreeMap<String, String>) -> Self {
        if map.is_empty() {
            return self;
        }
        let mut inner = Object::new();
        for (k, v) in map {
            inner = inner.str(k, v);
        }
        self.raw(key, &inner.finish())
    }

    pub(crate) fn strs(self, key: &str, items: &[String]) -> Self {
        let mut raw = vec![b'['];
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                raw.push(b',');
            }
            string_into(&mut raw, item.as_bytes());
        }
        raw.push(b']');
        self.raw(key, &raw)
    }

    pub(crate) fn finish(mut self) -> Vec<u8> {
        self.out.push(b'}');
        self.out
    }
}

/// A Go-encoded JSON string.
pub(crate) fn string_into(out: &mut Vec<u8>, value: &[u8]) {
    cpa_common::json::marshal_str(out, value, true);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_go_marshal() {
        let mut map = BTreeMap::new();
        map.insert("b".to_owned(), "2".to_owned());
        map.insert("a".to_owned(), "<1>".to_owned());
        let json = Object::new()
            .str("s", "x&y")
            .str_opt("empty", "")
            .int("n", -3)
            .map_opt("m", &map)
            .strs("l", &["\u{2028}".to_owned()])
            .finish();
        assert_eq!(
            String::from_utf8(json).unwrap(),
            r#"{"s":"x\u0026y","n":-3,"m":{"a":"\u003c1\u003e","b":"2"},"l":["\u2028"]}"#
        );
    }
}
