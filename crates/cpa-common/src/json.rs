//! Go-compatible JSON reading and editing.
//!
//! Byte-oriented ports of tidwall/gjson v1.18.0 (reading) and tidwall/sjson v1.2.5
//! (editing), with Go's encoding/json string and float encoding. CLIProxyAPI builds every
//! translated body with these two libraries, so matching their quirks is what makes the
//! output match byte for byte: number coercions (`"12.75"` is 0 as an int), conditional
//! HTML escaping of set strings, raw copies of malformed bytes, and appends that scan
//! backward for the last `}`.
//!
//! Strings are bytes, like Go strings. [`Res::str`] decodes lossily for comparisons;
//! anything copied into output goes through [`Res::bytes`] or the raw slice so invalid
//! UTF-8 keeps Go's treatment (raw when copied raw, `\ufffd` when marshaled).
//!
//! ponytail: paths cover keys, `\` escapes, `*`/`?` wildcards, indexes, `#`, `#.key` and
//! `|` pipes and `@this`. gjson queries (`#(...)`), other modifiers (`@reverse`), multipaths
//! and JSON lines return "not found"; no ported translator builds such a path. Port them when one does.

use std::borrow::Cow;

/// A gjson/sjson path. Go paths are byte strings, so keys taken from documents may not be
/// UTF-8; text paths work as before.
pub trait JsonPath {
    fn as_path(&self) -> &[u8];
}

impl JsonPath for str {
    fn as_path(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl JsonPath for String {
    fn as_path(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl JsonPath for [u8] {
    fn as_path(&self) -> &[u8] {
        self
    }
}

impl JsonPath for Vec<u8> {
    fn as_path(&self) -> &[u8] {
        self
    }
}

impl<T: JsonPath + ?Sized> JsonPath for &T {
    fn as_path(&self) -> &[u8] {
        (**self).as_path()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    #[default]
    Null,
    False,
    Number,
    String,
    True,
    Json,
}

/// gjson.Result. `raw` borrows the document unless the value was synthesized (`#`,
/// `#.key`), and `s` holds the unescaped string for [`Kind::String`].
#[derive(Clone, Debug, Default)]
pub struct Res<'a> {
    pub kind: Kind,
    pub raw: Cow<'a, [u8]>,
    pub s: Cow<'a, [u8]>,
    pub num: f64,
    pub index: usize,
    pub indexes: Option<Vec<usize>>,
}

const MODIFIERS: [&[u8]; 13] = [
    b"pretty", b"ugly", b"reverse", b"this", b"flatten", b"join", b"valid", b"keys", b"values", b"tostr", b"fromstr",
    b"group", b"dig",
];

impl<'a> Res<'a> {
    pub fn exists(&self) -> bool {
        self.kind != Kind::Null || !self.raw.is_empty()
    }

    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    /// Go's `Result.String()`, exact bytes.
    pub fn bytes(&self) -> Cow<'a, [u8]> {
        match self.kind {
            Kind::Null => Cow::Borrowed(b""),
            Kind::False => Cow::Borrowed(b"false"),
            Kind::True => Cow::Borrowed(b"true"),
            Kind::String => self.s.clone(),
            Kind::Json => self.raw.clone(),
            Kind::Number => {
                let raw = &self.raw[..];
                let digits = raw.strip_prefix(b"-").unwrap_or(raw);
                if raw.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
                    Cow::Owned(fmt_float(self.num).into_bytes())
                } else {
                    self.raw.clone()
                }
            }
        }
    }

    /// Go's `Result.String()`, decoded lossily. Use for comparisons and logic only.
    pub fn str(&self) -> Cow<'a, str> {
        match self.bytes() {
            Cow::Borrowed(b) => String::from_utf8_lossy(b),
            Cow::Owned(b) => Cow::Owned(String::from_utf8_lossy(&b).into_owned()),
        }
    }

    pub fn bool(&self) -> bool {
        match self.kind {
            Kind::True => true,
            Kind::String => matches!(self.s.to_ascii_lowercase().as_slice(), b"1" | b"t" | b"true"),
            Kind::Number => self.num != 0.0,
            _ => false,
        }
    }

    pub fn int(&self) -> i64 {
        match self.kind {
            Kind::True => 1,
            Kind::String => parse_int(&self.s).unwrap_or(0),
            Kind::Number => {
                if let Some(n) = safe_int(self.num) {
                    return n;
                }
                parse_int(&self.raw).unwrap_or_else(|| go_f64_to_i64(self.num))
            }
            _ => 0,
        }
    }

    pub fn uint(&self) -> u64 {
        match self.kind {
            Kind::True => 1,
            Kind::String => parse_uint(&self.s).unwrap_or(0),
            Kind::Number => match safe_int(self.num) {
                Some(n) if n >= 0 => n as u64,
                _ => parse_uint(&self.raw).unwrap_or_else(|| go_f64_to_u64(self.num)),
            },
            _ => 0,
        }
    }

    pub fn float(&self) -> f64 {
        match self.kind {
            Kind::True => 1.0,
            Kind::String => parse_float(&self.s),
            Kind::Number => self.num,
            _ => 0.0,
        }
    }

    pub fn is_object(&self) -> bool {
        self.kind == Kind::Json && self.raw.first() == Some(&b'{')
    }

    pub fn is_array(&self) -> bool {
        self.kind == Kind::Json && self.raw.first() == Some(&b'[')
    }

    pub fn is_bool(&self) -> bool {
        matches!(self.kind, Kind::True | Kind::False)
    }

    /// Owned copy, independent of the source document.
    pub fn into_owned(self) -> Res<'static> {
        Res {
            kind: self.kind,
            raw: Cow::Owned(self.raw.into_owned()),
            s: Cow::Owned(self.s.into_owned()),
            num: self.num,
            index: self.index,
            indexes: self.indexes,
        }
    }

    pub fn get(&self, path: &(impl JsonPath + ?Sized)) -> Res<'a> {
        let path = path.as_path();
        let mut r = match &self.raw {
            Cow::Borrowed(b) => get(b, path),
            Cow::Owned(v) => get(v, path).into_owned(),
        };
        match &mut r.indexes {
            Some(indexes) => indexes.iter_mut().for_each(|i| *i += self.index),
            None => r.index += self.index,
        }
        r
    }

    /// `json.Marshal(r.Value())`: gjson's own decoding (first duplicate key wins, gjson
    /// unescaping, float64 numbers) re-encoded with sorted keys. `None` where Marshal
    /// fails (NaN or infinite numbers).
    pub fn value_json(&self) -> Option<Vec<u8>> {
        AnyValue::from_res(self).marshal()
    }

    /// Go's `Result.Array()`: null is empty, a non-array is a one-element list.
    pub fn array(&self) -> Vec<Res<'a>> {
        if self.kind == Kind::Null {
            return vec![];
        }
        if !self.is_array() {
            return vec![self.clone()];
        }
        let mut items = match &self.raw {
            Cow::Borrowed(b) => array_or_map(b, b'[', self.index).0,
            Cow::Owned(v) => array_or_map(v, b'[', self.index)
                .0
                .into_iter()
                .map(Res::into_owned)
                .collect(),
        };
        if let Some(indexes) = &self.indexes {
            if indexes.len() != items.len() {
                items.iter_mut().for_each(|r| r.index = 0);
            } else {
                items.iter_mut().zip(indexes).for_each(|(r, i)| r.index = *i);
            }
        }
        items
    }

    /// Go's `Result.Map()` in document order; the first duplicate key wins.
    pub fn map(&self) -> Vec<(Vec<u8>, Res<'a>)> {
        if self.kind != Kind::Json {
            return vec![];
        }
        match &self.raw {
            Cow::Borrowed(b) => array_or_map(b, b'{', self.index).1,
            Cow::Owned(v) => array_or_map(v, b'{', self.index)
                .1
                .into_iter()
                .map(|(k, r)| (k, r.into_owned()))
                .collect(),
        }
    }

    /// Go's `Result.ForEach`. Keys are strings for objects and numbers for arrays; a
    /// non-JSON value is visited once with an empty key.
    pub fn each(&self, mut f: impl FnMut(Res<'a>, Res<'a>) -> bool) {
        if !self.exists() {
            return;
        }
        if self.kind != Kind::Json {
            f(Res::default(), self.clone());
            return;
        }
        let indexes = self.indexes.as_deref();
        match &self.raw {
            Cow::Borrowed(b) => each_in(b, self.index, indexes, &mut f),
            Cow::Owned(v) => each_in(v, self.index, indexes, &mut |k, v| f(k.into_owned(), v.into_owned())),
        }
    }
}

fn array_or_map(json: &[u8], vc: u8, base: usize) -> (Vec<Res<'_>>, Vec<(Vec<u8>, Res<'_>)>) {
    let mut items = vec![];
    let mut pairs: Vec<(Vec<u8>, Res<'_>)> = vec![];
    let mut i = 0;
    while i < json.len() {
        if json[i] == vc {
            i += 1;
            break;
        }
        if json[i] > b' ' {
            return (items, pairs);
        }
        i += 1;
    }
    let mut key: Option<Vec<u8>> = None;
    let mut count = 0;
    while i < json.len() {
        let c = json[i];
        if c <= b' ' {
            i += 1;
            continue;
        }
        if c == b']' || c == b'}' {
            break;
        }
        let rest = &json[i..];
        let mut value = match c {
            b'{' | b'[' => json_res(rest, 0, squash(rest).len()),
            b'n' | b't' | b'f' => literal_res(rest, 0, tolit(rest).len()),
            b'"' => {
                let (raw, s) = tostr(rest);
                Res {
                    kind: Kind::String,
                    raw: Cow::Borrowed(raw),
                    s,
                    ..Res::default()
                }
            }
            b'0'..=b'9' | b'-' => number_res(rest, 0, tonum(rest).len()),
            _ => {
                i += 1;
                continue;
            }
        };
        value.index = i + base;
        i += value.raw.len();
        if vc == b'{' {
            if count % 2 == 0 {
                key = Some(value.s.to_vec());
            } else if let Some(k) = key.take()
                && !pairs.iter().any(|(existing, _)| *existing == k)
            {
                pairs.push((k, value));
            }
            count += 1;
        } else {
            items.push(value);
        }
    }
    (items, pairs)
}

fn each_in<'d>(json: &'d [u8], base: usize, indexes: Option<&[usize]>, f: &mut dyn FnMut(Res<'d>, Res<'d>) -> bool) {
    let mut i = 0;
    let mut obj = false;
    let mut key = Res::default();
    while i < json.len() {
        if json[i] == b'{' {
            i += 1;
            key.kind = Kind::String;
            obj = true;
            break;
        } else if json[i] == b'[' {
            i += 1;
            key.kind = Kind::Number;
            key.num = -1.0;
            break;
        }
        if json[i] > b' ' {
            return;
        }
        i += 1;
    }
    let mut idx = 0;
    while i < json.len() {
        if obj {
            if json[i] != b'"' {
                i += 1;
                continue;
            }
            let s = i;
            let (next, end, esc, ok) = parse_string(json, i + 1);
            i = next;
            if !ok {
                return;
            }
            let raw = &json[s..end];
            let inner = &raw[1..raw.len() - 1];
            key.s = if esc {
                Cow::Owned(unescape(inner))
            } else {
                Cow::Borrowed(inner)
            };
            key.raw = Cow::Borrowed(raw);
            key.index = s + base;
        } else {
            key.num += 1.0;
        }
        while i < json.len() && (json[i] <= b' ' || json[i] == b',' || json[i] == b':') {
            i += 1;
        }
        let s = i;
        let (next, mut value, ok) = parse_any(json, i, true);
        i = next;
        if !ok {
            return;
        }
        match indexes {
            Some(ix) => {
                if let Some(&at) = ix.get(idx) {
                    value.index = at;
                }
            }
            None => value.index = s + base,
        }
        if !f(key.clone(), value) {
            return;
        }
        idx += 1;
        // Go's loop post statement skips one byte after every value.
        i += 1;
    }
}

fn safe_int(f: f64) -> Option<i64> {
    if f < -9007199254740991.0 || f > 9007199254740991.0 {
        return None;
    }
    Some(go_f64_to_i64(f))
}

