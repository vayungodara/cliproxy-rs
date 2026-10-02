//! CCH billing signature (claude_signing.go): xxHash64 over the request body with
//! the `cch` digits zeroed, `model` string contents and the `max_tokens`,
//! `fallbacks` and `fallback_credit_token` members removed at every object depth.

use super::cloak::{BILLING_PREFIX, text_block};
use crate::rawjson;

const SEED: u64 = 0x4D65_9218_E32A_3268;

/// Real Claude OAuth always signs; an opted-in API key signs only on first-party
/// Anthropic so third-party gateways keep a cache-stable billing header.
pub(crate) fn enabled(api_key: &str, cli_fingerprint: bool, first_party: bool) -> bool {
    api_key.contains("sk-ant-oat") || (cli_fingerprint && first_party)
}

/// `finalizeAnthropicMessagesBodyCCH`.
pub(crate) fn finalize(body: &str, fallback_billing: &str) -> Result<String, String> {
    let body = ensure_placeholder(body, fallback_billing);
    sign(&body)
}

fn ensure_placeholder(body: &str, fallback: &str) -> String {
    let billing = rawjson::get(body, "system.0.text");
    let mut body = body.to_owned();
    if billing.kind() != gjson::Kind::String || !billing.str().starts_with(BILLING_PREFIX) {
        if fallback.is_empty() {
            return body;
        }
        body = prepend_billing(&body, fallback);
    }
    if digits_offset(&body).is_some() {
        return body;
    }
    let text = rawjson::string(&body, "system.0.text");
    let Some(entry) = text.find("cc_entrypoint=") else {
        return body;
    };
    let Some(end) = text[entry..].find(';') else {
        return body;
    };
    let at = entry + end + 1;
    let text = format!("{} cch=00000;{}", &text[..at], &text[at..]);
    rawjson::set_str(&body, "system.0.text", &text)
}

fn prepend_billing(body: &str, billing: &str) -> String {
    let block = text_block(billing, None);
    let system = rawjson::get(body, "system");
    let array = match system.kind() {
        gjson::Kind::String => format!("[{block},{}]", text_block(system.str(), None)),
        gjson::Kind::Array => {
            let raw = system.json().trim();
            if raw == "[]" {
                format!("[{block}]")
            } else {
                format!("[{block},{}", &raw[1..])
            }
        }
        _ => format!("[{block}]"),
    };
    rawjson::set_raw(body, "system", &array)
}

/// Offset of the five `cch=` hex digits inside the first billing block.
fn digits_offset(body: &str) -> Option<usize> {
    let billing = rawjson::get(body, "system.0.text");
    if billing.kind() != gjson::Kind::String || !billing.str().starts_with(BILLING_PREFIX) {
        return None;
    }
    let base = rawjson::offset(body, &billing)?;
    let raw = billing.json().as_bytes();
    let mut from = 0;
    while let Some(rel) = find(&raw[from..], b"cch=") {
        let digits = from + rel + 4;
        let end = digits + 5;
        if end < raw.len()
            && raw[end] == b';'
            && raw[digits..end]
                .iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
        {
            return Some(base + digits);
        }
        from = from + rel + 4;
    }
    None
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn sign(body: &str) -> Result<String, String> {
    let Some(offset) = digits_offset(body) else {
        return Ok(body.to_owned());
    };
    let mut unsigned = body.as_bytes().to_vec();
    unsigned[offset..offset + 5].copy_from_slice(b"00000");
    let normalized = normalize(&unsigned).map_err(|e| format!("normalize Claude CCH input: {e}"))?;
    let cch = format!("{:05x}", xxh64(&normalized, SEED) & 0xFFFFF);
    unsigned[offset..offset + 5].copy_from_slice(cch.as_bytes());
    String::from_utf8(unsigned).map_err(|_| "signed body is not UTF-8".into())
}

struct Scanner<'a> {
    body: &'a [u8],
    pos: usize,
    edits: Vec<(usize, usize)>,
}

struct Member {
    start: usize,
    end: usize,
    comma_before: Option<usize>,
    comma_after: Option<usize>,
    excluded: bool,
}

fn normalize(body: &[u8]) -> Result<Vec<u8>, String> {
    if !gjson::valid(std::str::from_utf8(body).map_err(|e| e.to_string())?) {
        return Err("invalid JSON body".into());
    }
    let mut s = Scanner {
        body,
        pos: 0,
        edits: Vec::new(),
    };
    s.value(true)?;
    s.ws();
    if s.pos != body.len() {
        return Err(format!("unexpected JSON data at byte {}", s.pos));
    }
    s.edits.sort();
    let mut out = Vec::with_capacity(body.len());
    let mut last = 0;
    for (start, end) in s.edits {
        if start < last || end > body.len() {
            return Err(format!("overlapping CCH normalization edit at byte {start}"));
        }
        out.extend_from_slice(&body[last..start]);
        last = end;
    }
    out.extend_from_slice(&body[last..]);
    Ok(out)
}

