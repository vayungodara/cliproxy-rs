//! Go `encoding/json` decoding for the small, flat auth records Meta (and later Devin)
//! read from auth servers: `json.Unmarshal(body, &record)` with string, int and bool
//! fields.
//!
//! Go semantics reproduced: syntax errors with Go's scanner messages; keys matched to
//! field tags exactly, else case-insensitively (Go's fold, including `ſ` and the Kelvin
//! sign); later duplicates win; `null` leaves a field unchanged; a value of the wrong
//! type is skipped, decoding continues, and the first such error is returned. Unknown
//! keys are ignored. Expected values come from tests/device_fixtures/meta/decode.json.

use cpa_common::json::{self as gj, Kind};

/// A destination field (`json:"<tag>"`).
pub(crate) enum Slot<'a> {
    Str(&'a mut String),
    /// Go `int` or `int64`; the name is used in error messages.
    Int(&'a mut i64, &'static str),
    Bool(&'a mut bool),
}

impl Slot<'_> {
    fn go_type(&self) -> &'static str {
        match self {
            Self::Str(_) => "string",
            Self::Int(_, name) => name,
            Self::Bool(_) => "bool",
        }
    }
}

/// `json.Unmarshal(raw, &v)` for a struct `package.name` with `fields` (tag, slot).
pub(crate) fn unmarshal(raw: &[u8], package: &str, name: &str, fields: &mut [(&str, Slot<'_>)]) -> Result<(), String> {
    check_valid(raw)?;
    let root = gj::parse(raw);
    if root.kind == Kind::Null {
        return Ok(());
    }
    if !root.is_object() {
        return Err(format!(
            "json: cannot unmarshal {} into Go value of type {package}.{name}",
            kind_name(&root)
        ));
    }
    let mut first_error: Option<String> = None;
    root.each(|key, value| {
        let key = gj::go_unquote(&key.raw).unwrap_or_default();
        let key = key.as_str();
        let index = fields
            .iter()
            .position(|(tag, _)| *tag == key)
            .or_else(|| fields.iter().position(|(tag, _)| fold(tag) == fold(key)));
        let Some(index) = index else {
            return true;
        };
        let (tag, slot) = &mut fields[index];
        if let Err(value_name) = store(slot, &value)
            && first_error.is_none()
        {
            first_error = Some(format!(
                "json: cannot unmarshal {value_name} into Go struct field {name}.{tag} of type {}",
                slot.go_type()
            ));
        }
        true
    });
    first_error.map_or(Ok(()), Err)
}

/// Stores `value` in `slot`; on a type mismatch returns Go's description of the value.
fn store(slot: &mut Slot<'_>, value: &gj::Res<'_>) -> Result<(), String> {
    match (slot, value.kind) {
        (_, Kind::Null) => Ok(()),
        (Slot::Str(s), Kind::String) => {
            // Go's unquote: invalid UTF-8 becomes U+FFFD.
            **s = gj::go_unquote(&value.raw).unwrap_or_default();
            Ok(())
        }
        (Slot::Int(n, _), Kind::Number) => {
            let literal = String::from_utf8_lossy(&value.raw);
            match literal.parse::<i64>() {
                Ok(parsed) if literal.bytes().all(|b| b.is_ascii_digit() || b == b'-') => {
                    **n = parsed;
                    Ok(())
                }
                _ => Err(format!("number {literal}")),
            }
        }
        (Slot::Bool(b), Kind::True | Kind::False) => {
            **b = value.kind == Kind::True;
            Ok(())
        }
        _ => Err(kind_name(value).to_owned()),
    }
}

fn kind_name(value: &gj::Res<'_>) -> &'static str {
    match value.kind {
        Kind::String => "string",
        Kind::Number => "number",
        Kind::True | Kind::False => "bool",
        Kind::Json if value.is_array() => "array",
        Kind::Json => "object",
        Kind::Null => "null",
    }
}

/// Go's `foldName`: ASCII upper case, and every other rune folded to the smallest rune of
/// its case-fold set. Only `ſ` (to `S`) and the Kelvin sign (to `K`) fold onto ASCII.
fn fold(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'a'..='z' => c.to_ascii_uppercase(),
            '\u{17f}' => 'S',
            '\u{212a}' => 'K',
            _ => c,
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Parse {
    ObjectKey,
    ObjectValue,
    ArrayValue,
}

#[derive(Clone, Copy)]
enum State {
    BeginValue,
    BeginValueOrEmpty,
    BeginStringOrEmpty,
    BeginString,
    EndValue,
    EndTop,
    InString,
    InStringEsc,
    /// Inside `\u`, with this many hex digits read.
    InStringEscU(u8),
    Neg,
    One,
    Zero,
    Dot,
    Dot0,
    E,
    ESign,
    E0,
    /// Inside `true`/`false`/`null`, at this byte of the word.
    Literal(&'static str, usize),
}

const MAX_DEPTH: usize = 10_000;

/// Go's `checkValid` (encoding/json scanner), with its error messages.
pub(crate) fn check_valid(raw: &[u8]) -> Result<(), String> {
    let mut s = Scanner {
        state: State::BeginValue,
        stack: Vec::new(),
        end_top: false,
    };
    for &c in raw {
        s.step(c)?;
    }
    if s.end_top {
        return Ok(());
    }
    s.step(b' ')?;
    if s.end_top {
        return Ok(());
    }
    Err("unexpected end of JSON input".into())
}

struct Scanner {
    state: State,
    stack: Vec<Parse>,
    end_top: bool,
}

fn space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\r' | b'\n')
}

fn hex(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

/// Go's `quoteChar`: the byte as a rune, in `strconv.Quote` form between single quotes.
fn quote_char(c: u8) -> String {
    let inner = match c {
        b'\'' => "\\'".to_owned(),
        b'"' => "\"".to_owned(),
        b'\\' => "\\\\".to_owned(),
        7 => "\\a".to_owned(),
        8 => "\\b".to_owned(),
        0x0c => "\\f".to_owned(),
        b'\n' => "\\n".to_owned(),
        b'\r' => "\\r".to_owned(),
        b'\t' => "\\t".to_owned(),
        0x0b => "\\v".to_owned(),
        0..=0x1f | 0x7f => format!("\\x{c:02x}"),
        0x80..=0xa0 | 0xad => format!("\\u{c:04x}"),
        _ => char::from(c).to_string(),
    };
    format!("'{inner}'")
}

fn invalid(c: u8, context: &str) -> String {
    format!("invalid character {} {context}", quote_char(c))
}

impl Scanner {
    fn push(&mut self, c: u8, parse: Parse) -> Result<(), String> {
        self.stack.push(parse);
        if self.stack.len() > MAX_DEPTH {
            return Err(invalid(c, "exceeded max depth"));
        }
        Ok(())
    }

    fn pop(&mut self) {
        self.stack.pop();
        if self.stack.is_empty() {
            self.state = State::EndTop;
            self.end_top = true;
        } else {
            self.state = State::EndValue;
        }
    }

    fn step(&mut self, c: u8) -> Result<(), String> {
        match self.state {
            State::BeginValueOrEmpty => {
                if space(c) {
                    return Ok(());
                }
                if c == b']' {
                    return self.end_value(c);
                }
                self.begin_value(c)
            }
            State::BeginValue => self.begin_value(c),
            State::BeginStringOrEmpty => {
                if space(c) {
                    return Ok(());
                }
                if c == b'}' {
                    if let Some(top) = self.stack.last_mut() {
                        *top = Parse::ObjectValue;
                    }
                    return self.end_value(c);
                }
                self.begin_string(c)
            }
            State::BeginString => self.begin_string(c),
            State::EndValue => self.end_value(c),
            State::EndTop => self.end_top(c),
            State::InString => {
                match c {
                    b'"' => self.state = State::EndValue,
                    b'\\' => self.state = State::InStringEsc,
                    0..=0x1f => return Err(invalid(c, "in string literal")),
                    _ => {}
                }
                Ok(())
            }
            State::InStringEsc => match c {
                b'b' | b'f' | b'n' | b'r' | b't' | b'\\' | b'/' | b'"' => {
                    self.state = State::InString;
                    Ok(())
                }
                b'u' => {
                    self.state = State::InStringEscU(0);
                    Ok(())
                }
                _ => Err(invalid(c, "in string escape code")),
            },
            State::InStringEscU(n) => {
                if !hex(c) {
                    return Err(invalid(c, "in \\u hexadecimal character escape"));
                }
                self.state = if n == 3 {
                    State::InString
                } else {
                    State::InStringEscU(n + 1)
                };
                Ok(())
            }
            State::Neg => match c {
                b'0' => {
                    self.state = State::Zero;
                    Ok(())
                }
                b'1'..=b'9' => {
                    self.state = State::One;
                    Ok(())
                }
                _ => Err(invalid(c, "in numeric literal")),
            },
            State::One => {
                if c.is_ascii_digit() {
                    return Ok(());
                }
                self.zero(c)
            }
            State::Zero => self.zero(c),
            State::Dot => {
                if c.is_ascii_digit() {
                    self.state = State::Dot0;
                    return Ok(());
                }
                Err(invalid(c, "after decimal point in numeric literal"))
            }
            State::Dot0 => {
                if c.is_ascii_digit() {
                    return Ok(());
                }
                if c == b'e' || c == b'E' {
                    self.state = State::E;
                    return Ok(());
                }
                self.end_value(c)
            }
            State::E => {
                if c == b'+' || c == b'-' {
                    self.state = State::ESign;
                    return Ok(());
                }
                self.e_sign(c)
            }
            State::ESign => self.e_sign(c),
            State::E0 => {
                if c.is_ascii_digit() {
                    return Ok(());
                }
                self.end_value(c)
            }
            State::Literal(word, at) => {
                let expected = word.as_bytes()[at];
                if c != expected {
                    return Err(invalid(
                        c,
                        &format!("in literal {word} (expecting '{}')", expected as char),
                    ));
                }
                self.state = if at + 1 == word.len() {
                    State::EndValue
                } else {
                    State::Literal(word, at + 1)
                };
                Ok(())
            }
        }
    }

    fn begin_value(&mut self, c: u8) -> Result<(), String> {
        if space(c) {
            return Ok(());
        }
        self.state = match c {
            b'{' => {
                self.state = State::BeginStringOrEmpty;
                return self.push(c, Parse::ObjectKey);
            }
            b'[' => {
                self.state = State::BeginValueOrEmpty;
                return self.push(c, Parse::ArrayValue);
            }
            b'"' => State::InString,
            b'-' => State::Neg,
            b'0' => State::Zero,
            b't' => State::Literal("true", 1),
            b'f' => State::Literal("false", 1),
            b'n' => State::Literal("null", 1),
            b'1'..=b'9' => State::One,
            _ => return Err(invalid(c, "looking for beginning of value")),
        };
        Ok(())
    }

    fn begin_string(&mut self, c: u8) -> Result<(), String> {
        if space(c) {
            return Ok(());
        }
        if c == b'"' {
            self.state = State::InString;
            return Ok(());
        }
        Err(invalid(c, "looking for beginning of object key string"))
    }

    fn zero(&mut self, c: u8) -> Result<(), String> {
        match c {
            b'.' => {
                self.state = State::Dot;
                Ok(())
            }
            b'e' | b'E' => {
                self.state = State::E;
                Ok(())
            }
            _ => self.end_value(c),
        }
    }

    fn e_sign(&mut self, c: u8) -> Result<(), String> {
        if c.is_ascii_digit() {
            self.state = State::E0;
            return Ok(());
        }
        Err(invalid(c, "in exponent of numeric literal"))
    }

    fn end_value(&mut self, c: u8) -> Result<(), String> {
        let Some(&top) = self.stack.last() else {
            self.state = State::EndTop;
            self.end_top = true;
            return self.end_top(c);
        };
        if space(c) {
            self.state = State::EndValue;
            return Ok(());
        }
        match top {
            Parse::ObjectKey => {
                if c == b':' {
                    *self.stack.last_mut().unwrap() = Parse::ObjectValue;
                    self.state = State::BeginValue;
                    return Ok(());
                }
                Err(invalid(c, "after object key"))
            }
            Parse::ObjectValue => match c {
                b',' => {
                    *self.stack.last_mut().unwrap() = Parse::ObjectKey;
                    self.state = State::BeginString;
                    Ok(())
                }
                b'}' => {
                    self.pop();
                    Ok(())
                }
                _ => Err(invalid(c, "after object key:value pair")),
            },
            Parse::ArrayValue => match c {
                b',' => {
                    self.state = State::BeginValue;
                    Ok(())
                }
                b']' => {
                    self.pop();
                    Ok(())
                }
                _ => Err(invalid(c, "after array element")),
            },
        }
    }

    fn end_top(&mut self, c: u8) -> Result<(), String> {
        if !space(c) {
            return Err(invalid(c, "after top-level value"));
        }
        Ok(())
    }
}