/// Go's `int64(f)` on amd64: truncation, and the minimum value when out of range or NaN.
fn go_f64_to_i64(f: f64) -> i64 {
    if f.is_nan() || f >= 9.223_372_036_854_776e18 || f < -9.223_372_036_854_776e18 {
        i64::MIN
    } else {
        f as i64
    }
}

fn parse_uint(s: &[u8]) -> Option<u64> {
    if s.is_empty() || !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(
        s.iter()
            .fold(0u64, |n, c| n.wrapping_mul(10).wrapping_add(u64::from(c - b'0'))),
    )
}

fn parse_int(s: &[u8]) -> Option<i64> {
    let (neg, digits) = match s.strip_prefix(b"-") {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let n = parse_uint(digits)? as i64;
    Some(if neg { n.wrapping_neg() } else { n })
}

/// Go's `uint64(f)` on amd64: signed conversion below 2^63, offset conversion above.
fn go_f64_to_u64(f: f64) -> u64 {
    const CUTOFF: f64 = 9_223_372_036_854_775_808.0;
    if f < CUTOFF {
        go_f64_to_i64(f) as u64
    } else {
        go_f64_to_i64(f - CUTOFF) as u64 | (1 << 63)
    }
}

/// `strconv.ParseFloat(s, 64)` with the error ignored (0 on syntax errors, ±Inf on
/// overflow).
pub fn parse_float(s: &[u8]) -> f64 {
    go_parse_float(s).unwrap_or_else(|value| value)
}

/// `strconv.ParseFloat(s, 64)`: `Err` carries the value Go returns with the error (±Inf
/// on overflow, 0 on syntax errors). Accepts Go's signs, `inf`/`infinity`/`nan`,
/// digit-separating underscores and hexadecimal floats (`0x1.8p3`).
pub fn go_parse_float(s: &[u8]) -> Result<f64, f64> {
    let Ok(text) = std::str::from_utf8(s) else {
        return Err(0.0);
    };
    let (negative, body) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let sign = if negative { -1.0 } else { 1.0 };
    if body.eq_ignore_ascii_case("inf") || body.eq_ignore_ascii_case("infinity") {
        return Ok(sign * f64::INFINITY);
    }
    if text.eq_ignore_ascii_case("nan") {
        return Ok(f64::NAN);
    }
    let bytes = body.as_bytes();
    if bytes.len() > 2 && bytes[0] == b'0' && bytes[1] | 0x20 == b'x' {
        if text.contains('_') && !underscore_ok(text.as_bytes()) {
            return Err(0.0);
        }
        return parse_hex_float(&bytes[2..], negative);
    }
    if !bytes
        .iter()
        .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-' | b'_'))
    {
        return Err(0.0);
    }
    let cleaned;
    let digits = if text.contains('_') {
        if !underscore_ok(text.as_bytes()) {
            return Err(0.0);
        }
        cleaned = text.replace('_', "");
        cleaned.as_str()
    } else {
        text
    };
    match digits.parse::<f64>() {
        Ok(f) if f.is_infinite() => Err(f),
        Ok(f) => Ok(f),
        Err(_) => Err(0.0),
    }
}

/// strconv's readFloat + atofHex for the digits after `0x` (underscores already checked).
fn parse_hex_float(s: &[u8], negative: bool) -> Result<f64, f64> {
    const MANT_BITS: u32 = 52;
    const BIAS: i64 = -1023;
    let (mut mantissa, mut nd, mut nd_mant, mut dp) = (0u64, 0i64, 0i64, 0i64);
    let (mut saw_dot, mut saw_digits, mut trunc) = (false, false, false);
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        let digit = match c {
            b'_' => {
                i += 1;
                continue;
            }
            b'.' if saw_dot => break,
            b'.' => {
                saw_dot = true;
                dp = nd;
                i += 1;
                continue;
            }
            b'0'..=b'9' => u64::from(c - b'0'),
            _ if (b'a'..=b'f').contains(&(c | 0x20)) => u64::from((c | 0x20) - b'a' + 10),
            _ => break,
        };
        saw_digits = true;
        if c == b'0' && nd == 0 {
            dp -= 1;
        } else {
            nd += 1;
            if nd_mant < 16 {
                mantissa = mantissa * 16 + digit;
                nd_mant += 1;
            } else if c != b'0' {
                trunc = true;
            }
        }
        i += 1;
    }
    if !saw_digits {
        return Err(0.0);
    }
    if !saw_dot {
        dp = nd;
    }
    dp *= 4;
    nd_mant *= 4;
    if i >= s.len() || s[i] | 0x20 != b'p' {
        return Err(0.0);
    }
    i += 1;
    let mut exp_sign = 1;
    match s.get(i) {
        Some(b'+') => i += 1,
        Some(b'-') => {
            exp_sign = -1;
            i += 1;
        }
        _ => {}
    }
    if !s.get(i).is_some_and(u8::is_ascii_digit) {
        return Err(0.0);
    }
    let mut e = 0i64;
    while i < s.len() && (s[i].is_ascii_digit() || s[i] == b'_') {
        if s[i] != b'_' && e < 10000 {
            e = e * 10 + i64::from(s[i] - b'0');
        }
        i += 1;
    }
    if i != s.len() {
        return Err(0.0);
    }
    dp += e * exp_sign;
    let mut exp = if mantissa != 0 { dp - nd_mant } else { 0 };

    // atofHex
    let max_exp = (1i64 << 11) + BIAS - 2;
    let min_exp = BIAS + 1;
    exp += i64::from(MANT_BITS);
    while mantissa != 0 && mantissa >> (MANT_BITS + 2) == 0 {
        mantissa <<= 1;
        exp -= 1;
    }
    if trunc {
        mantissa |= 1;
    }
    while mantissa >> (1 + MANT_BITS + 2) != 0 {
        mantissa = mantissa >> 1 | mantissa & 1;
        exp += 1;
    }
    while mantissa > 1 && exp < min_exp - 2 {
        mantissa = mantissa >> 1 | mantissa & 1;
        exp += 1;
    }
    let mut round = mantissa & 3;
    mantissa >>= 2;
    round |= mantissa & 1;
    exp += 2;
    if round == 3 {
        mantissa += 1;
        if mantissa == 1 << (1 + MANT_BITS) {
            mantissa >>= 1;
            exp += 1;
        }
    }
    if mantissa >> MANT_BITS == 0 {
        exp = BIAS;
    }
    let overflow = exp > max_exp;
    if overflow {
        mantissa = 1 << MANT_BITS;
        exp = max_exp + 1;
    }
    let mut bits = mantissa & ((1 << MANT_BITS) - 1);
    bits |= (((exp - BIAS) & ((1 << 11) - 1)) as u64) << MANT_BITS;
    if negative {
        bits |= 1 << 63;
    }
    let f = f64::from_bits(bits);
    if overflow { Err(f) } else { Ok(f) }
}

/// strconv's underscoreOK: underscores only between digits (a base prefix counts as a
/// digit).
fn underscore_ok(s: &[u8]) -> bool {
    let s = s.strip_prefix(b"-").or_else(|| s.strip_prefix(b"+")).unwrap_or(s);
    let mut saw = b'^';
    let mut i = 0;
    let mut hex = false;
    if s.len() >= 2 && s[0] == b'0' && matches!(s[1] | 0x20, b'b' | b'o' | b'x') {
        i = 2;
        saw = b'0';
        hex = s[1] | 0x20 == b'x';
    }
    for &c in &s[i..] {
        if c.is_ascii_digit() || (hex && (b'a'..=b'f').contains(&(c | 0x20))) {
            saw = b'0';
        } else if c == b'_' {
            if saw != b'0' {
                return false;
            }
            saw = b'_';
        } else {
            if saw == b'_' {
                return false;
            }
            saw = b'!';
        }
    }
    saw != b'_'
}

/// `strconv.FormatFloat(f, 'f', -1, 64)`.
pub fn fmt_float(f: f64) -> String {
    if f.is_infinite() {
        return if f > 0.0 { "+Inf".into() } else { "-Inf".into() };
    }
    format!("{f}")
}

/// encoding/json's float64 encoding (shortest digits, exponent form outside
/// [1e-6, 1e21)). `None` for NaN and infinities, which Marshal rejects.
pub fn json_float(f: f64) -> Option<String> {
    if !f.is_finite() {
        return None;
    }
    let abs = f.abs();
    if abs != 0.0 && !(1e-6..1e21).contains(&abs) {
        let e = format!("{f:e}");
        let (mantissa, exp) = e.split_once('e').unwrap();
        let exp: i32 = exp.parse().unwrap();
        return Some(if exp < 0 {
            format!("{mantissa}e-{}", -exp)
        } else {
            format!("{mantissa}e+{exp:02}")
        });
    }
    Some(format!("{f}"))
}

fn tonum(json: &[u8]) -> &[u8] {
    for i in 1..json.len() {
        let c = json[i];
        if c <= b'-' {
            if c <= b' ' || c == b',' {
                return &json[..i];
            }
        } else if c == b']' || c == b'}' {
            return &json[..i];
        }
    }
    json
}

fn tolit(json: &[u8]) -> &[u8] {
    for i in 1..json.len() {
        if !json[i].is_ascii_lowercase() {
            return &json[..i];
        }
    }
    json
}

fn tostr(json: &[u8]) -> (&[u8], Cow<'_, [u8]>) {
    let mut i = 1;
    while i < json.len() {
        if json[i] > b'\\' {
            i += 1;
            continue;
        }
        if json[i] == b'"' {
            return (&json[..i + 1], Cow::Borrowed(&json[1..i]));
        }
        if json[i] == b'\\' {
            i += 1;
            while i < json.len() {
                if json[i] > b'\\' {
                    i += 1;
                    continue;
                }
                if json[i] == b'"' {
                    if json[i - 1] == b'\\' && backslashes_before(json, i, 1).is_multiple_of(2) {
                        i += 1;
                        continue;
                    }
                    return (&json[..i + 1], Cow::Owned(unescape(&json[1..i])));
                }
                i += 1;
            }
            let raw = if i + 1 < json.len() { &json[..i + 1] } else { &json[..i] };
            return (raw, Cow::Owned(unescape(&json[1..i.min(json.len())])));
        }
        i += 1;
    }
    (json, Cow::Borrowed(&json[1.min(json.len())..]))
}

/// Count of consecutive backslashes ending at `i - 2`, stopping above `floor` (Go loops
/// with `j > floor - 1`). Callers already know `json[i - 1]` is a backslash.
fn backslashes_before(json: &[u8], i: usize, floor: usize) -> usize {
    let mut n = 0;
    let mut j = i as isize - 2;
    while j >= floor as isize {
        if json[j as usize] != b'\\' {
            break;
        }
        n += 1;
        j -= 1;
    }
    n
}

