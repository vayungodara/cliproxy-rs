//! Go's `bytes.TrimSpace` for the Meta and Kimi executors. The Codex request helpers
//! that used to be adapted here are `cpa_common::codex_client` and `crate::codex_tokens`.

/// Go `bytes.TrimSpace`: leading and trailing runes with `unicode.IsSpace` (the same set as
/// Rust's `char::is_whitespace`); an invalid UTF-8 sequence is not space and stops trimming.
pub(crate) fn go_trim_space(b: &[u8]) -> &[u8] {
    fn rune(b: &[u8]) -> Option<char> {
        std::str::from_utf8(b).ok()?.chars().next()
    }
    let mut start = 0;
    while start < b.len() {
        let width = match b[start] {
            0x00..=0x7f => 1,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            _ => 4,
        };
        match rune(&b[start..(start + width).min(b.len())]) {
            Some(c) if c.is_whitespace() => start += width,
            _ => break,
        }
    }
    let mut end = b.len();
    while end > start {
        // The last rune starts at most four bytes back, at a non-continuation byte.
        let first = (end.saturating_sub(4).max(start)..end)
            .rev()
            .find(|&i| b[i] & 0xc0 != 0x80)
            .unwrap_or(end - 1);
        match rune(&b[first..end]) {
            Some(c) if c.is_whitespace() && first + c.len_utf8() == end => end = first,
            _ => break,
        }
    }
    &b[start..end]
}

#[cfg(test)]
mod tests {
    use super::go_trim_space;

    #[test]
    fn trim_space_matches_bytes_trim_space() {
        // Expected values printed by Go 1.26 bytes.TrimSpace for the same inputs.
        let cases: [(&[u8], &[u8]); 10] = [
            (b" \t\x0b\x0cx\r\n", b"x"),
            ("\u{a0}\u{85}x\u{2028}".as_bytes(), b"x"),
            (b"\xffx ", b"\xffx"),
            (b" x\xc2", b"x\xc2"),
            ("x\u{3000}".as_bytes(), b"x"),
            (b"\x1c x", b"\x1c x"),
            (b"   ", b""),
            ("x\u{200b}".as_bytes(), "x\u{200b}".as_bytes()),
            ("\u{180e}x".as_bytes(), "\u{180e}x".as_bytes()),
            (b"x \xe2\x80", b"x \xe2\x80"),
        ];
        for (input, expected) in cases {
            assert_eq!(go_trim_space(input), expected, "{input:?}");
        }
    }
}
