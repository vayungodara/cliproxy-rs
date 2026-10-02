//! xAI `encrypted_content` (grok_validation.go). xAI ciphertext has no envelope, so this
//! is a replay-safety shape check after provenance is known, never a detector.

use super::wire::RAW_STD;
use super::{
    ClaudeValidation, Error, GeminiValidation, err, first_invalid_char, inspect_gemini_thought_signature,
    is_valid_claude_cais_signature, is_valid_claude_thinking_signature, is_valid_kimi_thinking_signature,
    maybe_self_describing_envelope, split_provider_prefix,
};
use crate::gostr::trim_space;

pub const MAX_GROK_ENCRYPTED_CONTENT_LEN: usize = 8 * 1024 * 1024;
pub const MIN_GROK_ENCRYPTED_CONTENT_DECODED_LEN: usize = 32;
pub const MIN_GROK_ENCRYPTED_CONTENT_ENTROPY_RATIO: f64 = 0.85;

/// `GrokEncryptedContentInfo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrokContentInfo {
    pub raw_len: usize,
    pub decoded_len: usize,
}

/// `InspectGrokEncryptedContent`.
pub fn inspect_grok_encrypted_content(raw: impl AsRef<[u8]>) -> Result<GrokContentInfo, Error> {
    let raw = raw.as_ref();
    let sig = trim_space(raw);
    if sig.is_empty() {
        return err("empty Grok encrypted_content");
    }
    if sig.len() > MAX_GROK_ENCRYPTED_CONTENT_LEN {
        return err(format!(
            "Grok encrypted_content exceeds maximum length ({MAX_GROK_ENCRYPTED_CONTENT_LEN} bytes)"
        ));
    }
    if sig != raw {
        return err("Grok encrypted_content has leading or trailing whitespace");
    }
    if sig.contains(&b'=') {
        return err("invalid Grok encrypted_content: expected unpadded standard base64");
    }
    if let Some((index, rune)) = first_invalid_char(sig, b"+/") {
        return err(format!(
            "invalid Grok encrypted_content: contains non-base64 character U+{rune:04X} at byte {index}"
        ));
    }
    if split_provider_prefix(sig).is_some() {
        return err("invalid Grok encrypted_content: carries another provider's cache prefix");
    }
    if maybe_self_describing_envelope(sig) {
        if sig.starts_with(b"gAAAA") {
            return err("Grok encrypted_content looks like GPT/Codex reasoning signature");
        }
        if is_valid_claude_thinking_signature(sig, ClaudeValidation::STRICT) {
            return err("Grok encrypted_content looks like Claude thinking signature");
        }
        if is_valid_claude_cais_signature(sig) {
            return err("Grok encrypted_content looks like Claude CAIS thinking signature");
        }
        let known = GeminiValidation {
            require_known_envelope: true,
            ..Default::default()
        };
        if inspect_gemini_thought_signature(sig, known).is_ok() {
            return err("Grok encrypted_content looks like Gemini thoughtSignature");
        }
    }
    if is_valid_kimi_thinking_signature(sig) {
        return err("Grok encrypted_content has a Kimi thinking signature length");
    }
    let decoded = RAW_STD
        .decode(sig)
        .map_err(|e| Error(format!("invalid Grok encrypted_content: base64 decode failed: {e}")))?;
    if decoded.len() < MIN_GROK_ENCRYPTED_CONTENT_DECODED_LEN {
        return err(format!(
            "invalid Grok encrypted_content: decoded payload too short ({} bytes)",
            decoded.len()
        ));
    }
    let ratio = byte_entropy_ratio(&decoded);
    if ratio < MIN_GROK_ENCRYPTED_CONTENT_ENTROPY_RATIO {
        return err(format!(
            "invalid Grok encrypted_content: decoded payload entropy ratio {ratio:.3} below {MIN_GROK_ENCRYPTED_CONTENT_ENTROPY_RATIO:.3}"
        ));
    }
    Ok(GrokContentInfo {
        raw_len: sig.len(),
        decoded_len: decoded.len(),
    })
}

/// `IsValidGrokEncryptedContent`.
pub fn is_valid_grok_encrypted_content(raw: impl AsRef<[u8]>) -> bool {
    inspect_grok_encrypted_content(raw).is_ok()
}

/// Shannon entropy normalised by the most a buffer of this length can carry.
pub(crate) fn byte_entropy_ratio(buf: &[u8]) -> f64 {
    if buf.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    for &b in buf {
        counts[usize::from(b)] += 1;
    }
    let n = buf.len() as f64;
    let mut entropy = 0.0;
    for &count in &counts {
        if count == 0 {
            continue;
        }
        let p = count as f64 / n;
        entropy -= p * p.log2();
    }
    let symbols = buf.len().min(256);
    if symbols <= 1 {
        return 0.0;
    }
    entropy / (symbols as f64).log2()
}