fn squash(json: &[u8]) -> &[u8] {
    let (mut i, mut depth) = if json[0] != b'"' { (1, 1) } else { (0, 0) };
    while i < json.len() {
        match json[i] {
            b'"' => {
                i += 1;
                let s2 = i;
                while i < json.len() {
                    if json[i] > b'\\' {
                        i += 1;
                        continue;
                    }
                    if json[i] == b'"' {
                        if json[i - 1] == b'\\' && backslashes_before(json, i, s2).is_multiple_of(2) {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                if depth == 0 {
                    if i >= json.len() {
                        return json;
                    }
                    return &json[..i + 1];
                }
            }
            b'{' | b'[' | b'(' => depth += 1,
            b'}' | b']' | b')' => {
                depth -= 1;
                if depth == 0 {
                    return &json[..i + 1];
                }
            }
            _ => {}
        }
        i += 1;
    }
    json
}

fn rune_at(hex: &[u8]) -> u32 {
    std::str::from_utf8(&hex[..4])
        .ok()
        .filter(|h| !h.starts_with(['+', '-']))
        .and_then(|h| u32::from_str_radix(h, 16).ok())
        .unwrap_or(0)
}

/// gjson's unescape: stops at the first control byte or bad escape, decodes `\u`
/// surrogate pairs and writes U+FFFD for lone surrogates.
pub fn unescape(json: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(json.len());
    let mut i = 0;
    while i < json.len() {
        let c = json[i];
        if c < b' ' {
            return out;
        }
        if c != b'\\' {
            out.push(c);
            i += 1;
            continue;
        }
        i += 1;
        if i >= json.len() {
            return out;
        }
        match json[i] {
            b'\\' => out.push(b'\\'),
            b'/' => out.push(b'/'),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'"' => out.push(b'"'),
            b'u' => {
                if i + 5 > json.len() {
                    return out;
                }
                let mut r = rune_at(&json[i + 1..]);
                i += 5;
                if (0xD800..0xE000).contains(&r) && json.len() - i >= 6 && json[i] == b'\\' && json[i + 1] == b'u' {
                    let r2 = rune_at(&json[i + 2..]);
                    r = if (0xD800..0xDC00).contains(&r) && (0xDC00..0xE000).contains(&r2) {
                        0x10000 + ((r - 0xD800) << 10) + (r2 - 0xDC00)
                    } else {
                        0xFFFD
                    };
                    i += 6;
                }
                let ch = char::from_u32(r).unwrap_or('\u{FFFD}');
                let mut buf = [0; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                continue;
            }
            _ => return out,
        }
        i += 1;
    }
    out
}

/// Returns `(next, value_end, escaped, ok)` for a string whose opening quote is at
/// `i - 1`; the raw value is `json[i - 1..value_end]`.
fn parse_string(json: &[u8], i: usize) -> (usize, usize, bool, bool) {
    // memchr jumps over the string body; prompts make these strings hundreds of KB.
    let Some(k) = memchr::memchr2(b'"', b'\\', &json[i.min(json.len())..]) else {
        return (json.len().max(i), json.len(), false, false);
    };
    let mut i = i + k;
    if json[i] == b'"' {
        return (i + 1, i + 1, false, true);
    }
    i += 1;
    match closing_quote(json, i, 1) {
        Some(q) => (q + 1, q + 1, true, true),
        None => (json.len(), json.len(), false, false),
    }
}

/// The first `"` at or after `i` not escaped by a backslash (counted back to `floor`).
fn closing_quote(json: &[u8], mut i: usize, floor: usize) -> Option<usize> {
    while i < json.len() {
        i += memchr::memchr(b'"', &json[i..])?;
        if json[i - 1] == b'\\' && backslashes_before(json, i, floor).is_multiple_of(2) {
            i += 1;
            continue;
        }
        return Some(i);
    }
    None
}

fn parse_number(json: &[u8], mut i: usize) -> (usize, usize) {
    i += 1;
    while i < json.len() {
        if json[i] <= b' ' || matches!(json[i], b',' | b']' | b'}') {
            return (i, i);
        }
        i += 1;
    }
    (i, json.len())
}

fn parse_literal(json: &[u8], mut i: usize) -> (usize, usize) {
    i += 1;
    while i < json.len() {
        if !json[i].is_ascii_lowercase() {
            return (i, i);
        }
        i += 1;
    }
    (i, json.len())
}

fn vchar(c: u8) -> u8 {
    match c {
        b'"' => 2,
        b'{' | b'(' | b'[' => 3,
        b'}' | b')' | b']' => 1,
        _ => 0,
    }
}

/// Returns `(next, end)`; the value is `json[i..end]`.
fn parse_squash(json: &[u8], i: usize) -> (usize, usize) {
    let s = i;
    let mut i = i + 1;
    let mut depth: i32 = 1;
    while i < json.len() {
        let c = vchar(json[i]);
        if c == 0 {
            i += 1;
            continue;
        }
        if c == 2 {
            i += 1;
            i = closing_quote(json, i, i).unwrap_or(json.len());
        } else {
            depth += i32::from(c) - 2;
            if depth == 0 {
                i += 1;
                return (i, i);
            }
        }
        i += 1;
    }
    let _ = s;
    (i, json.len())
}

fn string_res(json: &[u8], start: usize, end: usize, esc: bool) -> Res<'_> {
    let raw = &json[start..end];
    let inner = &raw[1..raw.len() - 1];
    Res {
        kind: Kind::String,
        raw: Cow::Borrowed(raw),
        s: if esc {
            Cow::Owned(unescape(inner))
        } else {
            Cow::Borrowed(inner)
        },
        index: start,
        ..Res::default()
    }
}

fn number_res(json: &[u8], start: usize, end: usize) -> Res<'_> {
    let raw = &json[start..end];
    Res {
        kind: Kind::Number,
        raw: Cow::Borrowed(raw),
        num: parse_float(raw),
        index: start,
        ..Res::default()
    }
}

fn literal_res(json: &[u8], start: usize, end: usize) -> Res<'_> {
    Res {
        kind: match json[start] {
            b't' => Kind::True,
            b'f' => Kind::False,
            _ => Kind::Null,
        },
        raw: Cow::Borrowed(&json[start..end]),
        index: start,
        ..Res::default()
    }
}

fn json_res(json: &[u8], start: usize, end: usize) -> Res<'_> {
    Res {
        kind: Kind::Json,
        raw: Cow::Borrowed(&json[start..end]),
        index: start,
        ..Res::default()
    }
}

fn unindexed(r: Res<'_>) -> Res<'_> {
    Res { index: 0, ..r }
}

fn parse_any(json: &[u8], mut i: usize, hit: bool) -> (usize, Res<'_>, bool) {
    while i < json.len() {
        let c = json[i];
        if c == b'{' || c == b'[' {
            let (next, end) = parse_squash(json, i);
            let res = if hit { json_res(json, i, end) } else { Res::default() };
            return (next, res, true);
        }
        if c <= b' ' {
            i += 1;
            continue;
        }
        let mut num = false;
        match c {
            b'"' => {
                let start = i;
                let (next, end, esc, ok) = parse_string(json, i + 1);
                if !ok {
                    return (next, Res::default(), false);
                }
                // Go's parseAny only assigns indexes to containers.
                let res = if hit {
                    unindexed(string_res(json, start, end, esc))
                } else {
                    Res::default()
                };
                return (next, res, true);
            }
            b'n' if i + 1 < json.len() && json[i + 1] != b'u' => num = true,
            b'n' | b't' | b'f' => {
                let start = i;
                let (next, end) = parse_literal(json, i);
                i = next;
                if hit {
                    return (i, unindexed(literal_res(json, start, end)), true);
                }
            }
            b'+' | b'-' | b'0'..=b'9' | b'i' | b'I' | b'N' => num = true,
            _ => {}
        }
        if num {
            let start = i;
            let (next, end) = parse_number(json, i);
            let res = if hit {
                unindexed(number_res(json, start, end))
            } else {
                Res::default()
            };
            return (next, res, true);
        }
        i += 1;
    }
    (i, Res::default(), false)
}

struct Ctx<'a> {
    json: &'a [u8],
    value: Res<'a>,
    pipe: Option<Vec<u8>>,
}

struct ObjectPath<'p> {
    part: Cow<'p, [u8]>,
    path: &'p [u8],
    pipe: &'p [u8],
    piped: bool,
    wild: bool,
    more: bool,
}

fn is_dot_piper(s: &[u8]) -> bool {
    match s[0] {
        b'@' => {
            let end = s[1..]
                .iter()
                .position(|c| matches!(c, b'.' | b'|' | b':'))
                .map_or(s.len(), |p| p + 1);
            MODIFIERS.contains(&&s[1..end])
        }
        b'[' | b'{' => true,
        _ => false,
    }
}

fn parse_object_path(path: &[u8]) -> ObjectPath<'_> {
    let mut r = ObjectPath {
        part: Cow::Borrowed(path),
        path: b"",
        pipe: b"",
        piped: false,
        wild: false,
        more: false,
    };
    let mut i = 0;
    while i < path.len() {
        match path[i] {
            b'|' => {
                r.part = Cow::Borrowed(&path[..i]);
                r.pipe = &path[i + 1..];
                r.piped = true;
                return r;
            }
            b'.' => {
                r.part = Cow::Borrowed(&path[..i]);
                if i < path.len() - 1 && is_dot_piper(&path[i + 1..]) {
                    r.pipe = &path[i + 1..];
                    r.piped = true;
                } else {
                    r.path = &path[i + 1..];
                    r.more = true;
                }
                return r;
            }
            b'*' | b'?' => r.wild = true,
            b'\\' => {
                let mut epart = path[..i].to_vec();
                i += 1;
                if i < path.len() {
                    epart.push(path[i]);
                    i += 1;
                    while i < path.len() {
                        match path[i] {
                            b'\\' => {
                                i += 1;
                                if i < path.len() {
                                    epart.push(path[i]);
                                }
                                i += 1;
                                continue;
                            }
                            b'.' => {
                                r.part = Cow::Owned(epart);
                                if i < path.len() - 1 && is_dot_piper(&path[i + 1..]) {
                                    r.pipe = &path[i + 1..];
                                    r.piped = true;
                                } else {
                                    r.path = &path[i + 1..];
                                    r.more = true;
                                }
                                return r;
                            }
                            b'|' => {
                                r.part = Cow::Owned(epart);
                                r.pipe = &path[i + 1..];
                                r.piped = true;
                                return r;
                            }
                            b'*' | b'?' => r.wild = true,
                            _ => {}
                        }
                        epart.push(path[i]);
                        i += 1;
                    }
                }
                r.part = Cow::Owned(epart);
                return r;
            }
            _ => {}
        }
        i += 1;
    }
    r
}

fn parse_object<'a>(c: &mut Ctx<'a>, mut i: usize, path: &[u8]) -> (usize, bool) {
    let rp = parse_object_path(path);
    if !rp.more && rp.piped {
        c.pipe = Some(rp.pipe.to_vec());
    }
    let json = c.json;
    while i < json.len() {
        let mut key: (usize, usize) = (0, 0);
        let mut kesc = false;
        let mut ok = false;
        while i < json.len() {
            if json[i] == b'"' {
                i += 1;
                let s = i;
                let mut found = false;
                while i < json.len() {
                    if json[i] > b'\\' {
                        i += 1;
                        continue;
                    }
                    if json[i] == b'"' {
                        key = (s, i);
                        i += 1;
                        ok = true;
                        found = true;
                        break;
                    }
                    if json[i] == b'\\' {
                        i += 1;
                        while i < json.len() {
                            if json[i] > b'\\' {
                                i += 1;
                                continue;
                            }
                            if json[i] == b'"' {
                                if json[i - 1] == b'\\' && backslashes_before(json, i, 1).is_multiple_of(2) {
                                    i += 1;
                                    continue;
                                }
                                key = (s, i);
                                i += 1;
                                kesc = true;
                                ok = true;
                                found = true;
                                break;
                            }
                            i += 1;
                        }
                        break;
                    }
                    i += 1;
                }
                if !found {
                    key = (s, json.len());
                    kesc = false;
                    ok = false;
                }
                break;
            }
            if json[i] == b'}' {
                return (i + 1, false);
            }
            i += 1;
        }
        if !ok {
            return (i, false);
        }
        let raw_key = &json[key.0..key.1];
        let key_text: Cow<'_, [u8]> = if kesc {
            Cow::Owned(unescape(raw_key))
        } else {
            Cow::Borrowed(raw_key)
        };
        let pmatch = if rp.wild {
            match_limit(&key_text, &rp.part)
        } else {
            key_text == rp.part
        };
        let mut hit = pmatch && !rp.more;
        while i < json.len() {
            let mut num = false;
            match json[i] {
                b'"' => {
                    let start = i;
                    let (next, end, esc, ok) = parse_string(json, i + 1);
                    i = next;
                    if !ok {
                        return (i, false);
                    }
                    if hit {
                        c.value = string_res(json, start, end, esc);
                        return (i, true);
                    }
                }
                b'{' | b'[' => {
                    if pmatch && !hit {
                        let (next, h) = if json[i] == b'{' {
                            parse_object(c, i + 1, rp.path)
                        } else {
                            parse_array(c, i + 1, rp.path)
                        };
                        i = next;
                        hit = h;
                        if hit {
                            return (i, true);
                        }
                    } else {
                        let start = i;
                        let (next, end) = parse_squash(json, i);
                        i = next;
                        if hit {
                            c.value = json_res(json, start, end);
                            return (i, true);
                        }
                    }
                }
                b'n' if i + 1 < json.len() && json[i + 1] != b'u' => num = true,
                b'n' | b't' | b'f' => {
                    let start = i;
                    let (next, end) = parse_literal(json, i);
                    i = next;
                    if hit {
                        c.value = literal_res(json, start, end);
                        return (i, true);
                    }
                }
                b'+' | b'-' | b'0'..=b'9' | b'i' | b'I' | b'N' => num = true,
                _ => {
                    i += 1;
                    continue;
                }
            }
            if num {
                let start = i;
                let (next, end) = parse_number(json, i);
                i = next;
                if hit {
                    c.value = number_res(json, start, end);
                    return (i, true);
                }
            }
            break;
        }
    }
    (i, false)
}

