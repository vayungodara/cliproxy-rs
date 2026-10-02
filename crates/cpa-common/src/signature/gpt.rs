//! OpenAI reasoning `encrypted_content`: Fernet tokens (gpt_validation.go).

use super::wire::{RAW_URL, URL};
use super::{Error, err, first_invalid_char};
use crate::gostr::trim_space;

pub const MAX_GPT_REASONING_SIGNATURE_LEN: usize = 32 * 1024 * 1024;

/// `GPTReasoningSignatureInfo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GptSignatureInfo {
    pub decoded_len: usize,
    pub ciphertext_len: usize,
}

/// `IsValidGPTReasoningSignature`.
pub fn is_valid_gpt_reasoning_signature(raw: impl AsRef<[u8]>) -> bool {
    inspect_gpt_reasoning_signature(raw).is_ok()
}

/// `InspectGPTReasoningSignature`: version 0x80, 8-byte timestamp, 16-byte IV,
/// AES-block ciphertext and a 32-byte HMAC.
pub fn inspect_gpt_reasoning_signature(raw: impl AsRef<[u8]>) -> Result<GptSignatureInfo, Error> {
    let sig = trim_space(raw.as_ref());
    if sig.is_empty() {
        return err("empty GPT reasoning signature");
    }
    if sig.len() > MAX_GPT_REASONING_SIGNATURE_LEN {
        return err(format!(
            "GPT reasoning signature exceeds maximum length ({MAX_GPT_REASONING_SIGNATURE_LEN} bytes)"
        ));
    }
    if !sig.starts_with(b"gAAAA") {
        return err("invalid GPT reasoning signature: expected gAAAA prefix");
    }
    if let Some((index, rune)) = first_invalid_char(sig, b"-_=") {
        return err(format!(
            "invalid GPT reasoning signature: contains non-base64url character U+{rune:04X} at byte {index}"
        ));
    }
    let decoded = RAW_URL
        .decode(sig)
        .or_else(|_| URL.decode(sig))
        .map_err(|_| Error("invalid GPT reasoning signature: base64url decode failed".into()))?;
    if decoded.len() < 73 {
        return err("invalid GPT reasoning signature: decoded payload too short");
    }
    if decoded[0] != 0x80 {
        return err(format!(
            "invalid GPT reasoning signature: expected version 0x80, got 0x{:02x}",
            decoded[0]
        ));
    }
    let ciphertext_len = decoded.len() as i64 - 1 - 8 - 16 - 32;
    if ciphertext_len <= 0 || ciphertext_len % 16 != 0 {
        return err(format!(
            "invalid GPT reasoning signature: ciphertext length {ciphertext_len} is not a positive AES block multiple"
        ));
    }
    Ok(GptSignatureInfo {
        decoded_len: decoded.len(),
        ciphertext_len: ciphertext_len as usize,
    })
}
