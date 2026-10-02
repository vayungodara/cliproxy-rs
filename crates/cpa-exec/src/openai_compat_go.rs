//! Go standard-library behaviour the executor's byte handling depends on, where Rust's
//! defaults differ: `encoding/json.Valid`, `bytes.TrimSpace`, and `net/http.ParseTime`.

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

/// A cursor over a value parsed with a Go time layout.
struct Cursor<'a>(&'a str);

impl Cursor<'_> {
    /// Go `skip`: a space in the layout matches one or more spaces (or the end).
    fn lit(&mut self, layout: &str) -> Option<()> {
        let mut layout = layout;
        while let Some(c) = layout.chars().next() {
            if c == ' ' {
                if !self.0.is_empty() && !self.0.starts_with(' ') {
                    return None;
                }
                layout = layout.trim_start_matches(' ');
                self.0 = self.0.trim_start_matches(' ');
                continue;
            }
            self.0 = self.0.strip_prefix(c)?;
            layout = &layout[c.len_utf8()..];
        }
        Some(())
    }

    /// Go `lookup`: case-insensitive table match.
    fn name(&mut self, table: &[&str], short: bool) -> Option<usize> {
        for (i, full) in table.iter().enumerate() {
            let name = if short { &full[..3] } else { full };
            if self.0.len() >= name.len() && self.0.as_bytes()[..name.len()].eq_ignore_ascii_case(name.as_bytes()) {
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

    /// `15:04:05`, plus the fractional seconds Go accepts after `05` (`parseNanoseconds`).
    fn clock(&mut self) -> Option<(u32, u32, u32, u32)> {
        let h = self.num(false)?;
        self.lit(":")?;
        let m = self.num(true)?;
        self.lit(":")?;
        let s = self.num(true)?;
        let mut nanos = 0;
        let b = self.0.as_bytes();
        if b.len() >= 2 && matches!(b[0], b'.' | b',') && b[1].is_ascii_digit() {
            let n = 1 + b[1..].iter().take_while(|c| c.is_ascii_digit()).count();
            let frac = &self.0[1..n.min(10)];
            let mut scaled: u32 = frac.parse().ok()?;
            for _ in frac.len()..9 {
                scaled *= 10;
            }
            nanos = scaled;
            self.0 = &self.0[n..];
        }
        (h < 24 && m < 60 && s < 60).then_some((h, m, s, nanos))
    }

    /// Go `parseTimeZone` for `MST`. Abbreviations the host zone does not define get a
    /// fabricated location that keeps the UTC reading, so every accepted zone (including
    /// `GMT+3`) leaves the instant unchanged.
    fn zone(&mut self) -> Option<()> {
        let v = self.0;
        if let Some(rest) = v.strip_prefix("UTC") {
            self.0 = rest;
            return Some(());
        }
        let b = v.as_bytes();
        if b.len() < 3 {
            return None;
        }
        if v.starts_with("ChST") || v.starts_with("MeST") {
            self.0 = &v[4..];
            return Some(());
        }
        if let Some(rest) = v.strip_prefix("GMT") {
            // parseGMT: an optional signed hour offset up to 23.
            let sign = rest.chars().next().filter(|c| matches!(c, '+' | '-'));
            let digits = sign.map_or("", |_| &rest[1..]);
            let n = digits.bytes().take_while(u8::is_ascii_digit).count();
            let hours_ok = digits[..n].parse::<u32>().is_ok_and(|h| h <= 23);
            self.0 = if sign.is_some() && n > 0 && hours_ok {
                &digits[n..]
            } else {
                rest
            };
            return Some(());
        }
        let upper = b.iter().take(6).take_while(|c| c.is_ascii_uppercase()).count();
        let len = match upper {
            3 => 3,
            4 if b[3] == b'T' || v.starts_with("WITA") => 4,
            5 if b[4] == b'T' => 5,
            _ => return None,
        };
        self.0 = &v[len..];
        Some(())
    }
}

fn instant(year: i32, month: usize, day: u32, (h, m, s, nanos): (u32, u32, u32, u32)) -> Option<SystemTime> {
    let date = chrono::NaiveDate::from_ymd_opt(year, month as u32 + 1, day)?;
    let secs = date.and_hms_opt(h, m, s)?.and_utc().timestamp();
    let at = if secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(secs as u64))?
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(secs.unsigned_abs()))?
    };
    at.checked_add(Duration::from_nanos(u64::from(nanos)))
}

/// `http.ParseTime`: RFC 1123 with `GMT`, RFC 850, then ANSI C, with Go `time.Parse`
/// rules: the weekday must be a valid name but is not checked against the date, layout
/// spaces match runs of spaces, and fractional seconds are kept.
// ponytail: a zone abbreviation defined by the host's local zone would shift the instant
// in Go; servers run in UTC, where none does.
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
        instant(year, month, day, clock)
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
        c.zone()?;
        c.0.is_empty().then_some(())?;
        instant(if yy >= 69 { 1900 + yy } else { 2000 + yy }, month, day, clock)
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
        instant(year, month, day, clock)
    };
    rfc1123().or_else(rfc850).or_else(ansic)
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
    fn http_time_layouts_ignore_weekday_consistency() {
        let t = |s| parse_http_time(s).map(|t| t.duration_since(UNIX_EPOCH).unwrap().as_secs());
        assert_eq!(t("Mon, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(t("Sunday, 06-Nov-94 08:49:37 GMT"), Some(784111777));
        assert_eq!(t("Sun Nov  6 08:49:37 1994"), Some(784111777));
        assert_eq!(t("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784111777));
        assert_eq!(t("Xyz, 06 Nov 1994 08:49:37 GMT"), None);
        assert_eq!(t("Sun, 31 Feb 1994 08:49:37 GMT"), None);
    }
}