struct ArrayPath<'p> {
    part: &'p [u8],
    path: &'p [u8],
    pipe: &'p [u8],
    piped: bool,
    more: bool,
    alogok: bool,
    arrch: bool,
    alogkey: &'p [u8],
}

fn parse_array_path(path: &[u8]) -> ArrayPath<'_> {
    let mut r = ArrayPath {
        part: path,
        path: b"",
        pipe: b"",
        piped: false,
        more: false,
        alogok: false,
        arrch: false,
        alogkey: b"",
    };
    for i in 0..path.len() {
        match path[i] {
            b'|' => {
                r.part = &path[..i];
                r.pipe = &path[i + 1..];
                r.piped = true;
                return r;
            }
            b'.' => {
                r.part = &path[..i];
                if !r.arrch && i < path.len() - 1 && is_dot_piper(&path[i + 1..]) {
                    r.pipe = &path[i + 1..];
                    r.piped = true;
                } else {
                    r.path = &path[i + 1..];
                    r.more = true;
                }
                return r;
            }
            b'#' => {
                r.arrch = true;
                if i == 0 && path.len() > 1 && path[1] == b'.' {
                    r.alogok = true;
                    r.alogkey = &path[2..];
                    r.path = &path[..1];
                }
            }
            _ => {}
        }
    }
    r.part = path;
    r.path = b"";
    r
}

fn split_possible_pipe(path: &[u8]) -> Option<(&[u8], &[u8])> {
    if !path.contains(&b'|') || path.first() == Some(&b'{') {
        return None;
    }
    let mut i = 0;
    while i < path.len() {
        match path[i] {
            b'\\' => i += 1,
            b'.' => {
                if i == path.len() - 1 {
                    return None;
                }
                if path[i + 1] == b'#' {
                    i += 2;
                    if i == path.len() {
                        return None;
                    }
                }
            }
            b'|' => return Some((&path[..i], &path[i + 1..])),
            _ => {}
        }
        i += 1;
    }
    None
}

fn parse_array<'a>(c: &mut Ctx<'a>, mut i: usize, path: &[u8]) -> (usize, bool) {
    let mut rp = parse_array_path(path);
    let json = c.json;
    let partidx: isize = if rp.arrch {
        0
    } else {
        parse_uint(rp.part).map_or(-1, |n| n as isize)
    };
    if !rp.more && rp.piped {
        c.pipe = Some(rp.pipe.to_vec());
    }
    let mut h: isize = 0;
    let mut alog: Vec<usize> = vec![];
    let mut pmatch = false;
    let mut hit = false;
    while i < json.len() + 1 {
        if !rp.arrch {
            pmatch = partidx == h;
            hit = pmatch && !rp.more;
        }
        h += 1;
        if rp.alogok {
            alog.push(i);
        }
        loop {
            let ch = if i > json.len() {
                break;
            } else if i == json.len() {
                b']'
            } else {
                json[i]
            };
            let mut num = false;
            match ch {
                b'"' => {
                    let start = i;
                    let (next, end, esc, ok) = parse_string(json, i + 1);
                    i = next;
                    if !ok {
                        return (i, false);
                    }
                    if hit && !rp.alogok {
                        c.value = string_res(json, start, end, esc);
                        return (i, true);
                    }
                }
                b'{' | b'[' => {
                    if pmatch && !hit {
                        let (next, h2) = if ch == b'{' {
                            parse_object(c, i + 1, rp.path)
                        } else {
                            parse_array(c, i + 1, rp.path)
                        };
                        i = next;
                        hit = h2;
                        if hit && !rp.alogok {
                            return (i, true);
                        }
                    } else {
                        let start = i;
                        let (next, end) = parse_squash(json, i);
                        i = next;
                        if hit && !rp.alogok {
                            c.value = json_res(json, start, end);
                            return (i, true);
                        }
                    }
                }
                b'n' if i + 1 < json.len() && json[i + 1] != b'u' => num = true,
                b'n' | b't' | b'f' => {
                    let start = i;
                    let (next, end) = parse_literal(json, i);
                    i = next;
                    if hit && !rp.alogok {
                        c.value = literal_res(json, start, end);
                        return (i, true);
                    }
                }
                b'+' | b'-' | b'0'..=b'9' | b'i' | b'I' | b'N' => num = true,
                b']' => {
                    if rp.arrch && rp.part == b"#" {
                        if rp.alogok {
                            if let Some((left, right)) = split_possible_pipe(rp.alogkey) {
                                rp.alogkey = left;
                                c.pipe = Some(right.to_vec());
                            }
                            let key = String::from_utf8_lossy(rp.alogkey).into_owned();
                            let mut indexes = vec![];
                            let mut out = vec![b'['];
                            for &start in &alog {
                                let mut idx = start;
                                while idx < json.len() && matches!(json[idx], b' ' | b'\t' | b'\r' | b'\n') {
                                    idx += 1;
                                }
                                if idx < json.len() && json[idx] != b']' {
                                    let (_, res, ok) = parse_any(json, idx, true);
                                    if ok {
                                        let res = res.get(&key);
                                        if res.exists() {
                                            if !indexes.is_empty() {
                                                out.push(b',');
                                            }
                                            if res.raw.is_empty() {
                                                out.extend_from_slice(&res.bytes());
                                            } else {
                                                out.extend_from_slice(&res.raw);
                                            }
                                            indexes.push(res.index);
                                        }
                                    }
                                }
                            }
                            out.push(b']');
                            c.value = Res {
                                kind: Kind::Json,
                                raw: Cow::Owned(out),
                                indexes: Some(indexes),
                                ..Res::default()
                            };
                            return (i + 1, true);
                        }
                        c.value = Res {
                            kind: Kind::Number,
                            raw: Cow::Owned((h - 1).to_string().into_bytes()),
                            num: (h - 1) as f64,
                            ..Res::default()
                        };
                        return (i + 1, true);
                    }
                    return (i + 1, false);
                }
                _ => {
                    i += 1;
                    continue;
                }
            }
            if num {
                let start = i;
                let (next, end) = parse_number(json, i);
                i = next;
                if hit && !rp.alogok {
                    c.value = number_res(json, start, end);
                    return (i, true);
                }
            }
            break;
        }
    }
    (i, false)
}

/// gjson.Get.
pub fn get<'a>(json: &'a [u8], path: &(impl JsonPath + ?Sized)) -> Res<'a> {
    let path = path.as_path();
    if path.len() > 1 && path[0] == b'@' && is_dot_piper(path) {
        let end = path[1..]
            .iter()
            .position(|c| matches!(c, b'.' | b'|' | b':'))
            .map_or(path.len(), |p| p + 1);
        if &path[1..end] != b"this" || path.get(end) == Some(&b':') {
            return Res::default();
        }
        if end < path.len() {
            let mut res = get(json, &path[end + 1..]);
            res.index = 0;
            res.indexes = None;
            return res;
        }
        return parse(json);
    }
    let mut c = Ctx {
        json,
        value: Res::default(),
        pipe: None,
    };
    if let Some(i) = json.iter().position(|&b| b == b'{' || b == b'[') {
        if json[i] == b'{' {
            parse_object(&mut c, i + 1, path);
        } else {
            parse_array(&mut c, i + 1, path);
        }
    }
    if let Some(pipe) = c.pipe {
        // Go pipes before fillIndex, while the left-hand value's index is still zero.
        c.value.index = 0;
        let mut res = c.value.get(&pipe);
        res.index = 0;
        return res;
    }
    c.value
}

/// The members of a JSON object that [`valid`] accepted, in document order, as borrowed
/// raw slices: the key without its quotes and whether it holds escapes, then the value
/// (a string keeps its quotes). Nothing is decoded or allocated; `f` returning false
/// stops the walk. Anything but an object visits nothing.
pub fn object_members<'a>(json: &'a [u8], mut f: impl FnMut(&'a [u8], bool, &'a [u8]) -> bool) {
    let ws = |json: &[u8], mut i: usize| {
        while i < json.len() && json[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    let mut i = ws(json, 0);
    if json.get(i) != Some(&b'{') {
        return;
    }
    i += 1;
    loop {
        i = ws(json, i);
        if json.get(i) != Some(&b'"') {
            return;
        }
        let (next, end, esc, ok) = parse_string(json, i + 1);
        if !ok {
            return;
        }
        let key = &json[i + 1..end - 1];
        i = ws(json, next);
        if json.get(i) != Some(&b':') {
            return;
        }
        let start = ws(json, i + 1);
        let end = match json.get(start) {
            Some(b'{' | b'[') => parse_squash(json, start).1,
            Some(b'"') => parse_string(json, start + 1).1,
            Some(_) => {
                let mut e = start;
                while e < json.len() && !matches!(json[e], b',' | b'}' | b']') && !json[e].is_ascii_whitespace() {
                    e += 1;
                }
                e
            }
            None => return,
        };
        if !f(key, esc, &json[start..end]) {
            return;
        }
        i = ws(json, end);
        if json.get(i) != Some(&b',') {
            return;
        }
        i += 1;
    }
}

/// gjson.Parse: the first value, with containers taking the rest of the input as raw.
pub fn parse(json: &[u8]) -> Res<'_> {
    let mut i = 0;
    while i < json.len() {
        let c = json[i];
        if c == b'{' || c == b'[' {
            return json_res(json, i, json.len());
        }
        if c <= b' ' {
            i += 1;
            continue;
        }
        let rest = &json[i..];
        let mut value = match c {
            b'+' | b'-' | b'0'..=b'9' | b'i' | b'I' | b'N' => number_res(rest, 0, tonum(rest).len()),
            b'n' if i + 1 < json.len() && json[i + 1] != b'u' => number_res(rest, 0, tonum(rest).len()),
            b'n' | b't' | b'f' => literal_res(rest, 0, tolit(rest).len()),
            b'"' => {
                let (raw, s) = tostr(rest);
                Res {
                    kind: Kind::String,
                    raw: Cow::Borrowed(raw),
                    s,
                    ..Res::default()
                }
            }
            _ => return Res::default(),
        };
        value.index = i;
        return value;
    }
    Res::default()
}

/// gjson.Valid.
/// encoding/json `Valid`: gjson's grammar check plus encoding/json's nesting limit of
/// 10000 arrays and objects.
pub fn std_valid(data: &[u8]) -> bool {
    if !valid(data) {
        return false;
    }
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for &c in data {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > 10_000 {
                    return false;
                }
            }
            b'}' | b']' => depth -= 1,
            _ => {}
        }
    }
    true
}

/// The first index at or after `i` holding a byte a JSON string scan must look at (a
/// control character, `"` or `\\`), or `d.len()`: memchr2 finds the next `"` or `\\`,
/// then one vectorised pass looks for a control character before it.
fn skip_plain(d: &[u8], i: usize) -> usize {
    let rest = &d[i.min(d.len())..];
    let end = memchr::memchr2(b'"', b'\\', rest).unwrap_or(rest.len());
    let span = &rest[..end];
    // No early exit, so the check vectorises; the position is only needed on failure.
    if span.iter().fold(false, |any, &b| any | (b < b' ')) {
        return i + span.iter().position(|&b| b < b' ').unwrap_or(end);
    }
    i + end
}

