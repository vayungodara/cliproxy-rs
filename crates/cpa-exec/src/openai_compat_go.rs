//! Go standard-library behaviour the executor's byte handling depends on, where Rust's
//! defaults differ: `encoding/json.Valid`, `bytes.TrimSpace`, gjson's `Int()` coercion,
//! `net/http.ParseTime`, and byte-exact edits of JSON that is not valid UTF-8.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// encoding/json's `maxNestingDepth`.
const MAX_DEPTH: usize = 10_000;

/// `json.Valid`: RFC 8259 grammar, string bytes unchecked for UTF-8, nesting capped at
/// 10,000. Iterative, so hostile depth cannot exhaust the stack.
pub(crate) fn json_valid(b: &[u8]) -> bool {
    let ws = |mut i: usize| {
        while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r' | b'\n') {
            i += 1;
        }
        i
    };
    let string = |mut i: usize| -> Option<usize> {
        if b.get(i) != Some(&b'"') {
            return None;
        }
        i += 1;
        loop {
            match *b.get(i)? {
                b'"' => return Some(i + 1),
                b'\\' => match *b.get(i + 1)? {
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => i += 2,
                    b'u' => {
                        let hex = b.get(i + 2..i + 6)?;
                        if !hex.iter().all(u8::is_ascii_hexdigit) {
                            return None;
                        }
                        i += 6;
                    }
                    _ => return None,
                },
                c if c < 0x20 => return None,
                _ => i += 1,
            }
        }
    };
    let number = |mut i: usize| -> Option<usize> {
        if b.get(i) == Some(&b'-') {
            i += 1;
        }
        match *b.get(i)? {
            b'0' => i += 1,
            b'1'..=b'9' => {
                while b.get(i).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                }
            }
            _ => return None,
        }
        if b.get(i) == Some(&b'.') {
            i += 1;
            if !b.get(i).is_some_and(u8::is_ascii_digit) {
                return None;
            }
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
        }
        if matches!(b.get(i), Some(b'e' | b'E')) {
            i += 1;
            if matches!(b.get(i), Some(b'+' | b'-')) {
                i += 1;
            }
            if !b.get(i).is_some_and(u8::is_ascii_digit) {
                return None;
            }
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
        }
        Some(i)
    };
    let mut stack: Vec<u8> = Vec::new();
    let mut i = ws(0);
    'value: loop {
        // Parse one value starting at `i`.
        match b.get(i) {
            Some(b'{') | Some(b'[') => {
                let open = b[i];
                stack.push(open);
                if stack.len() > MAX_DEPTH {
                    return false;
                }
                i = ws(i + 1);
                let close = if open == b'{' { b'}' } else { b']' };
                if b.get(i) == Some(&close) {
                    stack.pop();
                    i += 1;
                } else if open == b'{' {
                    let Some(next) = string(i) else { return false };
                    i = ws(next);
                    if b.get(i) != Some(&b':') {
                        return false;
                    }
                    i = ws(i + 1);
                    continue 'value;
                } else {
                    continue 'value;
                }
            }
            Some(b'"') => match string(i) {
                Some(next) => i = next,
                None => return false,
            },
            Some(b't') if b[i..].starts_with(b"true") => i += 4,
            Some(b'f') if b[i..].starts_with(b"false") => i += 5,
            Some(b'n') if b[i..].starts_with(b"null") => i += 4,
            Some(b'-' | b'0'..=b'9') => match number(i) {
                Some(next) => i = next,
                None => return false,
            },
            _ => return false,
        }
        // After a value: close containers or move to the next element.
        loop {
            i = ws(i);
            match stack.last() {
                None => return i == b.len(),
                Some(&open) => {
                    let close = if open == b'{' { b'}' } else { b']' };
                    match b.get(i) {
                        Some(c) if *c == close => {
                            stack.pop();
                            i += 1;
                        }
                        Some(b',') => {
                            i = ws(i + 1);
                            if open == b'{' {
                                let Some(next) = string(i) else { return false };
                                i = ws(next);
                                if b.get(i) != Some(&b':') {
                                    return false;
                                }
                                i = ws(i + 1);
                            }
                            continue 'value;
                        }
                        _ => return false,
                    }
                }
            }
        }
    }
}

/// `bytes.TrimSpace`: Unicode white space at both ends; invalid UTF-8 stops trimming.
pub(crate) fn trim_space(b: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < b.len() {
        match leading_char(&b[start..]) {
            Some((c, n)) if c.is_whitespace() => start += n,
            _ => break,
        }
    }
    let mut end = b.len();
    while end > start {
        match trailing_char(&b[start..end]) {
            Some((c, n)) if c.is_whitespace() => end -= n,
            _ => break,
        }
    }
    &b[start..end]
}

