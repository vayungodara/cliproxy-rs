//! Kimi thinking signatures, identified by fixed length only (kimi_validation.go).

use super::grok::byte_entropy_ratio;
use super::wire::RAW_STD;
use super::{
    ClaudeValidation, Error, GeminiValidation, err, first_invalid_char, is_valid_claude_cais_signature,
    is_valid_claude_thinking_signature, is_valid_gemini_thought_signature, maybe_self_describing_envelope,
    split_provider_prefix,
};
use crate::gostr::trim_space;

pub const KIMI_THINKING_SIGNATURE_NON_STREAMING_LEN: usize = 12946;
pub const KIMI_THINKING_SIGNATURE_STREAMING_LEN: usize = 4340;
pub const MIN_KIMI_THINKING_SIGNATURE_ENTROPY_RATIO: f64 = 0.85;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KimiSignatureMode {
    NonStreaming,
    Streaming,
}

impl KimiSignatureMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NonStreaming => "non_streaming",
            Self::Streaming => "streaming",
        }
    }
}

/// `KimiThinkingSignatureInfo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KimiSignatureInfo {
    pub raw_len: usize,
    pub decoded_len: usize,
    pub mode: KimiSignatureMode,
}

/// `InspectKimiThinkingSignature`.
pub fn inspect_kimi_thinking_signature(raw: impl AsRef<[u8]>) -> Result<KimiSignatureInfo, Error> {
    let raw = raw.as_ref();
    let sig = trim_space(raw);
    if sig.is_empty() {
        return err("empty Kimi thinking signature");
    }
    if sig != raw {
        return err("Kimi thinking signature has leading or trailing whitespace");
    }
    let mode = match sig.len() {
        KIMI_THINKING_SIGNATURE_NON_STREAMING_LEN => KimiSignatureMode::NonStreaming,
        KIMI_THINKING_SIGNATURE_STREAMING_LEN => KimiSignatureMode::Streaming,
        len => return err(format!("invalid Kimi thinking signature: unexpected length {len}")),
    };
    if sig.contains(&b'=') {
        return err("invalid Kimi thinking signature: expected unpadded standard base64");
    }
    if let Some((index, rune)) = first_invalid_char(sig, b"+/") {
        return err(format!(
            "invalid Kimi thinking signature: contains non-base64 character U+{rune:04X} at byte {index}"
        ));
    }
    if split_provider_prefix(sig).is_some() {
        return err("invalid Kimi thinking signature: carries another provider's cache prefix");
    }
    if maybe_self_describing_envelope(sig) {
        if sig.starts_with(b"gAAAA") {
            return err("Kimi thinking signature looks like GPT/Codex reasoning signature");
        }
        if is_valid_claude_cais_signature(sig) {
            return err("Kimi thinking signature looks like Claude CAIS thinking signature");
        }
        if is_valid_claude_thinking_signature(sig, ClaudeValidation::STRICT) {
            return err("Kimi thinking signature looks like Claude thinking signature");
        }
        let known = GeminiValidation {
            require_known_envelope: true,
            ..Default::default()
        };
        if is_valid_gemini_thought_signature(sig, known) {
            return err("Kimi thinking signature looks like Gemini thoughtSignature");
        }
    }
    let decoded = RAW_STD
        .decode(sig)
        .map_err(|e| Error(format!("invalid Kimi thinking signature: base64 decode failed: {e}")))?;
    let ratio = byte_entropy_ratio(&decoded);
    if ratio < MIN_KIMI_THINKING_SIGNATURE_ENTROPY_RATIO {
        return err(format!(
            "invalid Kimi thinking signature: decoded payload entropy ratio {ratio:.3} below {MIN_KIMI_THINKING_SIGNATURE_ENTROPY_RATIO:.3}"
        ));
    }
    Ok(KimiSignatureInfo {
        raw_len: sig.len(),
        decoded_len: decoded.len(),
        mode,
    })
}

/// `IsValidKimiThinkingSignature`.
pub fn is_valid_kimi_thinking_signature(raw: impl AsRef<[u8]>) -> bool {
    inspect_kimi_thinking_signature(raw).is_ok()
}