pub fn valid(data: &[u8]) -> bool {
    fn ws(c: u8) -> bool {
        matches!(c, b' ' | b'\t' | b'\n' | b'\r')
    }
    /// One value starting at or after `i` (leading white space allowed). Containers are
    /// tracked on an explicit stack: gjson recurses, which Go's growable stacks absorb but
    /// a fixed native stack does not.
    fn any(d: &[u8], mut i: usize) -> Option<usize> {
        #[derive(Clone, Copy)]
        enum Container {
            Object,
            Array,
        }
        let mut stack: Vec<Container> = vec![];
        'value: loop {
            while i < d.len() && ws(d[i]) {
                i += 1;
            }
            i = match *d.get(i)? {
                b'{' => {
                    let at = skip_to(d, i + 1, b"}\"")?;
                    if d[at] == b'}' {
                        at + 1
                    } else {
                        stack.push(Container::Object);
                        i = string(d, at + 1)?;
                        i = skip_to(d, i, b":")? + 1;
                        continue 'value;
                    }
                }
                b'[' => {
                    let mut at = i + 1;
                    while at < d.len() && ws(d[at]) {
                        at += 1;
                    }
                    if *d.get(at)? == b']' {
                        at + 1
                    } else {
                        stack.push(Container::Array);
                        i = at;
                        continue 'value;
                    }
                }
                b'"' => string(d, i + 1)?,
                b'-' | b'0'..=b'9' => number(d, i + 1)?,
                b't' => lit(d, i + 1, b"rue")?,
                b'f' => lit(d, i + 1, b"alse")?,
                b'n' => lit(d, i + 1, b"ull")?,
                _ => return None,
            };
            // A value ended at `i`: close every container it completes.
            loop {
                match stack.last() {
                    None => return Some(i),
                    Some(Container::Object) => {
                        i = skip_to(d, i, b",}")?;
                        if d[i] == b'}' {
                            i += 1;
                            stack.pop();
                            continue;
                        }
                        i = skip_to(d, i + 1, b"\"")?;
                        i = string(d, i + 1)?;
                        i = skip_to(d, i, b":")? + 1;
                        continue 'value;
                    }
                    Some(Container::Array) => {
                        i = skip_to(d, i, b",]")?;
                        if d[i] == b']' {
                            i += 1;
                            stack.pop();
                            continue;
                        }
                        i += 1;
                        continue 'value;
                    }
                }
            }
        }
    }
    fn lit(d: &[u8], i: usize, rest: &[u8]) -> Option<usize> {
        d.get(i..i + rest.len()).filter(|s| *s == rest).map(|_| i + rest.len())
    }
    fn skip_to(d: &[u8], mut i: usize, want: &[u8]) -> Option<usize> {
        while i < d.len() {
            if ws(d[i]) {
                i += 1;
            } else if want.contains(&d[i]) {
                return Some(i);
            } else {
                return None;
            }
        }
        None
    }
    fn string(d: &[u8], mut i: usize) -> Option<usize> {
        while i < d.len() {
            i = skip_plain(d, i);
            let Some(&c) = d.get(i) else { break };
            match c {
                c if c < b' ' => return None,
                b'\\' => {
                    i += 1;
                    match *d.get(i)? {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {}
                        b'u' => {
                            for _ in 0..4 {
                                i += 1;
                                if !d.get(i)?.is_ascii_hexdigit() {
                                    return None;
                                }
                            }
                        }
                        _ => return None,
                    }
                }
                b'"' => return Some(i + 1),
                _ => {}
            }
            i += 1;
        }
        None
    }
    fn number(d: &[u8], i: usize) -> Option<usize> {
        let mut i = i - 1;
        if d[i] == b'-' {
            i += 1;
            if !d.get(i)?.is_ascii_digit() {
                return None;
            }
        }
        if d[i] == b'0' {
            i += 1;
        } else {
            while i < d.len() && d[i].is_ascii_digit() {
                i += 1;
            }
        }
        if i == d.len() {
            return Some(i);
        }
        if d[i] == b'.' {
            i += 1;
            if !d.get(i)?.is_ascii_digit() {
                return None;
            }
            while i < d.len() && d[i].is_ascii_digit() {
                i += 1;
            }
        }
        if i == d.len() {
            return Some(i);
        }
        if d[i] == b'e' || d[i] == b'E' {
            i += 1;
            if matches!(d.get(i), Some(b'+' | b'-')) {
                i += 1;
            }
            if !d.get(i)?.is_ascii_digit() {
                return None;
            }
            while i < d.len() && d[i].is_ascii_digit() {
                i += 1;
            }
        }
        Some(i)
    }
    let mut i = 0;
    while i < data.len() && ws(data[i]) {
        i += 1;
    }
    if i == data.len() {
        return false;
    }
    match any(data, i) {
        Some(end) => data[end..].iter().all(|&c| ws(c)),
        None => false,
    }
}

/// utf8.DecodeRune: `(rune, size)`, `(None, 1)` for an invalid sequence, `(None, 0)` at
/// end of input.
pub fn decode_rune(s: &[u8]) -> (Option<char>, usize) {
    if s.is_empty() {
        return (None, 0);
    }
    let n = match s[0] {
        0x00..=0x7F => return (Some(s[0] as char), 1),
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return (None, 1),
    };
    match s.get(..n).map(std::str::from_utf8) {
        Some(Ok(text)) => (text.chars().next(), n),
        _ => (None, 1),
    }
}

/// Go's rune view: invalid bytes decode as U+FFFD with size 1, empty input as size 0.
fn rune(s: &[u8]) -> (char, usize) {
    match decode_rune(s) {
        (Some(c), n) => (c, n),
        (None, 0) => ('\0', 0),
        (None, n) => ('\u{FFFD}', n),
    }
}

fn last_rune(s: &[u8]) -> (char, usize) {
    if s.is_empty() {
        return ('\u{FFFD}', 0);
    }
    let start = s.len().saturating_sub(4);
    for i in (start..s.len()).rev() {
        if s[i] & 0xC0 != 0x80 {
            return match decode_rune(&s[i..]) {
                (Some(c), n) if i + n == s.len() => (c, n),
                _ => ('\u{FFFD}', 1),
            };
        }
    }
    ('\u{FFFD}', 1)
}

/// tidwall/match.MatchLimit with gjson's limit of 10000 comparisons per input byte.
fn match_limit(s: &[u8], pattern: &[u8]) -> bool {
    #[derive(PartialEq)]
    enum R {
        No,
        Match,
        Stop,
    }
    fn trim_suffix<'s, 'p>(mut s: &'s [u8], mut pat: &'p [u8]) -> (&'s [u8], &'p [u8], bool) {
        let mut matched = true;
        while !s.is_empty() && pat.len() > 1 {
            let (pc, mut ps) = last_rune(pat);
            let mut esc = false;
            let mut i = 0;
            loop {
                if pat[pat.len() - ps - i - 1] != b'\\' {
                    if i & 1 == 1 {
                        esc = true;
                        ps += 1;
                    }
                    break;
                }
                i += 1;
            }
            if pc == '*' && !esc {
                matched = true;
                break;
            }
            let (sc, ss) = last_rune(s);
            if !((pc == '?' && !esc) || pc == sc) {
                matched = false;
                break;
            }
            s = &s[..s.len() - ss];
            pat = &pat[..pat.len() - ps];
        }
        (s, pat, matched)
    }
    fn go(mut s: &[u8], mut pat: &[u8], slen: usize, counter: &mut usize) -> R {
        if *counter > slen * 10000 {
            return R::Stop;
        }
        *counter += 1;
        while !pat.is_empty() {
            let mut wild = false;
            let (mut pc, mut ps) = rune(pat);
            let (sc, ss) = rune(s);
            match pc {
                '?' => {
                    if ss == 0 {
                        return R::No;
                    }
                }
                '*' => {
                    while pat.len() > 1 && pat[1] == b'*' {
                        pat = &pat[1..];
                    }
                    if pat.len() == 1 {
                        return R::Match;
                    }
                    let (s2, p2, ok) = trim_suffix(s, pat);
                    if !ok {
                        return R::No;
                    }
                    s = s2;
                    pat = p2;
                    if pat.len() == 1 {
                        return R::Match;
                    }
                    let r = go(s, &pat[1..], slen, counter);
                    if r != R::No {
                        return r;
                    }
                    if s.is_empty() {
                        return R::No;
                    }
                    wild = true;
                }
                _ => {
                    if ss == 0 {
                        return R::No;
                    }
                    if pc == '\\' {
                        pat = &pat[ps..];
                        (pc, ps) = rune(pat);
                        if ps == 0 {
                            return R::No;
                        }
                    }
                    if sc != pc {
                        return R::No;
                    }
                }
            }
            s = &s[ss..];
            if !wild {
                pat = &pat[ps..];
            }
        }
        if s.is_empty() { R::Match } else { R::No }
    }
    if pattern == b"*" {
        return true;
    }
    let mut counter = 0;
    go(s, pattern, s.len(), &mut counter) == R::Match
}

// ---------------------------------------------------------------------------------------
// Encoding

const HEX: &[u8; 16] = b"0123456789abcdef";

/// encoding/json string encoding. `html` escapes `<`, `>` and `&` like `json.Marshal`;
/// without it this matches an Encoder with `SetEscapeHTML(false)`.
pub fn marshal_str(dst: &mut Vec<u8>, s: &[u8], html: bool) {
    dst.push(b'"');
    let mut start = 0;
    let mut i = 0;
    while i < s.len() {
        let b = s[i];
        if b < 0x80 {
            let safe = b >= 0x20 && b != b'"' && b != b'\\' && !(html && matches!(b, b'<' | b'>' | b'&'));
            if safe {
                i += 1;
                continue;
            }
            dst.extend_from_slice(&s[start..i]);
            match b {
                b'\\' | b'"' => dst.extend_from_slice(&[b'\\', b]),
                8 => dst.extend_from_slice(b"\\b"),
                12 => dst.extend_from_slice(b"\\f"),
                b'\n' => dst.extend_from_slice(b"\\n"),
                b'\r' => dst.extend_from_slice(b"\\r"),
                b'\t' => dst.extend_from_slice(b"\\t"),
                _ => dst.extend_from_slice(&[
                    b'\\',
                    b'u',
                    b'0',
                    b'0',
                    HEX[usize::from(b >> 4)],
                    HEX[usize::from(b & 15)],
                ]),
            }
            i += 1;
            start = i;
            continue;
        }
        let (c, size) = decode_rune(&s[i..]);
        match c {
            None => {
                dst.extend_from_slice(&s[start..i]);
                dst.extend_from_slice(b"\\ufffd");
                i += size;
                start = i;
            }
            Some(c @ ('\u{2028}' | '\u{2029}')) => {
                dst.extend_from_slice(&s[start..i]);
                dst.extend_from_slice(b"\\u202");
                dst.push(HEX[(c as usize) & 15]);
                i += size;
                start = i;
            }
            Some(_) => i += size,
        }
    }
    dst.extend_from_slice(&s[start..]);
    dst.push(b'"');
}

/// encoding/json's compact as `json.Marshal` applies it to `RawMessage` values: drops
/// whitespace outside strings and, with `html`, escapes `<`, `>`, `&`, U+2028 and U+2029.
/// Input must already be valid JSON.
pub fn compact(src: &[u8], html: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut i = 0;
    while i < src.len() {
        let c = src[i];
        if html && matches!(c, b'<' | b'>' | b'&') {
            out.extend_from_slice(&[
                b'\\',
                b'u',
                b'0',
                b'0',
                HEX[usize::from(c >> 4)],
                HEX[usize::from(c & 15)],
            ]);
            i += 1;
            continue;
        }
        if html && c == 0xE2 && i + 2 < src.len() && src[i + 1] == 0x80 && src[i + 2] & !1 == 0xA8 {
            out.extend_from_slice(b"\\u202");
            out.push(HEX[usize::from(src[i + 2] & 15)]);
            i += 3;
            continue;
        }
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            out.push(c);
        } else if c == b'"' {
            in_string = true;
            out.push(c);
        } else if !matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
            out.push(c);
        }
        i += 1;
    }
    out
}