fn leading_char(b: &[u8]) -> Option<(char, usize)> {
    let len = match b.first()? {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return None,
    };
    let c = std::str::from_utf8(b.get(..len)?).ok()?.chars().next()?;
    Some((c, len))
}

fn trailing_char(b: &[u8]) -> Option<(char, usize)> {
    for len in 1..=4.min(b.len()) {
        if let Ok(s) = std::str::from_utf8(&b[b.len() - len..])
            && let Some(c) = s.chars().next()
            && c.len_utf8() == len
        {
            return Some((c, len));
        }
    }
    None
}

/// gjson `Result.Int()`.
pub(crate) fn int(value: &gjson::Value<'_>) -> i64 {
    let parse_int = |s: &str| {
        let digits = s.strip_prefix('-').unwrap_or(s);
        (!digits.is_empty() && digits.bytes().all(|c| c.is_ascii_digit()))
            .then(|| s.parse::<i64>().ok())
            .flatten()
    };
    match value.kind() {
        gjson::Kind::True => 1,
        gjson::Kind::String => parse_int(value.str()).unwrap_or(0),
        gjson::Kind::Number => {
            let f = value.f64();
            // safeInt: integral and within ±2^53.
            if f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_991.0 {
                return f as i64;
            }
            parse_int(value.json()).unwrap_or(f as i64)
        }
        _ => 0,
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// A tiny cursor over Go time layout elements.
struct Cursor<'a>(&'a str);

impl Cursor<'_> {
    fn lit(&mut self, s: &str) -> Option<()> {
        self.0 = self.0.strip_prefix(s)?;
        Some(())
    }

    /// Go `lookup`: case-insensitive table match.
    fn name(&mut self, table: &[&str], short: bool) -> Option<usize> {
        for (i, full) in table.iter().enumerate() {
            let name = if short { &full[..3] } else { full };
            if self.0.len() >= name.len() && self.0[..name.len()].eq_ignore_ascii_case(name) {
                self.0 = &self.0[name.len()..];
                return Some(i);
            }
        }
        None
    }

    /// Go `getnum`: one or two digits, exactly two when `fixed`.
    fn num(&mut self, fixed: bool) -> Option<u32> {
        let b = self.0.as_bytes();
        if !b.first()?.is_ascii_digit() {
            return None;
        }
        let two = b.get(1).is_some_and(u8::is_ascii_digit);
        if !two && fixed {
            return None;
        }
        let n = if two { 2 } else { 1 };
        let v = self.0[..n].parse().ok()?;
        self.0 = &self.0[n..];
        Some(v)
    }

    fn digits(&mut self, n: usize) -> Option<i32> {
        let s = self.0.get(..n)?;
        if !s.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        self.0 = &self.0[n..];
        s.parse().ok()
    }

    /// `15:04:05` with Go's optional fractional seconds.
    fn clock(&mut self) -> Option<(u32, u32, u32)> {
        let h = self.num(false)?;
        self.lit(":")?;
        let m = self.num(true)?;
        self.lit(":")?;
        let s = self.num(true)?;
        if let Some(rest) = self.0.strip_prefix(['.', ',']) {
            let n = rest.bytes().take_while(u8::is_ascii_digit).count();
            if n > 0 {
                self.0 = &rest[n..];
            }
        }
        (h < 24 && m < 60 && s < 60).then_some((h, m, s))
    }
}

fn utc(year: i32, month: usize, day: u32, (h, m, s): (u32, u32, u32)) -> Option<SystemTime> {
    let date = chrono::NaiveDate::from_ymd_opt(year, month as u32 + 1, day)?;
    let secs = date.and_hms_opt(h, m, s)?.and_utc().timestamp();
    if secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(secs as u64))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(secs.unsigned_abs()))
    }
}

/// `http.ParseTime`: RFC 1123 with `GMT`, RFC 850, then ANSI C. Like Go's `time.Parse`
/// the weekday must be a valid name but is not checked against the date, and an RFC 850
/// zone abbreviation is read with a zero offset.
pub(crate) fn parse_http_time(raw: &str) -> Option<SystemTime> {
    let rfc1123 = || {
        let mut c = Cursor(raw);
        c.name(&DAYS, true)?;
        c.lit(", ")?;
        let day = c.num(true)?;
        c.lit(" ")?;
        let month = c.name(&MONTHS, false)?;
        c.lit(" ")?;
        let year = c.digits(4)?;
        c.lit(" ")?;
        let clock = c.clock()?;
        c.lit(" GMT")?;
        c.0.is_empty().then_some(())?;
        utc(year, month, day, clock)
    };
    let rfc850 = || {
        let mut c = Cursor(raw);
        c.name(&DAYS, false)?;
        c.lit(", ")?;
        let day = c.num(true)?;
        c.lit("-")?;
        let month = c.name(&MONTHS, false)?;
        c.lit("-")?;
        let yy = c.digits(2)?;
        c.lit(" ")?;
        let clock = c.clock()?;
        c.lit(" ")?;
        let zone = c.0.bytes().take_while(u8::is_ascii_uppercase).count();
        (zone >= 3).then_some(())?;
        c.0 = &c.0[zone..];
        c.0.is_empty().then_some(())?;
        utc(if yy >= 69 { 1900 + yy } else { 2000 + yy }, month, day, clock)
    };
    let ansic = || {
        let mut c = Cursor(raw);
        c.name(&DAYS, true)?;
        c.lit(" ")?;
        let month = c.name(&MONTHS, false)?;
        c.lit(" ")?;
        if c.0.starts_with(' ') {
            c.0 = &c.0[1..];
        }
        let day = c.num(false)?;
        c.lit(" ")?;
        let clock = c.clock()?;
        c.lit(" ")?;
        let year = c.digits(4)?;
        c.0.is_empty().then_some(())?;
        utc(year, month, day, clock)
    };
    rfc1123().or_else(rfc850).or_else(ansic)
}

