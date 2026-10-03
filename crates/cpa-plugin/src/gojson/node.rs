//! A parsed JSON document as Go's decoder walks it: every object member in order
//! (duplicates included), numbers as their literal text, strings unquoted the way
//! `encoding/json` does (invalid UTF-8 bytes and lone surrogates become U+FFFD).
//! Syntax follows Go's scanner; nesting is limited to 10000 levels, as in Go.

use super::DecodeError;

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Null,
    Bool(bool),
    /// The number literal.
    Number(String),
    String(String),
    Array(Vec<Node>),
    Object(Vec<(String, Node)>),
}

impl Node {
    pub fn is_null(&self) -> bool {
        matches!(self, Node::Null)
    }

    /// The last member whose key matches `name` case-insensitively.
    pub fn member(&self, name: &str) -> Option<&Node> {
        match self {
            Node::Object(members) => members
                .iter()
                .rev()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// Go's `any` view: numbers as float64, objects with the last duplicate winning.
    pub fn to_value(&self) -> Result<serde_json::Value, DecodeError> {
        Ok(match self {
            Node::Null => serde_json::Value::Null,
            Node::Bool(b) => serde_json::Value::Bool(*b),
            Node::Number(n) => {
                let f = parse_f64(n)?;
                serde_json::Number::from_f64(f)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            }
            Node::String(s) => serde_json::Value::String(s.clone()),
            Node::Array(items) => serde_json::Value::Array(items.iter().map(Node::to_value).collect::<Result<_, _>>()?),
            Node::Object(members) => {
                let mut map = serde_json::Map::new();
                for (k, v) in members {
                    map.insert(k.clone(), v.to_value()?);
                }
                serde_json::Value::Object(map)
            }
        })
    }

    /// Compact JSON text (for `json.RawMessage` fields).
    pub fn to_json(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut Vec<u8>) {
        match self {
            Node::Null => out.extend_from_slice(b"null"),
            Node::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
            Node::Number(n) => out.extend_from_slice(n.as_bytes()),
            Node::String(s) => cpa_common::json::marshal_str(out, s.as_bytes(), false),
            Node::Array(items) => {
                out.push(b'[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    item.write(out);
                }
                out.push(b']');
            }
            Node::Object(members) => {
                out.push(b'{');
                for (i, (k, v)) in members.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    cpa_common::json::marshal_str(out, k.as_bytes(), false);
                    out.push(b':');
                    v.write(out);
                }
                out.push(b'}');
            }
        }
    }
}

/// `strconv.ParseFloat(s, 64)` as `encoding/json` uses it: overflow is an error.
pub fn parse_f64(lexeme: &str) -> Result<f64, DecodeError> {
    match lexeme.parse::<f64>() {
        Ok(f) if f.is_finite() => Ok(f),
        _ => Err(DecodeError(format!(
            "json: cannot unmarshal number {lexeme} into Go value of type float64"
        ))),
    }
}

const MAX_DEPTH: usize = 10000;

enum Frame {
    Array(Vec<Node>),
    Object(Vec<(String, Node)>, String),
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

fn describe(c: u8) -> String {
    match c {
        b'\'' => r"'\''".to_owned(),
        b'"' => r#"'"'"#.to_owned(),
        c if c.is_ascii_graphic() || c == b' ' => format!("'{}'", c as char),
        c => format!("{:?}", c as char),
    }
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn err(&self, context: &str) -> DecodeError {
        match self.s.get(self.i) {
            None => DecodeError("unexpected end of JSON input".into()),
            Some(&c) => DecodeError(format!("invalid character {} {context}", describe(c))),
        }
    }

    fn literal(&mut self, word: &[u8], node: Node) -> Result<Node, DecodeError> {
        for &expected in word {
            if self.s.get(self.i) != Some(&expected) {
                return Err(self.err("in literal"));
            }
            self.i += 1;
        }
        Ok(node)
    }

    fn number(&mut self) -> Result<Node, DecodeError> {
        let start = self.i;
        let digits = |p: &mut Self| {
            let from = p.i;
            while p.i < p.s.len() && p.s[p.i].is_ascii_digit() {
                p.i += 1;
            }
            p.i > from
        };
        if self.s.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        match self.s.get(self.i) {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => {
                digits(self);
            }
            _ => return Err(self.err("in numeric literal")),
        }
        if self.s.get(self.i) == Some(&b'.') {
            self.i += 1;
            if !digits(self) {
                return Err(self.err("after decimal point in numeric literal"));
            }
        }
        if matches!(self.s.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.s.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !digits(self) {
                return Err(self.err("in exponent of numeric literal"));
            }
        }
        Ok(Node::Number(
            String::from_utf8_lossy(&self.s[start..self.i]).into_owned(),
        ))
    }

    fn hex4(&self, at: usize) -> Option<u32> {
        let hex = self.s.get(at..at + 4)?;
        u32::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()
    }

    /// Go's scanner rules plus `unquote`.
    fn string(&mut self) -> Result<String, DecodeError> {
        self.i += 1;
        let mut out = String::new();
        loop {
            let Some(&c) = self.s.get(self.i) else {
                return Err(DecodeError("unexpected end of JSON input".into()));
            };
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    let Some(&e) = self.s.get(self.i + 1) else {
                        return Err(DecodeError("unexpected end of JSON input".into()));
                    };
                    self.i += 2;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let Some(first) = self.hex4(self.i) else {
                                self.i += self.s[self.i..]
                                    .iter()
                                    .take(4)
                                    .take_while(|c| c.is_ascii_hexdigit())
                                    .count();
                                return Err(self.err("in \\u hexadecimal character escape"));
                            };
                            self.i += 4;
                            if (0xD800..0xDC00).contains(&first)
                                && self.s.get(self.i..self.i + 2) == Some(b"\\u")
                                && let Some(second) = self.hex4(self.i + 2)
                                && (0xDC00..0xE000).contains(&second)
                            {
                                self.i += 6;
                                let c = 0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00);
                                out.push(char::from_u32(c).unwrap_or('\u{FFFD}'));
                            } else {
                                out.push(char::from_u32(first).unwrap_or('\u{FFFD}'));
                            }
                        }
                        _ => {
                            self.i -= 1;
                            return Err(self.err("in string escape code"));
                        }
                    }
                }
                c if c < 0x20 => return Err(self.err("in string literal")),
                c if c < 0x80 => {
                    out.push(c as char);
                    self.i += 1;
                }
                _ => {
                    let (rune, size) = cpa_common::json::decode_rune(&self.s[self.i..]);
                    out.push(rune.unwrap_or('\u{FFFD}'));
                    self.i += size.max(1);
                }
            }
        }
    }

    fn scalar_or_open(&mut self, stack: &mut Vec<Frame>) -> Result<Option<Node>, DecodeError> {
        self.ws();
        match self.s.get(self.i) {
            Some(b'{') => {
                self.i += 1;
                stack.push(Frame::Object(Vec::new(), String::new()));
                Ok(None)
            }
            Some(b'[') => {
                self.i += 1;
                stack.push(Frame::Array(Vec::new()));
                Ok(None)
            }
            Some(b'"') => self.string().map(|s| Some(Node::String(s))),
            Some(b't') => self.literal(b"true", Node::Bool(true)).map(Some),
            Some(b'f') => self.literal(b"false", Node::Bool(false)).map(Some),
            Some(b'n') => self.literal(b"null", Node::Null).map(Some),
            Some(b'-' | b'0'..=b'9') => self.number().map(Some),
            _ => Err(self.err("looking for beginning of value")),
        }
    }
}