/// encoding/json's decoding of one raw string token (decode.go unquoteBytes): invalid
/// UTF-8 becomes U+FFFD per byte, and a `\u` surrogate consumes the next escape only when
/// the two form a valid pair. `None` for a token Go rejects. Use this rather than
/// [`Res::s`] (gjson's decoding) wherever Go decodes with encoding/json.
pub fn go_unquote(raw: &[u8]) -> Option<String> {
    let s = raw.strip_prefix(b"\"")?.strip_suffix(b"\"")?;
    let u4 = |s: &[u8]| -> Option<u32> {
        let hex = s.strip_prefix(b"\\u")?.get(..4)?;
        if !hex.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        u32::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()
    };
    let mut out = String::with_capacity(s.len());
    let mut r = 0;
    while r < s.len() {
        let c = s[r];
        if c == b'\\' {
            let escaped = *s.get(r + 1)?;
            match escaped {
                b'"' | b'\\' | b'/' | b'\'' => out.push(escaped as char),
                b'b' => out.push('\u{8}'),
                b'f' => out.push('\u{c}'),
                b'n' => out.push('\n'),
                b'r' => out.push('\r'),
                b't' => out.push('\t'),
                b'u' => {
                    let mut rune = u4(&s[r..])?;
                    r += 6;
                    if (0xD800..0xE000).contains(&rune) {
                        if let Some(low) = u4(&s[r..])
                            && (0xD800..0xDC00).contains(&rune)
                            && (0xDC00..0xE000).contains(&low)
                        {
                            r += 6;
                            out.push(char::from_u32(0x10000 + ((rune - 0xD800) << 10) + (low - 0xDC00))?);
                            continue;
                        }
                        rune = 0xFFFD;
                    }
                    out.push(char::from_u32(rune).unwrap_or('\u{FFFD}'));
                    continue;
                }
                _ => return None,
            }
            r += 2;
        } else if c == b'"' || c < b' ' {
            return None;
        } else {
            let (ch, n) = decode_rune(&s[r..]);
            out.push(ch.unwrap_or('\u{FFFD}'));
            r += n;
        }
    }
    Some(out)
}

/// `json.Marshal(string)`.
pub fn quote(s: impl AsRef<[u8]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.as_ref().len() + 2);
    marshal_str(&mut out, s.as_ref(), true);
    out
}

/// `json.Marshal([]string)`; an empty slice is `[]` (Go's nil slice would be `null`).
pub fn quote_all<S: AsRef<[u8]>>(items: &[S]) -> Vec<u8> {
    let mut out = vec![b'['];
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        marshal_str(&mut out, item.as_ref(), true);
    }
    out.push(b']');
    out
}

// ---------------------------------------------------------------------------------------
// Editing (sjson)

struct PathPart {
    part: Vec<u8>,
    gpart: Vec<u8>,
    force: bool,
}

fn simple_char(c: u8) -> bool {
    !matches!(c, b'|' | b'#' | b'@' | b'*' | b'?')
}

/// sjson.parsePath for the whole path; `None` when any component is not simple.
fn split_path(path: &[u8]) -> Option<Vec<PathPart>> {
    let mut parts = vec![];
    let mut path = path;
    loop {
        let mut r = PathPart {
            part: vec![],
            gpart: vec![],
            force: false,
        };
        if path.first() == Some(&b':') {
            r.force = true;
            path = &path[1..];
        }
        let mut more = None;
        let mut i = 0;
        let mut done = false;
        while i < path.len() {
            if path[i] == b'.' {
                r.part = path[..i].to_vec();
                r.gpart = path[..i].to_vec();
                more = Some(&path[i + 1..]);
                done = true;
                break;
            }
            if !simple_char(path[i]) {
                return None;
            }
            if path[i] == b'\\' {
                let mut epart = path[..i].to_vec();
                let mut gpart = path[..i + 1].to_vec();
                i += 1;
                if i < path.len() {
                    epart.push(path[i]);
                    gpart.push(path[i]);
                    i += 1;
                    while i < path.len() {
                        if path[i] == b'\\' {
                            gpart.push(b'\\');
                            i += 1;
                            if i < path.len() {
                                epart.push(path[i]);
                                gpart.push(path[i]);
                            }
                            i += 1;
                            continue;
                        } else if path[i] == b'.' {
                            more = Some(&path[i + 1..]);
                            break;
                        } else if !simple_char(path[i]) {
                            return None;
                        }
                        epart.push(path[i]);
                        gpart.push(path[i]);
                        i += 1;
                    }
                }
                r.part = epart;
                r.gpart = gpart;
                done = true;
                break;
            }
            i += 1;
        }
        if !done {
            r.part = path.to_vec();
            r.gpart = path.to_vec();
        }
        parts.push(r);
        match more {
            Some(rest) => path = rest,
            None => return Some(parts),
        }
    }
}

fn must_marshal(s: &[u8]) -> bool {
    s.iter()
        .any(|&c| !(b' '..=0x7f).contains(&c) || c == b'"' || c == b'\\')
}

fn append_stringify(buf: &mut Vec<u8>, s: &[u8]) {
    if must_marshal(s) {
        marshal_str(buf, s, true);
    } else {
        buf.push(b'"');
        buf.extend_from_slice(s);
        buf.push(b'"');
    }
}

/// sjson's atoui accumulates into Go's signed int, so huge indexes wrap negative.
fn atoui(p: &PathPart) -> (isize, bool) {
    if p.force || !p.part.iter().all(u8::is_ascii_digit) {
        return (0, false);
    }
    (
        p.part
            .iter()
            .fold(0isize, |n, c| n.wrapping_mul(10).wrapping_add(isize::from(c - b'0'))),
        true,
    )
}

/// appendRepeat: a negative count repeats nothing.
fn repeat(buf: &mut Vec<u8>, s: &[u8], n: isize) {
    for _ in 0..n.max(0) {
        buf.extend_from_slice(s);
    }
}

fn append_build(buf: &mut Vec<u8>, array: bool, paths: &[PathPart], raw: &[u8], stringify: bool) {
    if !array {
        append_stringify(buf, &paths[0].part);
        buf.push(b':');
    }
    if paths.len() > 1 {
        let (n, numeric) = atoui(&paths[1]);
        if numeric || (!paths[1].force && paths[1].part == b"-1") {
            buf.push(b'[');
            repeat(buf, b"null,", n);
            append_build(buf, true, &paths[1..], raw, stringify);
            buf.push(b']');
        } else {
            buf.push(b'{');
            append_build(buf, false, &paths[1..], raw, stringify);
            buf.push(b'}');
        }
    } else if stringify {
        append_stringify(buf, raw);
    } else {
        buf.extend_from_slice(raw);
    }
}

fn delete_tail_item(buf: &mut Vec<u8>) -> bool {
    let mut i = buf.len() as isize - 1;
    while i >= 0 {
        match buf[i as usize] {
            b'[' => return true,
            b',' => {
                buf.truncate(i as usize);
                return false;
            }
            b':' => {
                i -= 1;
                while i >= 0 {
                    if buf[i as usize] == b'"' {
                        i -= 1;
                        while i >= 0 {
                            if buf[i as usize] == b'"' {
                                i -= 1;
                                if i >= 0 && buf[i as usize] == b'\\' {
                                    // Go's `continue` also runs the loop's `i--`.
                                    i -= 2;
                                    continue;
                                }
                                while i >= 0 {
                                    match buf[i as usize] {
                                        b'{' => {
                                            buf.truncate(i as usize + 1);
                                            return true;
                                        }
                                        b',' => {
                                            buf.truncate(i as usize);
                                            return false;
                                        }
                                        _ => {}
                                    }
                                    i -= 1;
                                }
                            }
                            i -= 1;
                        }
                        break;
                    }
                    i -= 1;
                }
                return false;
            }
            _ => {}
        }
        i -= 1;
    }
    false
}

enum SetError {
    NoChange,
    /// An sjson error, with sjson's message.
    Invalid(String),
}

fn append_raw_paths(
    buf: &mut Vec<u8>,
    jstr: &[u8],
    paths: &[PathPart],
    raw: &[u8],
    stringify: bool,
    del: bool,
) -> Result<(), SetError> {
    let mut res = None;
    if del && paths[0].part == b"-1" && !paths[0].force {
        let count = get(jstr, "#");
        if count.int() > 0 {
            res = Some(get(jstr, &(count.int() - 1).to_string()));
        }
    }
    let res = res.unwrap_or_else(|| get(jstr, &paths[0].gpart));
    if res.index > 0 {
        let end = res.index + res.raw.len();
        if paths.len() > 1 {
            buf.extend_from_slice(&jstr[..res.index]);
            append_raw_paths(buf, &res.raw, &paths[1..], raw, stringify, del)?;
            buf.extend_from_slice(&jstr[end..]);
            return Ok(());
        }
        buf.extend_from_slice(&jstr[..res.index]);
        let mut exidx = 0;
        if del {
            if delete_tail_item(buf) {
                for (j, &c) in jstr[end..].iter().enumerate() {
                    if c <= b' ' {
                        continue;
                    }
                    if c == b',' {
                        exidx = j + 1;
                    }
                    break;
                }
            }
        } else if stringify {
            append_stringify(buf, raw);
        } else {
            buf.extend_from_slice(raw);
        }
        buf.extend_from_slice(&jstr[end + exidx..]);
        return Ok(());
    }
    if del {
        return Err(SetError::NoChange);
    }
    let (n, numeric) = atoui(&paths[0]);
    let mut jstr = jstr;
    if jstr.iter().all(|&c| c <= b' ') {
        jstr = if numeric { b"[]" } else { b"{}" };
    }
    let mut jsres = parse(jstr);
    if jsres.kind != Kind::Json {
        jstr = if numeric { b"[]" } else { b"{}" };
        jsres = parse(jstr);
    }
    let jraw = &jsres.raw[..];
    let comma = jraw[1..]
        .iter()
        .find(|&&c| c > b' ')
        .is_some_and(|&c| c != b'}' && c != b']');
    match jraw[0] {
        b'{' => {
            let end = (1..jraw.len()).rev().find(|&e| jraw[e] == b'}').unwrap_or(0);
            buf.extend_from_slice(&jraw[..end]);
            if comma {
                buf.push(b',');
            }
            append_build(buf, false, paths, raw, stringify);
            buf.push(b'}');
            Ok(())
        }
        _ => {
            if !numeric {
                if paths[0].part == b"-1" && !paths[0].force {
                    let trimmed = trim_ws(jraw);
                    let trimmed = trimmed.strip_suffix(b"]").unwrap_or(trimmed);
                    buf.extend_from_slice(trimmed);
                    if comma {
                        buf.push(b',');
                    }
                    append_build(buf, true, paths, raw, stringify);
                    buf.push(b']');
                    return Ok(());
                }
                return Err(SetError::Invalid(format!(
                    "cannot set array element for non-numeric key '{}'",
                    String::from_utf8_lossy(&paths[0].part)
                )));
            }
            buf.push(b'[');
            let items = jsres.array();
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    buf.push(b',');
                }
                buf.extend_from_slice(&item.raw);
            }
            if items.is_empty() {
                repeat(buf, b"null,", n);
            } else {
                repeat(buf, b",null", n.wrapping_sub(items.len() as isize));
                if comma {
                    buf.push(b',');
                }
            }
            append_build(buf, true, paths, raw, stringify);
            buf.push(b']');
            Ok(())
        }
    }
}

fn trim_ws(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&c| c > b' ').unwrap_or(s.len());
    let end = s.iter().rposition(|&c| c > b' ').map_or(start, |e| e + 1);
    &s[start..end]
}

fn set_impl(jstr: &[u8], path: &[u8], raw: &[u8], stringify: bool, del: bool) -> Result<Vec<u8>, SetError> {
    if path.is_empty() {
        return Err(SetError::Invalid("path cannot be empty".into()));
    }
    let Some(paths) = split_path(path) else {
        if del {
            return Err(SetError::Invalid("cannot delete value from a complex path".into()));
        }
        return set_complex(jstr, path, raw, stringify);
    };
    let mut buf = Vec::with_capacity(jstr.len() + raw.len() + path.len() + 8);
    append_raw_paths(&mut buf, jstr, &paths, raw, stringify, del)?;
    Ok(buf)
}

