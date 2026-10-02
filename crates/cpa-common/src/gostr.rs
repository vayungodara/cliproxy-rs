//! Go string semantics the ported code compares and prints with: `strings.ToLower`,
//! `strings.EqualFold` and `strconv.Quote` (`%q`). They run on Go's own Unicode tables
//! (gostr_tables.rs, generated from Go 1.26), so results do not drift with Rust's
//! Unicode version or its full (multi-character) case mappings.

use crate::gostr_tables::{CASE_ORBIT, CASE_RANGES, PRINT_RANGES};

const MAX_RUNE: u32 = 0x10FFFF;
const UPPER: usize = 0;
const LOWER: usize = 1;

/// `unicode.To(case, r)` over `unicode.CaseRanges`.
fn to_case(case: usize, r: u32) -> u32 {
    let Ok(i) = CASE_RANGES.binary_search_by(|&(lo, hi, ..)| {
        if hi < r {
            std::cmp::Ordering::Less
        } else if lo > r {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    }) else {
        return r;
    };
    let (lo, _, upper, lower, title) = CASE_RANGES[i];
    let delta = [upper, lower, title][case];
    if delta > MAX_RUNE as i32 {
        // Alternating Upper/Lower run: even offsets are upper case, odd are lower.
        return lo + (((r - lo) & !1) | (case as u32 & 1));
    }
    r.wrapping_add_signed(delta)
}

/// `unicode.ToLower`.
pub fn to_lower_rune(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    char::from_u32(to_case(LOWER, c as u32)).unwrap_or(c)
}

/// `unicode.ToUpper`.
pub fn to_upper_rune(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_uppercase();
    }
    char::from_u32(to_case(UPPER, c as u32)).unwrap_or(c)
}

/// `unicode.SimpleFold`.
fn simple_fold(r: u32) -> u32 {
    if let Ok(i) = CASE_ORBIT.binary_search_by_key(&r, |&(from, _)| from) {
        return CASE_ORBIT[i].1;
    }
    let Some(c) = char::from_u32(r) else { return r };
    let lower = to_lower_rune(c);
    if lower != c {
        lower as u32
    } else {
        to_upper_rune(c) as u32
    }
}

/// Go's string comparisons, as methods so call sites read like the Go they port.
pub trait GoStr {
    /// `strings.ToLower`: rune-wise simple lowercase (no context, no expansion).
    fn go_lower(&self) -> String;
    /// `strings.EqualFold`: simple Unicode case folding.
    fn go_eq_fold(&self, other: &str) -> bool;
}

impl GoStr for str {
    fn go_lower(&self) -> String {
        self.chars().map(to_lower_rune).collect()
    }

    fn go_eq_fold(&self, other: &str) -> bool {
        let mut t = other.chars();
        for s in self.chars() {
            let Some(t) = t.next() else { return false };
            if s == t {
                continue;
            }
            let (mut sr, mut tr) = (s as u32, t as u32);
            if tr < sr {
                std::mem::swap(&mut sr, &mut tr);
            }
            if tr < 0x80 {
                if (u32::from(b'A')..=u32::from(b'Z')).contains(&sr) && tr == sr + 32 {
                    continue;
                }
                return false;
            }
            let mut r = simple_fold(sr);
            while r != sr && r < tr {
                r = simple_fold(r);
            }
            if r != tr {
                return false;
            }
        }
        t.next().is_none()
    }
}

/// `strconv.IsPrint`.
pub fn is_print(c: char) -> bool {
    let r = c as u32;
    PRINT_RANGES
        .binary_search_by(|&(lo, hi)| {
            if hi < r {
                std::cmp::Ordering::Less
            } else if lo > r {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// `strconv.Quote`, which is what `%q` prints for a string.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c if is_print(c) => out.push(c),
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{b}' => out.push_str("\\v"),
            c if c < ' ' || c == '\u{7f}' => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if (c as u32) < 0x10000 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push_str(&format!("\\U{:08x}", c as u32)),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values from Go 1.26 (strings.ToLower, strings.EqualFold, strconv.Quote).
    #[test]
    fn matches_go_case_and_quote_rules() {
        assert_eq!("HİGH".go_lower(), "high", "U+0130 lowercases to plain i in Go");
        assert_eq!("ΣΑΣ".go_lower(), "σασ", "no final-sigma context in Go");
        assert_eq!("ǅ".go_lower(), "ǆ");
        assert!("\u{17f}".go_eq_fold("s") && "S".go_eq_fold("\u{17f}"));
        assert!("\u{212a}".go_eq_fold("k"));
        assert!("ς".go_eq_fold("Σ") && "σ".go_eq_fold("ς"));
        assert!(!"ı".go_eq_fold("i") && !"ı".go_eq_fold("I") && !"İ".go_eq_fold("i"));
        assert!(!"high".go_eq_fold("hig") && !"hig".go_eq_fold("high"));
        assert_eq!(quote("a\nb"), r#""a\nb""#);
        assert_eq!(quote("\u{a0}"), r#""\u00a0""#);
        assert_eq!(quote("é\u{7f}\u{1}\""), r#""é\x7f\x01\"""#);
        assert_eq!(quote("\u{e0000}"), r#""\U000e0000""#);
        assert_eq!(quote("\u{1f600}"), "\"\u{1f600}\"");
    }
}
