//! Adapters for the Codex request helpers the Meta executor calls but this thread does
//! not own, plus Go's `bytes.TrimSpace`. Each names its owner; at integration the owner's
//! function replaces it and the Meta fixtures (tests/device_fixtures/meta) are the
//! acceptance test.

use cpa_common::json::{self as gj, Kind};

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

/// ponytail: adapter, owner the Codex thread (codex_executor_request.go
/// `normalizeCodexInstructions`; the Codex executor inlines it in its request shaping).
/// A missing or null `instructions` becomes "".
pub(crate) fn normalize_codex_instructions(body: &mut Vec<u8>) {
    let instructions = gj::get(body, "instructions");
    if !instructions.exists() || instructions.kind == Kind::Null {
        gj::set_str(body, "instructions", "");
    }
}

/// ponytail: adapter, owner the Codex thread (codex_executor_tokens.go
/// `countCodexInputTokens` with the O200kBase encoder).
pub(crate) fn count_codex_input_tokens(body: &[u8]) -> Result<i64, String> {
    if body.is_empty() {
        return Ok(0);
    }
    let mut segments: Vec<Vec<u8>> = Vec::new();
    let mut push = |s: &[u8]| {
        let s = go_trim_space(s);
        if !s.is_empty() {
            segments.push(s.to_vec());
        }
    };
    // `params.Raw`, or the string value for a string.
    let raw_or_string = |v: &gj::Res<'_>| {
        if v.kind == Kind::String {
            v.bytes().into_owned()
        } else {
            v.raw.to_vec()
        }
    };
    let root = gj::parse(body);
    push(&root.get("instructions").bytes());
    let input = root.get("input");
    if input.is_array() {
        for item in input.array() {
            match &*item.get("type").bytes() {
                b"message" => {
                    let content = item.get("content");
                    if content.is_array() {
                        for part in content.array() {
                            push(&part.get("text").bytes());
                        }
                    }
                }
                b"function_call" => {
                    push(&item.get("name").bytes());
                    push(&item.get("arguments").bytes());
                }
                b"function_call_output" => push(&item.get("output").bytes()),
                _ => push(&item.get("text").bytes()),
            }
        }
    }
    let tools = root.get("tools");
    if tools.is_array() {
        for tool in tools.array() {
            push(&tool.get("name").bytes());
            push(&tool.get("description").bytes());
            let params = tool.get("parameters");
            if params.exists() {
                push(&raw_or_string(&params));
            }
        }
    }
    let format = root.get("text.format");
    if format.exists() {
        push(&format.get("name").bytes());
        let schema = format.get("schema");
        if schema.exists() {
            push(&raw_or_string(&schema));
        }
    }
    let text = String::from_utf8_lossy(&segments.join(&b'\n')).into_owned();
    if text.is_empty() {
        return Ok(0);
    }
    static ENCODER: std::sync::OnceLock<Result<tiktoken_rs::CoreBPE, String>> = std::sync::OnceLock::new();
    let encoder = ENCODER
        .get_or_init(|| tiktoken_rs::o200k_base().map_err(|e| e.to_string()))
        .as_ref()
        .map_err(Clone::clone)?;
    Ok(encoder.encode_ordinary(&text).len() as i64)
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