impl Scanner<'_> {
    fn ws(&mut self) {
        while self.pos < self.body.len() && matches!(self.body[self.pos], b' ' | b'\t' | b'\r' | b'\n') {
            self.pos += 1;
        }
    }
    fn eat(&mut self, c: u8) -> bool {
        let ok = self.pos < self.body.len() && self.body[self.pos] == c;
        if ok {
            self.pos += 1;
        }
        ok
    }
    fn edit(&mut self, start: usize, end: usize) {
        if start < end {
            self.edits.push((start, end));
        }
    }
    fn string(&mut self) -> Result<(usize, usize), String> {
        if self.pos >= self.body.len() || self.body[self.pos] != b'"' {
            return Err(format!("missing JSON string at byte {}", self.pos));
        }
        let start = self.pos;
        self.pos += 1;
        while self.pos < self.body.len() {
            match self.body[self.pos] {
                b'\\' => self.pos += 2,
                b'"' => {
                    self.pos += 1;
                    return Ok((start, self.pos));
                }
                _ => self.pos += 1,
            }
        }
        Err(format!("unterminated JSON string at byte {start}"))
    }
    fn value(&mut self, collect: bool) -> Result<(), String> {
        self.ws();
        match self.body.get(self.pos) {
            None => Err(format!("missing JSON value at byte {}", self.pos)),
            Some(b'{') => self.object(collect),
            Some(b'[') => self.array(collect),
            Some(b'"') => self.string().map(|_| ()),
            Some(_) => {
                let start = self.pos;
                while self.pos < self.body.len()
                    && !matches!(self.body[self.pos], b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n')
                {
                    self.pos += 1;
                }
                if self.pos == start {
                    return Err(format!("missing JSON value at byte {start}"));
                }
                Ok(())
            }
        }
    }
    fn array(&mut self, collect: bool) -> Result<(), String> {
        self.pos += 1;
        self.ws();
        if self.eat(b']') {
            return Ok(());
        }
        loop {
            self.value(collect)?;
            self.ws();
            if self.eat(b',') {
                continue;
            }
            if !self.eat(b']') {
                return Err(format!("missing array end at byte {}", self.pos));
            }
            return Ok(());
        }
    }
    fn object(&mut self, collect: bool) -> Result<(), String> {
        self.pos += 1;
        self.ws();
        if self.eat(b'}') {
            return Ok(());
        }
        let mut members = Vec::new();
        let mut comma_before = None;
        loop {
            self.ws();
            let start = self.pos;
            let (ks, ke) = self.string()?;
            self.ws();
            if !self.eat(b':') {
                return Err(format!("missing object colon at byte {}", self.pos));
            }
            self.ws();
            let key = &self.body[ks..ke];
            let excluded =
                collect && matches!(key, b"\"max_tokens\"" | b"\"fallbacks\"" | b"\"fallback_credit_token\"");
            if collect && key == b"\"model\"" && self.body.get(self.pos) == Some(&b'"') {
                let (vs, ve) = self.string()?;
                self.edit(vs + 1, ve - 1);
            } else {
                self.value(collect && !excluded)?;
            }
            let end = self.pos;
            self.ws();
            let comma_after = self.eat(b',').then(|| self.pos - 1);
            members.push(Member {
                start,
                end,
                comma_before,
                comma_after,
                excluded,
            });
            if comma_after.is_some() {
                comma_before = comma_after;
                continue;
            }
            if !self.eat(b'}') {
                return Err(format!("missing object end at byte {}", self.pos));
            }
            break;
        }
        if collect {
            let mut i = 0;
            while i < members.len() {
                if !members[i].excluded {
                    i += 1;
                    continue;
                }
                let mut j = i;
                while j + 1 < members.len() && members[j + 1].excluded {
                    j += 1;
                }
                let (first, last) = (&members[i], &members[j]);
                let range = if j + 1 < members.len() {
                    (first.start, last.comma_after.map_or(last.end, |c| c + 1))
                } else if i > 0 && j > i {
                    (first.start, last.end)
                } else if i > 0 {
                    (first.comma_before.unwrap_or(first.start), last.end)
                } else {
                    (first.start, last.end)
                };
                self.edit(range.0, range.1);
                i = j + 1;
            }
        }
        Ok(())
    }
}