/// `json.Unmarshal`'s syntax pass and value tree.
pub fn parse(raw: &[u8]) -> Result<Node, DecodeError> {
    let mut p = Parser { s: raw, i: 0 };
    let mut stack: Vec<Frame> = Vec::new();
    // Each iteration either opens a container, or produces a value and attaches it.
    let mut value = p.scalar_or_open(&mut stack)?;
    loop {
        if stack.len() > MAX_DEPTH {
            return Err(DecodeError("exceeded max depth".into()));
        }
        // Object frames need a key before each value.
        if value.is_none() {
            match stack.last_mut() {
                Some(Frame::Object(members, key)) => {
                    p.ws();
                    if members.is_empty() && p.s.get(p.i) == Some(&b'}') {
                        p.i += 1;
                        let Some(Frame::Object(members, _)) = stack.pop() else {
                            unreachable!()
                        };
                        value = Some(Node::Object(members));
                    } else {
                        if p.s.get(p.i) != Some(&b'"') {
                            return Err(p.err("looking for beginning of object key string"));
                        }
                        *key = p.string()?;
                        p.ws();
                        if p.s.get(p.i) != Some(&b':') {
                            return Err(p.err("after object key"));
                        }
                        p.i += 1;
                        value = p.scalar_or_open(&mut stack)?;
                        continue;
                    }
                }
                Some(Frame::Array(items)) => {
                    p.ws();
                    if items.is_empty() && p.s.get(p.i) == Some(&b']') {
                        p.i += 1;
                        let Some(Frame::Array(items)) = stack.pop() else {
                            unreachable!()
                        };
                        value = Some(Node::Array(items));
                    } else {
                        value = p.scalar_or_open(&mut stack)?;
                        continue;
                    }
                }
                None => unreachable!("a value or an open container is always pending"),
            }
        }
        let done = value.take().expect("value set above");
        match stack.last_mut() {
            None => {
                p.ws();
                if p.i < p.s.len() {
                    return Err(p.err("after top-level value"));
                }
                return Ok(done);
            }
            Some(Frame::Array(items)) => {
                items.push(done);
                p.ws();
                match p.s.get(p.i) {
                    Some(b',') => {
                        p.i += 1;
                        value = p.scalar_or_open(&mut stack)?;
                    }
                    Some(b']') => {
                        p.i += 1;
                        let Some(Frame::Array(items)) = stack.pop() else {
                            unreachable!()
                        };
                        value = Some(Node::Array(items));
                    }
                    _ => return Err(p.err("after array element")),
                }
            }
            Some(Frame::Object(members, key)) => {
                members.push((std::mem::take(key), done));
                p.ws();
                match p.s.get(p.i) {
                    Some(b',') => {
                        p.i += 1;
                        p.ws();
                        if p.s.get(p.i) != Some(&b'"') {
                            return Err(p.err("looking for beginning of object key string"));
                        }
                        let k = p.string()?;
                        p.ws();
                        if p.s.get(p.i) != Some(&b':') {
                            return Err(p.err("after object key"));
                        }
                        p.i += 1;
                        if let Some(Frame::Object(_, key)) = stack.last_mut() {
                            *key = k;
                        }
                        value = p.scalar_or_open(&mut stack)?;
                    }
                    Some(b'}') => {
                        p.i += 1;
                        let Some(Frame::Object(members, _)) = stack.pop() else {
                            unreachable!()
                        };
                        value = Some(Node::Object(members));
                    }
                    _ => return Err(p.err("after object key:value pair")),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_duplicates_lexemes_and_go_string_rules() {
        let n = parse(br#" {"a":1.50,"a":[true,null,{}],"s":"\ud83d\ude00\ud800x\u00e9","e":[]} "#).unwrap();
        let Node::Object(m) = &n else { panic!() };
        assert_eq!(m.len(), 4);
        assert_eq!(m[0], ("a".into(), Node::Number("1.50".into())));
        assert_eq!(m[2].1, Node::String("😀\u{FFFD}xé".into()));
        assert_eq!(
            String::from_utf8(n.to_json()).unwrap(),
            r#"{"a":1.50,"a":[true,null,{}],"s":"😀�xé","e":[]}"#
        );
        // Invalid UTF-8 becomes one U+FFFD per bad byte, as Go's unquote does.
        assert_eq!(parse(b"\"\xe2\x82\"").unwrap(), Node::String("\u{FFFD}\u{FFFD}".into()));
        for bad in [
            &b"{\"a\":1,}"[..],
            b"[01]",
            b"\"a\nb\"",
            b"{} x",
            b"",
            b"[1.]",
            b"tru",
            b"{\"a\" 1}",
        ] {
            assert!(parse(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
        let deep = format!("{}{}", "[".repeat(10001), "]".repeat(10001));
        assert!(parse(deep.as_bytes()).is_err());
        let ok = format!("{}{}", "[".repeat(9999), "]".repeat(9999));
        assert!(parse(ok.as_bytes()).is_ok());
    }
}