fn set_complex(jstr: &[u8], path: &[u8], raw: &[u8], stringify: bool) -> Result<Vec<u8>, SetError> {
    let res = get(jstr, path);
    if !res.exists() || (res.index == 0 && res.indexes.as_ref().is_none_or(Vec::is_empty)) {
        return Err(SetError::NoChange);
    }
    let mut out = jstr.to_vec();
    let replace = |out: &mut Vec<u8>, index: usize, len: usize| {
        let mut next = out[..index].to_vec();
        if stringify {
            append_stringify(&mut next, raw);
        } else {
            next.extend_from_slice(raw);
        }
        next.extend_from_slice(&out[index + len..]);
        *out = next;
    };
    if res.index != 0 {
        replace(&mut out, res.index, res.raw.len());
    }
    if let Some(indexes) = res.indexes.as_ref().filter(|i| !i.is_empty()) {
        let mut values = vec![];
        res.each(|_, v| {
            values.push(v.raw.len());
            true
        });
        if values.len() != indexes.len() {
            return Err(SetError::NoChange);
        }
        let mut pairs: Vec<(usize, usize)> = indexes.iter().copied().zip(values).collect();
        pairs.sort_by_key(|p| std::cmp::Reverse(p.0));
        for (index, len) in pairs {
            replace(&mut out, index, len);
        }
    }
    Ok(out)
}

/// Applies an edit in place. False exactly when sjson returns an error; a no-op edit (a
/// missing delete or complex-path target) is a success, as in Go.
fn apply(out: &mut Vec<u8>, path: &[u8], raw: &[u8], stringify: bool, del: bool) -> bool {
    match set_impl(out, path, raw, stringify, del) {
        Ok(next) => {
            *out = next;
            true
        }
        Err(SetError::NoChange) => true,
        Err(SetError::Invalid(_)) => false,
    }
}

fn checked(json: &[u8], path: &[u8], raw: &[u8], stringify: bool, del: bool) -> Result<Vec<u8>, String> {
    match set_impl(json, path, raw, stringify, del) {
        Ok(next) => Ok(next),
        Err(SetError::NoChange) => Ok(json.to_vec()),
        Err(SetError::Invalid(message)) => Err(message),
    }
}

/// `sjson.SetBytes` with a string value, returning sjson's error message.
pub fn try_set_str(json: &[u8], path: &(impl JsonPath + ?Sized), value: impl AsRef<[u8]>) -> Result<Vec<u8>, String> {
    let path = path.as_path();
    checked(json, path, value.as_ref(), true, false)
}

/// `sjson.SetRawBytes`, returning sjson's error message.
pub fn try_set_raw(json: &[u8], path: &(impl JsonPath + ?Sized), raw: impl AsRef<[u8]>) -> Result<Vec<u8>, String> {
    let path = path.as_path();
    checked(json, path, raw.as_ref(), false, false)
}

/// `sjson.DeleteBytes`, returning sjson's error message.
pub fn try_delete(json: &[u8], path: &(impl JsonPath + ?Sized)) -> Result<Vec<u8>, String> {
    let path = path.as_path();
    checked(json, path, b"", false, true)
}

/// `sjson.SetBytes(out, path, string)`. Returns false when sjson would return an error
/// (the document is then unchanged), true otherwise.
pub fn set_str(out: &mut Vec<u8>, path: &(impl JsonPath + ?Sized), value: impl AsRef<[u8]>) -> bool {
    let path = path.as_path();
    apply(out, path, value.as_ref(), true, false)
}

/// `sjson.SetRawBytes`.
pub fn set_raw(out: &mut Vec<u8>, path: &(impl JsonPath + ?Sized), raw: impl AsRef<[u8]>) -> bool {
    let path = path.as_path();
    apply(out, path, raw.as_ref(), false, false)
}

/// `sjson.SetBytes` with any Go integer type.
pub fn set_int(out: &mut Vec<u8>, path: &(impl JsonPath + ?Sized), value: i64) -> bool {
    let path = path.as_path();
    apply(out, path, value.to_string().as_bytes(), false, false)
}

/// `sjson.SetBytes` with a float64 (`strconv.FormatFloat(v, 'f', -1, 64)`).
pub fn set_f64(out: &mut Vec<u8>, path: &(impl JsonPath + ?Sized), value: f64) -> bool {
    let path = path.as_path();
    apply(out, path, fmt_float(value).as_bytes(), false, false)
}

pub fn set_bool(out: &mut Vec<u8>, path: &(impl JsonPath + ?Sized), value: bool) -> bool {
    let path = path.as_path();
    apply(out, path, if value { b"true" } else { b"false" }, false, false)
}

/// `sjson.SetBytes` with a `[]string`, which goes through json.Marshal and is therefore
/// always HTML-escaped.
pub fn set_strs<S: AsRef<[u8]>>(out: &mut Vec<u8>, path: &(impl JsonPath + ?Sized), items: &[S]) -> bool {
    let path = path.as_path();
    apply(out, path, &quote_all(items), false, false)
}

/// `sjson.DeleteBytes`.
pub fn delete(out: &mut Vec<u8>, path: &(impl JsonPath + ?Sized)) -> bool {
    let path = path.as_path();
    apply(out, path, b"", false, true)
}

/// common.SetStringWithoutHTMLEscape: an Encoder with HTML escaping off, set raw.
pub fn set_str_no_html(out: &mut Vec<u8>, path: &(impl JsonPath + ?Sized), value: impl AsRef<[u8]>) -> bool {
    let path = path.as_path();
    let mut raw = vec![];
    marshal_str(&mut raw, value.as_ref(), false);
    set_raw(out, path, raw)
}

/// common.JoinRawArray.
pub fn join<S: AsRef<[u8]>>(items: &[S]) -> Vec<u8> {
    let mut out = Vec::with_capacity(items.iter().map(|i| i.as_ref().len() + 1).sum::<usize>() + 2);
    out.push(b'[');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(item.as_ref());
    }
    out.push(b']');
    out
}

/// common.SetRawArrayItems: no-op for no items, an in-place fill of an existing `[]` for
/// one item, otherwise a raw set of the joined array.
pub fn set_items<S: AsRef<[u8]>>(out: &mut Vec<u8>, path: &(impl JsonPath + ?Sized), items: &[S]) {
    let path = path.as_path();
    if items.is_empty() {
        return;
    }
    if items.len() == 1 {
        let array = get(out, path);
        if &array.raw[..] == b"[]" && array.index + 2 <= out.len() {
            let index = array.index;
            let mut next = out[..index].to_vec();
            next.push(b'[');
            next.extend_from_slice(items[0].as_ref());
            next.push(b']');
            next.extend_from_slice(&out[index + 2..]);
            *out = next;
            return;
        }
    }
    set_raw(out, path, join(items));
}

// ---------------------------------------------------------------------------------------
// gjson's `any` model (`Result.Value()`)

/// What gjson's `Result.Value()` returns, as Go code then edits and marshals it: objects
/// keep the first duplicate key, strings keep gjson's unescaped bytes (invalid UTF-8
/// marshals as `\ufffd`), numbers are float64 and non-container JSON is nil.
#[derive(Debug, Clone, PartialEq)]
pub enum AnyValue {
    Null,
    Bool(bool),
    Number(f64),
    String(Vec<u8>),
    Array(Vec<AnyValue>),
    Object(std::collections::BTreeMap<Vec<u8>, AnyValue>),
}

impl AnyValue {
    pub fn from_res(r: &Res<'_>) -> Self {
        match r.kind {
            Kind::Null => Self::Null,
            Kind::True => Self::Bool(true),
            Kind::False => Self::Bool(false),
            Kind::Number => Self::Number(r.num),
            Kind::String => Self::String(r.s.to_vec()),
            Kind::Json => match r.raw.iter().find(|&&c| c > b' ' || c == b'{' || c == b'[') {
                Some(b'{') => {
                    let mut map = std::collections::BTreeMap::new();
                    for (key, value) in array_or_map(&r.raw, b'{', 0).1 {
                        map.entry(key).or_insert_with(|| Self::from_res(&value));
                    }
                    Self::Object(map)
                }
                Some(b'[') => Self::Array(array_or_map(&r.raw, b'[', 0).0.iter().map(Self::from_res).collect()),
                _ => Self::Null,
            },
        }
    }

    /// `json.Marshal`: sorted keys, HTML-escaped strings. `None` for NaN or infinite
    /// numbers, which Marshal rejects.
    pub fn marshal(&self) -> Option<Vec<u8>> {
        let mut out = vec![];
        self.write(&mut out)?;
        Some(out)
    }

    fn write(&self, out: &mut Vec<u8>) -> Option<()> {
        match self {
            Self::Null => out.extend_from_slice(b"null"),
            Self::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
            Self::Number(n) => out.extend_from_slice(json_float(*n)?.as_bytes()),
            Self::String(s) => marshal_str(out, s, true),
            Self::Array(items) => {
                out.push(b'[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    item.write(out)?;
                }
                out.push(b']');
            }
            Self::Object(map) => {
                out.push(b'{');
                for (i, (key, value)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    marshal_str(out, key, true);
                    out.push(b':');
                    value.write(out)?;
                }
                out.push(b'}');
            }
        }
        Some(())
    }

    /// `out, _ = sjson.SetBytes(out, path, value)` for this value: scalars use sjson's
    /// own encodings (strings conditionally escaped, floats in `'f'` format), containers
    /// are marshaled. Where Marshal fails (a NaN or infinite number inside a container)
    /// sjson returns nil, so `out` is cleared, as Go's callers end up with.
    pub fn set(&self, out: &mut Vec<u8>, path: &(impl JsonPath + ?Sized)) -> bool {
        let path = path.as_path();
        match self {
            Self::Null => set_raw(out, path, b"null"),
            Self::Bool(b) => set_bool(out, path, *b),
            Self::Number(n) => set_f64(out, path, *n),
            Self::String(s) => set_str(out, path, s),
            Self::Array(_) | Self::Object(_) => match self.marshal() {
                Some(raw) => set_raw(out, path, raw),
                None => {
                    out.clear();
                    false
                }
            },
        }
    }
}

// ---------------------------------------------------------------------------------------
// Go's `any` model (encoding/json decode then marshal)

/// A JSON document decoded the way Go decodes into `any`: objects become maps that
/// marshal with sorted keys (the last duplicate wins), strings are valid UTF-8 (invalid
/// bytes become U+FFFD) and numbers are either kept literally (`Decoder.UseNumber`) or
/// round-tripped through float64 (plain `json.Unmarshal`).
#[derive(Debug, Clone, PartialEq)]
pub enum GoValue {
    Null,
    Bool(bool),
    /// The number text Marshal writes back.
    Number(String),
    String(String),
    Array(Vec<GoValue>),
    Object(std::collections::BTreeMap<String, GoValue>),
}

impl GoValue {
    /// `Decoder.UseNumber()` then decode: numbers keep their literal text.
    pub fn parse(text: &[u8]) -> Option<Self> {
        valid(text).then(|| Self::from_res(&parse(text), false))?
    }

    /// `json.Unmarshal(text, &any)`: numbers become float64, so `1e3` marshals as `1000`.
    /// `None` when the text is invalid or a number overflows float64.
    pub fn parse_f64(text: &[u8]) -> Option<Self> {
        valid(text).then(|| Self::from_res(&parse(text), true))?
    }

    fn from_res(r: &Res<'_>, float: bool) -> Option<Self> {
        Some(match r.kind {
            Kind::Null => Self::Null,
            Kind::True => Self::Bool(true),
            Kind::False => Self::Bool(false),
            Kind::Number if float => Self::Number(json_float(parse_float(&r.raw))?),
            Kind::Number => Self::Number(String::from_utf8_lossy(&r.raw).into_owned()),
            Kind::String => Self::String(go_unquote(&r.raw)?),
            Kind::Json if r.is_array() => {
                let mut items = vec![];
                for item in r.array() {
                    items.push(Self::from_res(&item, float)?);
                }
                Self::Array(items)
            }
            Kind::Json => {
                let mut map = std::collections::BTreeMap::new();
                let mut ok = true;
                r.each(|key, value| {
                    match (go_unquote(&key.raw), Self::from_res(&value, float)) {
                        (Some(k), Some(v)) => {
                            map.insert(k, v);
                        }
                        _ => ok = false,
                    }
                    ok
                });
                if !ok {
                    return None;
                }
                Self::Object(map)
            }
        })
    }