/// Code points standing in for bytes that are not UTF-8, so sjson-style text edits can
/// run on Go `[]byte` JSON and every untouched byte comes back unchanged.
const BYTE_BASE: u32 = 0x10_FF00;

/// Bytes as editable text. `None` only when valid input already uses the stand-in range.
pub(crate) fn bytes_to_text(b: &[u8]) -> Option<String> {
    if let Ok(s) = std::str::from_utf8(b) {
        return Some(s.to_owned());
    }
    let mut out = String::with_capacity(b.len() + 8);
    for chunk in b.utf8_chunks() {
        if chunk.valid().chars().any(|c| c as u32 >= BYTE_BASE) {
            return None;
        }
        out.push_str(chunk.valid());
        for byte in chunk.invalid() {
            out.push(char::from_u32(BYTE_BASE + u32::from(*byte)).expect("valid code point"));
        }
    }
    Some(out)
}

/// Inverse of [`bytes_to_text`].
pub(crate) fn text_to_bytes(s: &str) -> Vec<u8> {
    if !s.chars().any(|c| c as u32 >= BYTE_BASE) {
        return s.as_bytes().to_vec();
    }
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        let code = c as u32;
        if code >= BYTE_BASE {
            out.push((code - BYTE_BASE) as u8);
        } else {
            let mut buf = [0; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_valid_matches_go_grammar_and_depth() {
        for ok in ["{}", " [1, -0.5e+3, \"a\\u00e9\", true, null, {\"k\": []}] "] {
            assert!(json_valid(ok.as_bytes()), "{ok}");
        }
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\"}",
            "01",
            "1.",
            "\"\u{1}\"",
            "nul",
            "[1] x",
            "{\"a\":1,}",
        ] {
            assert!(!json_valid(bad.as_bytes()), "{bad}");
        }
        assert!(
            json_valid(b"{\"a\":\"\xff\"}"),
            "string bytes are not checked for UTF-8"
        );
        let deep = |n| format!("{}0{}", "[".repeat(n), "]".repeat(n));
        assert!(json_valid(deep(10_000).as_bytes()));
        assert!(!json_valid(deep(10_001).as_bytes()));
        assert!(!json_valid("[".repeat(1_000_000).as_bytes()));
    }

    #[test]
    fn trim_space_is_unicode_aware() {
        assert_eq!(trim_space("\u{a0} x \u{3000}\r".as_bytes()), b"x");
        assert_eq!(trim_space(b"\xff "), b"\xff");
    }

    #[test]
    fn int_follows_gjson() {
        let v = |s: &str| int(&gjson::parse(s));
        assert_eq!(v("\"429.5\""), 0);
        assert_eq!(v("\"-7\""), -7);
        assert_eq!(v("429.9"), 429);
        assert_eq!(v("4e2"), 400);
        assert_eq!(v("true"), 1);
    }

    #[test]
    fn http_time_layouts_ignore_weekday_consistency() {
        let t = |s| parse_http_time(s).map(|t| t.duration_since(UNIX_EPOCH).unwrap().as_secs());
        assert_eq!(t("Mon, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(t("Sunday, 06-Nov-94 08:49:37 GMT"), Some(784111777));
        assert_eq!(t("Sun Nov  6 08:49:37 1994"), Some(784111777));
        assert_eq!(t("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784111777));
        assert_eq!(t("Xyz, 06 Nov 1994 08:49:37 GMT"), None);
        assert_eq!(t("Sun, 31 Feb 1994 08:49:37 GMT"), None);
    }

    #[test]
    fn non_utf8_round_trips() {
        let raw = b"{\"p\":\"\xff\xfe\",\"q\":\"\xc3\xa9\"}";
        let text = bytes_to_text(raw).unwrap();
        assert_eq!(text_to_bytes(&text), raw);
    }
}
