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
    /// Walks with an explicit stack, so the 10000 levels `parse` accepts cannot
    /// overflow the thread stack.
    pub fn to_value(&self) -> Result<serde_json::Value, DecodeError> {
        enum Open<'a> {
            Array(std::slice::Iter<'a, Node>, Vec<serde_json::Value>),
            Object(
                std::slice::Iter<'a, (String, Node)>,
                serde_json::Map<String, serde_json::Value>,
                String,
            ),
        }
        let mut stack: Vec<Open> = Vec::new();
        let mut next = self;
        loop {
            let mut done = match next {
                Node::Null => Some(serde_json::Value::Null),
                Node::Bool(b) => Some(serde_json::Value::Bool(*b)),
                Node::Number(n) => match parse_f64(n) {
                    Ok(f) => Some(
                        serde_json::Number::from_f64(f)
                            .map(serde_json::Value::Number)
                            .unwrap_or(serde_json::Value::Null),
                    ),
                    Err(e) => {
                        for open in stack {
                            match open {
                                Open::Array(_, out) => out.into_iter().for_each(dispose),
                                Open::Object(_, out, _) => out.into_iter().for_each(|(_, v)| dispose(v)),
                            }
                        }
                        return Err(e);
                    }
                },
                Node::String(s) => Some(serde_json::Value::String(s.clone())),
                Node::Array(items) => {
                    stack.push(Open::Array(items.iter(), Vec::with_capacity(items.len())));
                    None
                }
                Node::Object(members) => {
                    stack.push(Open::Object(members.iter(), serde_json::Map::new(), String::new()));
                    None
                }
            };
            // Attach finished values to their parents until a parent has another child.
            loop {
                if let Some(value) = done.take() {
                    match stack.last_mut() {
                        None => return Ok(value),
                        Some(Open::Array(_, out)) => out.push(value),
                        Some(Open::Object(_, out, key)) => {
                            // A duplicate key replaces the earlier value, which may be deep.
                            if let Some(old) = out.insert(std::mem::take(key), value) {
                                dispose(old);
                            }
                        }
                    }
                }
                match stack.last_mut() {
                    None => unreachable!("an open container is pending"),
                    Some(Open::Array(items, _)) => {
                        if let Some(item) = items.next() {
                            next = item;
                            break;
                        }
                    }
                    Some(Open::Object(members, _, key)) => {
                        if let Some((k, v)) = members.next() {
                            key.clone_from(k);
                            next = v;
                            break;
                        }
                    }
                }
                done = Some(match stack.pop() {
                    Some(Open::Array(_, out)) => serde_json::Value::Array(out),
                    Some(Open::Object(_, out, _)) => serde_json::Value::Object(out),
                    None => unreachable!(),
                });
            }
        }
    }

    /// Compact JSON text (for `json.RawMessage` fields).
    pub fn to_json(&self) -> Vec<u8> {
        enum Step<'a> {
            Node(&'a Node),
            Key(&'a str),
            Byte(u8),
        }
        let mut out = Vec::new();
        let mut steps = vec![Step::Node(self)];
        while let Some(step) = steps.pop() {
            let node = match step {
                Step::Byte(b) => {
                    out.push(b);
                    continue;
                }
                Step::Key(k) => {
                    cpa_common::json::marshal_str(&mut out, k.as_bytes(), false);
                    continue;
                }
                Step::Node(node) => node,
            };
            match node {
                Node::Null => out.extend_from_slice(b"null"),
                Node::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
                Node::Number(n) => out.extend_from_slice(n.as_bytes()),
                Node::String(s) => cpa_common::json::marshal_str(&mut out, s.as_bytes(), false),
                Node::Array(items) => {
                    out.push(b'[');
                    steps.push(Step::Byte(b']'));
                    for (i, item) in items.iter().enumerate().rev() {
                        steps.push(Step::Node(item));
                        if i > 0 {
                            steps.push(Step::Byte(b','));
                        }
                    }
                }
                Node::Object(members) => {
                    out.push(b'{');
                    steps.push(Step::Byte(b'}'));
                    for (i, (k, v)) in members.iter().enumerate().rev() {
                        steps.push(Step::Node(v));
                        steps.push(Step::Byte(b':'));
                        steps.push(Step::Key(k));
                        if i > 0 {
                            steps.push(Step::Byte(b','));
                        }
                    }
                }
            }
        }
        out
    }
}