/// XXH64 (pierrec/xxHash xxHash64 with a seed).
fn xxh64(data: &[u8], seed: u64) -> u64 {
    const P1: u64 = 0x9E37_79B1_85EB_CA87;
    const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
    const P3: u64 = 0x1656_67B1_9E37_79F9;
    const P4: u64 = 0x85EB_CA77_C2B2_AE63;
    const P5: u64 = 0x27D4_EB2F_1656_67C5;
    let read64 = |b: &[u8]| u64::from_le_bytes(b[..8].try_into().expect("8 bytes"));
    let round = |acc: u64, input: u64| {
        acc.wrapping_add(input.wrapping_mul(P2))
            .rotate_left(31)
            .wrapping_mul(P1)
    };
    let merge = |acc: u64, v: u64| (acc ^ round(0, v)).wrapping_mul(P1).wrapping_add(P4);
    let mut rest = data;
    let mut h = if data.len() >= 32 {
        let mut v = [
            seed.wrapping_add(P1).wrapping_add(P2),
            seed.wrapping_add(P2),
            seed,
            seed.wrapping_sub(P1),
        ];
        while rest.len() >= 32 {
            for (i, lane) in v.iter_mut().enumerate() {
                *lane = round(*lane, read64(&rest[i * 8..]));
            }
            rest = &rest[32..];
        }
        let mut h = v[0]
            .rotate_left(1)
            .wrapping_add(v[1].rotate_left(7))
            .wrapping_add(v[2].rotate_left(12))
            .wrapping_add(v[3].rotate_left(18));
        for lane in v {
            h = merge(h, lane);
        }
        h
    } else {
        seed.wrapping_add(P5)
    };
    h = h.wrapping_add(data.len() as u64);
    while rest.len() >= 8 {
        h = (h ^ round(0, read64(rest)))
            .rotate_left(27)
            .wrapping_mul(P1)
            .wrapping_add(P4);
        rest = &rest[8..];
    }
    if rest.len() >= 4 {
        let k = u64::from(u32::from_le_bytes(rest[..4].try_into().expect("4 bytes")));
        h = (h ^ k.wrapping_mul(P1))
            .rotate_left(23)
            .wrapping_mul(P2)
            .wrapping_add(P3);
        rest = &rest[4..];
    }
    for &b in rest {
        h = (h ^ u64::from(b).wrapping_mul(P5)).rotate_left(11).wrapping_mul(P1);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(P2);
    h ^= h >> 29;
    h = h.wrapping_mul(P3);
    h ^ (h >> 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xxh64_reference_vectors() {
        // Published XXH64 test vectors (seed 0 and the reference "abc"/long inputs).
        assert_eq!(xxh64(b"", 0), 0xEF46_DB37_51D8_E999);
        assert_eq!(xxh64(b"a", 0), 0xD24E_C4F1_A98C_6E5B);
        assert_eq!(xxh64(b"abc", 0), 0x44BC_2CF5_AD77_0999);
        assert_eq!(
            xxh64(b"Nobody inspects the spammish repetition", 0),
            0xFBCE_A83C_8A37_8BF1
        );
    }

    #[test]
    fn signs_the_go_captured_body() {
        // Upstream body from the Go differential capture (case `buffered`).
        let signed = r#"{"model":"claude-sonnet-4-6","max_tokens":17,"messages":[{"role":"user","content":[{"type":"text","text":"<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# currentDate\nToday's date is 2026-10-02.\n\n      IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.\n</system-reminder>\n\n"},{"type":"text","text":"Local question","cache_control":{"type":"ephemeral","ttl":"1h"}}]}],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.280.b43; cc_entrypoint=cli; cch=2a498; cc_prompt_id=83ec619f-ab81-4d70-9c67-44c3865291b9; cc_turn_origin=human;"},{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude.","cache_control":{"type":"ephemeral","ttl":"1h"}}],"diagnostics":{"previous_message_id":null},"stream":false,"metadata":{"user_id":"{\"device_id\":\"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\",\"account_uuid\":\"aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee\",\"session_id\":\"dd01238e-cdb5-5572-8f27-a28d98fe9075\"}"}}"#;
        let unsigned = signed.replace("cch=2a498", "cch=00000");
        assert_eq!(finalize(&unsigned, "").unwrap(), signed);
        // An already-signed body re-signs to the same digits.
        assert_eq!(finalize(signed, "").unwrap(), signed);
    }

    #[test]
    fn fallback_billing_is_prepended_to_string_system() {
        let out = finalize(
            r#"{"system":"Native caller system","messages":[]}"#,
            "x-anthropic-billing-header: cc_version=2.1.280.b43; cc_entrypoint=cli;",
        )
        .unwrap();
        assert!(out.starts_with(r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.280.b43; cc_entrypoint=cli; cch="#));
        assert!(out.ends_with(r#";"},{"type":"text","text":"Native caller system"}],"messages":[]}"#));
    }
}