    /// serde JSON metadata in Go's model (numbers keep their literal text).
    pub fn from_json(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(b) => Self::Bool(*b),
            serde_json::Value::Number(n) => Self::Number(n.to_string()),
            serde_json::Value::String(s) => Self::String(s.clone()),
            serde_json::Value::Array(items) => Self::Array(items.iter().map(Self::from_json).collect()),
            serde_json::Value::Object(map) => {
                Self::Object(map.iter().map(|(k, v)| (k.clone(), Self::from_json(v))).collect())
            }
        }
    }

    /// `json.Marshal`.
    pub fn marshal(&self) -> Vec<u8> {
        let mut out = vec![];
        self.write(&mut out);
        out
    }

    /// A `json.Encoder` with `SetIndent("", "  ")`, including the trailing newline.
    pub fn encode_indented(&self) -> Vec<u8> {
        let mut out = vec![];
        self.write_indented(&mut out, 0);
        out.push(b'\n');
        out
    }

    fn write(&self, out: &mut Vec<u8>) {
        self.write_with(out, true);
    }

    /// An Encoder with `SetEscapeHTML(false)`, without the trailing newline.
    pub fn marshal_no_html(&self) -> Vec<u8> {
        let mut out = vec![];
        self.write_with(&mut out, false);
        out
    }

    fn write_with(&self, out: &mut Vec<u8>, html: bool) {
        match self {
            Self::Null => out.extend_from_slice(b"null"),
            Self::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
            Self::Number(n) => out.extend_from_slice(n.as_bytes()),
            Self::String(s) => marshal_str(out, s.as_bytes(), html),
            Self::Array(items) => {
                out.push(b'[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    item.write_with(out, html);
                }
                out.push(b']');
            }
            Self::Object(map) => {
                out.push(b'{');
                for (i, (key, item)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    marshal_str(out, key.as_bytes(), html);
                    out.push(b':');
                    item.write_with(out, html);
                }
                out.push(b'}');
            }
        }
    }

    fn write_indented(&self, out: &mut Vec<u8>, depth: usize) {
        let pad = |out: &mut Vec<u8>, depth: usize| {
            out.push(b'\n');
            out.extend(std::iter::repeat_n(b' ', depth * 2));
        };
        match self {
            Self::Array(items) if !items.is_empty() => {
                out.push(b'[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    pad(out, depth + 1);
                    item.write_indented(out, depth + 1);
                }
                pad(out, depth);
                out.push(b']');
            }
            Self::Object(map) if !map.is_empty() => {
                out.push(b'{');
                for (i, (key, item)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    pad(out, depth + 1);
                    marshal_str(out, key.as_bytes(), true);
                    out.extend_from_slice(b": ");
                    item.write_indented(out, depth + 1);
                }
                pad(out, depth);
                out.push(b'}');
            }
            _ => self.write(out),
        }
    }
}

/// Go decode-then-marshal of one document with `UseNumber`, for semantic comparisons.
pub fn canonical(text: &[u8]) -> Option<Vec<u8>> {
    GoValue::parse(trim_ws(text)).map(|v| v.marshal())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(b: Vec<u8>) -> String {
        String::from_utf8(b).unwrap()
    }

    // Cases carried over from cpa-exec's kimi_json, each checked against sjson.
    #[test]
    fn set_appends_and_replaces_like_sjson() {
        assert_eq!(
            s(try_set_str(br#"{"a":1}"#, "model", "k3").unwrap()),
            r#"{"a":1,"model":"k3"}"#
        );
        assert_eq!(
            s(try_set_str(br#"{"model":"x","a":1}"#, "model", "k3").unwrap()),
            r#"{"model":"k3","a":1}"#
        );
        assert_eq!(
            s(try_set_str(b"{}", "thinking.type", "enabled").unwrap()),
            r#"{"thinking":{"type":"enabled"}}"#
        );
        assert_eq!(
            s(try_set_str(br#"{ "a": 1 }"#, "b", "x").unwrap()),
            r#"{ "a": 1 ,"b":"x"}"#
        );
        assert_eq!(
            s(try_set_raw(br#"{"m":[{"a":1},{"b":2}]}"#, "m.1.c", "true").unwrap()),
            r#"{"m":[{"a":1},{"b":2,"c":true}]}"#
        );
        assert_eq!(s(try_set_raw(b"  ", "a.0", "1").unwrap()), r#"{"a":[1]}"#);
        assert_eq!(
            try_set_raw(br#"{"stream_options":[]}"#, "stream_options.include_usage", "true"),
            Err("cannot set array element for non-numeric key 'include_usage'".to_owned())
        );
        // Plain ASCII strings stay raw; anything that needs marshaling is HTML-escaped too.
        assert_eq!(s(try_set_str(b"{}", "a", "a<b").unwrap()), r#"{"a":"a<b"}"#);
        assert_eq!(s(try_set_str(b"{}", "a", "é<\"").unwrap()), "{\"a\":\"é\\u003c\\\"\"}");
    }

    /// `valid` jumps over plain string bytes; a byte that decides validity must be seen
    /// wherever it sits in a long string.
    #[test]
    fn valid_sees_string_bytes_in_every_word_lane() {
        for at in 0..40 {
            let doc = |inner: &[u8]| {
                let mut d = b"{\"k\":\"".to_vec();
                d.extend(std::iter::repeat_n(b'x', at));
                d.extend_from_slice(inner);
                d.extend_from_slice("yyyyyyyéyyyyyyyyy\"}".as_bytes());
                d
            };
            assert!(valid(&doc(b"")), "plain, offset {at}");
            assert!(valid(&doc(b"\\n")), "escape, offset {at}");
            assert!(valid(&doc(b"\\u00e9")), "unicode escape, offset {at}");
            assert!(!valid(&doc(b"\n")), "raw newline, offset {at}");
            assert!(!valid(&doc(b"\x1f")), "raw 0x1f, offset {at}");
            assert!(!valid(&doc(b"\\x")), "bad escape, offset {at}");
            assert!(!valid(&doc(b"\"")), "early quote, offset {at}");
            assert!(valid(&doc(b"\x20\x7f")), "0x20 and 0x7f are plain, offset {at}");
        }
    }

    #[test]
    fn delete_takes_one_neighbouring_comma() {
        let body: &[u8] = br#"{"a": 1, "b": 2, "c": 3}"#;
        let del = |b: &[u8], p: &str| s(try_delete(b, p).unwrap());
        assert_eq!(del(body, "a"), r#"{ "b": 2, "c": 3}"#);
        assert_eq!(del(body, "b"), r#"{"a": 1, "c": 3}"#);
        assert_eq!(del(body, "c"), r#"{"a": 1, "b": 2}"#);
        assert_eq!(del(br#"{"only":true}"#, "only"), "{}");
        assert_eq!(del(body, "missing"), String::from_utf8_lossy(body));
        assert_eq!(del(b"[1,2]", "-1"), "[1]");
        // Go's backward scan skips a byte after an escaped quote and leaves malformed JSON
        // for a key that itself contains one; parity keeps sjson's exact output.
        assert_eq!(del(br#"{"\"x":1,"y":2}"#, "\\\"x"), r#"{"\"x":,"y":2}"#);
        assert_eq!(
            del(br#"{"t":{"type":"x","effort":"y"}}"#, "t.effort"),
            r#"{"t":{"type":"x"}}"#
        );
    }

    #[test]
    fn go_marshal_sorts_keys_and_picks_number_mode() {
        let v = GoValue::parse(br#"{"z":1.50,"a":{"y":"<&>","b":[true,null]},"a":2}"#).unwrap();
        assert_eq!(s(v.marshal()), r#"{"a":2,"z":1.50}"#);
        assert_eq!(
            s(canonical(br#"{"b":"\u2028x","a":1e3}"#).unwrap()),
            r#"{"a":1e3,"b":"\u2028x"}"#
        );
        let f = GoValue::parse_f64(br#"{"n":[1e3,1.50,9007199254740993,1e-7,1e21],"s":"bad\ud800"}"#).unwrap();
        assert_eq!(
            s(f.marshal()),
            r#"{"n":[1000,1.5,9007199254740992,1e-7,1e+21],"s":"bad�"}"#
        );
        assert!(GoValue::parse_f64(b"[1e400]").is_none());
        assert_eq!(quote("<&>\u{1}\u{8}"), br#""\u003c\u0026\u003e\u0001\b""#);
    }

    // The string scans jump to the next quote and count the backslashes before it: an
    // odd run escapes the quote, an even run is escaped backslashes and ends the string.
    #[test]
    fn lookups_skip_strings_with_backslash_runs() {
        let tricky = [
            "",
            "\\",
            "\\\\",
            "\"",
            "\\\"",
            "a\\\\\"}{][",
            "x".repeat(70_000).as_str(),
        ]
        .map(String::from);
        for (i, a) in tricky.iter().enumerate() {
            for b in &tricky[i..] {
                let doc = serde_json::json!({
                    "skip": {"s": a, "nested": [a, {"t": b}]},
                    "plain": a,
                    "want": b,
                })
                .to_string();
                let got = get(doc.as_bytes(), "want");
                assert_eq!(got.str(), b.as_str(), "{doc:.200}");
                assert_eq!(get(doc.as_bytes(), "skip.nested.1.t").str(), b.as_str());
                assert!(!get(doc.as_bytes(), "missing").exists());
                // Unterminated documents end the scan without a match.
                let cut = &doc.as_bytes()[..doc.len() - b.len().min(3) - 3];
                assert!(!get(cut, "absent").exists());
            }
        }
    }
}

#[cfg(test)]
mod parse_float_tests {
    use super::go_parse_float;

    /// Expectations from Go 1.26 strconv.ParseFloat (value, error?).
    #[test]
    fn parse_float_matches_go_strconv() {
        let ok = |s: &str, want: f64| {
            let got = go_parse_float(s.as_bytes()).unwrap_or_else(|_| panic!("{s} should parse"));
            assert!(got == want || (got.is_nan() && want.is_nan()), "{s}: {got} != {want}");
            assert_eq!(got.is_sign_negative(), want.is_sign_negative(), "{s} sign");
        };
        let err = |s: &str, value: f64| assert_eq!(go_parse_float(s.as_bytes()), Err(value), "{s}");
        ok("1_000", 1000.0);
        ok("1_000.5", 1000.5);
        ok("1e1_0", 1e10);
        ok("0x1p4", 16.0);
        ok("0X1.8P1", 3.0);
        ok("0x_1p0", 1.0);
        ok("0x.8p1", 1.0);
        ok("-0x1p-1074", -5e-324);
        ok("0x1.fffffffffffffp1023", f64::MAX);
        err("0x1.fffffffffffff8p1023", f64::INFINITY);
        ok("0x1.00000000000008p0", 1.0);
        ok("0x1.00000000000018p0", 1.0000000000000004);
        ok("inf", f64::INFINITY);
        ok("+Inf", f64::INFINITY);
        ok("-infinity", f64::NEG_INFINITY);
        ok("NaN", f64::NAN);
        ok("1e-400", 0.0);
        ok("1.", 1.0);
        ok(".5", 0.5);
        ok("-0", -0.0);
        ok("1E5", 100000.0);
        err("1__0", 0.0);
        err("_1", 0.0);
        err("0x10", 0.0);
        err("0x1p", 0.0);
        err("+nan", 0.0);
        err("1e400", f64::INFINITY);
        err("-1e400", f64::NEG_INFINITY);
        err("0x1p1024", f64::INFINITY);
        err("  1", 0.0);
        err("1e", 0.0);
        err("+", 0.0);
        err("0b101", 0.0);
        err("infx", 0.0);
        err("1e_5", 0.0);
    }
}