/// Drops a `serde_json::Value` with an explicit stack; its own drop glue recurses
/// once per nesting level.
fn dispose(value: serde_json::Value) {
    let mut pending = vec![value];
    while let Some(mut value) = pending.pop() {
        match &mut value {
            serde_json::Value::Array(items) => pending.append(items),
            serde_json::Value::Object(members) => {
                pending.extend(std::mem::take(members).into_iter().map(|(_, v)| v));
            }
            _ => {}
        }
    }
}

/// Frees nested containers with an explicit stack instead of the recursive
/// drop glue, which overflows the thread stack on deeply nested documents.
impl Drop for Node {
    fn drop(&mut self) {
        let mut pending: Vec<Node> = Vec::new();
        let take = |node: &mut Node, pending: &mut Vec<Node>| match node {
            Node::Array(items) => pending.append(items),
            Node::Object(members) => pending.extend(members.drain(..).map(|(_, v)| v)),
            _ => {}
        };
        take(self, &mut pending);
        while let Some(mut node) = pending.pop() {
            take(&mut node, &mut pending);
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

    /// Go's scanner: a wrong byte names the literal and the byte it expected.
    fn literal(&mut self, word: &[u8], node: Node) -> Result<Node, DecodeError> {
        for &expected in word {
            if self.s.get(self.i) != Some(&expected) {
                return Err(self.err(&format!(
                    "in literal {} (expecting {})",
                    String::from_utf8_lossy(word),
                    describe(expected)
                )));
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
    parse_value(raw, true)
}

/// `json.NewDecoder(r).Decode` over a complete reader: the first value only, with
/// anything after it left unread; a truncated value is `unexpected EOF` and input
/// without any value `EOF`.
pub fn parse_first(raw: &[u8]) -> Result<Node, DecodeError> {
    if raw.iter().all(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\r')) {
        return Err(DecodeError("EOF".into()));
    }
    parse_value(raw, false).map_err(|e| match e.0.as_str() {
        "unexpected end of JSON input" => DecodeError("unexpected EOF".into()),
        _ => e,
    })
}

/// The value tree; `whole` rejects anything but whitespace after the value.
fn parse_value(raw: &[u8], whole: bool) -> Result<Node, DecodeError> {
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
                if whole && p.i < p.s.len() {
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

    #[test]
    fn deep_documents_convert_and_drop_without_recursion() {
        let text = format!("{}1{}", r#"{"a":["#.repeat(4999), "]}".repeat(4999));
        let node = parse(text.as_bytes()).unwrap();
        assert_eq!(node.to_json(), text.as_bytes());
        let mut value = node.to_value().unwrap();
        for _ in 0..4999 {
            value = value["a"][0].take();
        }
        assert_eq!(value, serde_json::json!(1.0));
        // The last duplicate wins in Go's `any` view; the text keeps both.
        let dup = parse(br#"{"k":{"x":1},"k":[2,{"y":null}]}"#).unwrap();
        assert_eq!(dup.to_value().unwrap(), serde_json::json!({"k": [2.0, {"y": null}]}));
        assert_eq!(dup.to_json(), br#"{"k":{"x":1},"k":[2,{"y":null}]}"#);
        // A deep value replaced by a later duplicate, and deep values already built
        // when a later number overflows, are freed without recursion.
        let deep = format!("{}{}", "[".repeat(9990), "]".repeat(9990));
        let replaced = parse(format!(r#"{{"k":{deep},"k":0}}"#).as_bytes()).unwrap();
        assert_eq!(replaced.to_value().unwrap(), serde_json::json!({"k": 0.0}));
        let overflow = parse(format!(r#"[{deep},1e400]"#).as_bytes()).unwrap();
        assert!(overflow.to_value().is_err());
    }
}
