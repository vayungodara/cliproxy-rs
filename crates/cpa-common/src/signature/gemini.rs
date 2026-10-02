//! Gemini `thoughtSignature` validation (gemini_validation.go).

use super::claude::is_valid_claude_cais_signature;
use super::gemini_sanitize::{has_normalized_part_signature, part_thought_signature};
use super::wire::{self, BYTES, FIXED32, FIXED64, RAW_STD, STD, VARINT};
use super::{Error, err};
use crate::gostr::{GoStr, lower_bytes, trim_space};
use crate::json::{self, Res};

pub const MAX_GEMINI_THOUGHT_SIGNATURE_LEN: usize = 32 * 1024 * 1024;
/// Sentinel Gemini accepts in place of a missing first-functionCall signature.
pub const GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR: &str = "skip_thought_signature_validator";
pub const GEMINI_CONTEXT_ENGINEERING_BYPASS: &str = "context_engineering_is_the_way_to_go";

/// `GeminiThoughtSignatureValidationOptions`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GeminiValidation {
    pub allow_bypass_sentinel: bool,
    pub require_known_envelope: bool,
    pub require_observed_marker: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeminiEnvelope {
    Unknown,
    ProtobufField2,
    AsciiUuid,
}

impl GeminiEnvelope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::ProtobufField2 => "protobuf_field_2",
            Self::AsciiUuid => "ascii_uuid",
        }
    }
}

/// `GeminiThoughtSignatureInfo`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GeminiSignatureInfo {
    pub is_bypass_sentinel: bool,
    pub bypass_sentinel: String,
    pub decoded_len: usize,
    pub first_byte: u8,
    pub has_observed_marker: bool,
    pub known_envelope: bool,
    /// `None` for the bypass sentinel, which is not decoded.
    pub envelope: Option<GeminiEnvelope>,
    pub record_count: usize,
    pub opaque_payload_len: usize,
}

/// `IsGeminiThoughtSignatureBypass`.
pub fn is_gemini_bypass(raw: impl AsRef<[u8]>) -> bool {
    let raw = trim_space(raw.as_ref());
    raw == GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.as_bytes() || raw == GEMINI_CONTEXT_ENGINEERING_BYPASS.as_bytes()
}

/// `IsValidGeminiThoughtSignature`.
pub fn is_valid_gemini_thought_signature(raw: impl AsRef<[u8]>, opt: GeminiValidation) -> bool {
    inspect_gemini_thought_signature(raw, opt).is_ok()
}

/// `InspectGeminiThoughtSignature`.
pub fn inspect_gemini_thought_signature(
    raw: impl AsRef<[u8]>,
    opt: GeminiValidation,
) -> Result<GeminiSignatureInfo, Error> {
    let sig = trim_space(raw.as_ref());
    if sig.is_empty() {
        return err("empty Gemini thought signature");
    }
    if is_valid_claude_cais_signature(sig) {
        return err("invalid Gemini thought signature: detected Claude CAIS signature");
    }
    if is_gemini_bypass(sig) {
        if !opt.allow_bypass_sentinel {
            return err("Gemini thought signature bypass sentinel is not allowed");
        }
        return Ok(GeminiSignatureInfo {
            is_bypass_sentinel: true,
            bypass_sentinel: super::ascii(sig),
            ..Default::default()
        });
    }
    let decoded = decode(sig)?;
    if decoded.is_empty() {
        return err("invalid Gemini thought signature: empty decoded payload");
    }
    let (envelope, known) = classify_envelope(&decoded);
    let (record_count, opaque_len) = if envelope == GeminiEnvelope::ProtobufField2 {
        field2_envelope(&decoded).map_or((0, 0), |len| (1, len))
    } else {
        (0, 0)
    };
    let info = GeminiSignatureInfo {
        decoded_len: decoded.len(),
        first_byte: decoded[0],
        has_observed_marker: decoded[0] == 0x12,
        known_envelope: known,
        envelope: Some(envelope),
        record_count,
        opaque_payload_len: opaque_len,
        ..Default::default()
    };
    if opt.require_known_envelope && !info.known_envelope {
        return err(format!(
            "invalid Gemini thought signature: unknown envelope {}",
            crate::gostr::quote(envelope.as_str())
        ));
    }
    if opt.require_observed_marker && !info.has_observed_marker {
        return err(format!(
            "invalid Gemini thought signature: expected observed marker 0x12, got 0x{:02x}",
            info.first_byte
        ));
    }
    Ok(info)
}

fn decode(sig: &[u8]) -> Result<Vec<u8>, Error> {
    if sig.len() > MAX_GEMINI_THOUGHT_SIGNATURE_LEN {
        return err(format!(
            "Gemini thought signature exceeds maximum length ({MAX_GEMINI_THOUGHT_SIGNATURE_LEN} bytes)"
        ));
    }
    match STD.decode(sig) {
        Ok(decoded) => Ok(decoded),
        Err(std_err) => RAW_STD.decode(sig).map_err(|_| {
            Error(format!(
                "invalid Gemini thought signature: base64 decode failed: {std_err}"
            ))
        }),
    }
}

/// The Gemini part (or `request.` wrapped) contents array and its path.
pub(crate) fn contents(body: &[u8]) -> (Res<'_>, &'static str) {
    let contents = json::get(body, "contents");
    if contents.exists() {
        return (contents, "contents");
    }
    (json::get(body, "request.contents"), "request.contents")
}

/// `ValidateGeminiThoughtSignatures`.
pub fn validate_gemini_thought_signatures(body: &[u8], opt: GeminiValidation) -> Result<(), Error> {
    let (contents, path) = contents(body);
    if !contents.is_array() {
        return Ok(());
    }
    for (i, content) in contents.array().iter().enumerate() {
        let parts = content.get("parts");
        if !parts.is_array() {
            continue;
        }
        let model_turn = String::from_utf8_lossy(trim_space(&content.get("role").bytes())).go_eq_fold("model");
        let mut first_call_seen = false;
        for (j, part) in parts.array().iter().enumerate() {
            let has_call = part.get("functionCall").exists();
            let first_call = model_turn && has_call && !first_call_seen;
            if model_turn && has_call {
                first_call_seen = true;
            }
            let (raw, has_sig) = part_thought_signature(part);
            if !has_call && !has_sig {
                continue;
            }
            let part_path = format!("{path}[{i}].parts[{j}]");
            let raw = trim_space(&raw);
            if part.get("functionResponse").exists() && has_sig {
                return err(format!("{part_path}: functionResponse must not carry thoughtSignature"));
            }
            if raw.is_empty() {
                if first_call {
                    return err(format!("{part_path}: missing thoughtSignature on first functionCall"));
                }
                if has_sig {
                    return err(format!("{part_path}: empty thoughtSignature"));
                }
                continue;
            }
            if is_gemini_bypass(raw) && !first_call {
                return err(format!(
                    "{part_path}: Gemini bypass sentinel is allowed only on the first model functionCall"
                ));
            }
            if !has_normalized_part_signature(part, raw) {
                return err(format!(
                    "{part_path}: thoughtSignature must use one canonical top-level field"
                ));
            }
            if let Err(e) = inspect_gemini_thought_signature(raw, opt) {
                return err(format!("{part_path}: {e}"));
            }
        }
    }
    Ok(())
}

/// `ValidateGeminiFunctionCallPairing`: every functionCall group is answered by the
/// next content with matching functionResponse parts.
pub fn validate_gemini_function_call_pairing(body: &[u8]) -> Result<(), Error> {
    struct Call {
        id: Vec<u8>,
        name: Vec<u8>,
        path: String,
    }
    let (contents, path) = contents(body);
    if !contents.is_array() {
        return Ok(());
    }
    let mut pending: Vec<Call> = Vec::new();
    let mut result = Ok(());
    let mut i = 0usize;
    contents.each(|_, content| {
        let index = i;
        i += 1;
        let parts = content.get("parts");
        if !parts.is_array() || parts.raw() == b"[]" || !parts.get("0").exists() {
            if !pending.is_empty() {
                result = err(format!(
                    "{path}[{index}]: content appears before {} pending functionResponse part(s)",
                    pending.len()
                ));
            }
            return result.is_ok();
        }
        let mut calls = Vec::new();
        let mut responses = Vec::new();
        for (j, part) in parts.array().iter().enumerate() {
            let part_path = format!("{path}[{index}].parts[{j}]");
            let call = part.get("functionCall");
            if call.exists() {
                let name = call.get("name").bytes().into_owned();
                if name.is_empty() {
                    result = err(format!("{part_path}: missing functionCall.name"));
                    return false;
                }
                calls.push(Call {
                    id: call.get("id").bytes().into_owned(),
                    name,
                    path: part_path.clone(),
                });
            }
            let response = part.get("functionResponse");
            if response.exists() {
                responses.push((
                    response.get("id").bytes().into_owned(),
                    response.get("name").bytes().into_owned(),
                    part_path,
                ));
            }
        }
        if !calls.is_empty() && !responses.is_empty() {
            result = err(format!(
                "{path}[{index}]: functionCall and functionResponse parts must not be interleaved in the same content"
            ));
            return false;
        }
        if !calls.is_empty() && !pending.is_empty() {
            result = err(format!(
                "{path}[{index}]: functionCall appears before {} pending functionResponse part(s)",
                pending.len()
            ));
            return false;
        }
        if !calls.is_empty() {
            pending = calls;
            return true;
        }
        if responses.is_empty() {
            if !pending.is_empty() && lower_bytes(trim_space(&content.get("role").bytes())) == "model" {
                result = err(format!(
                    "{path}[{index}]: model content appears before {} pending functionResponse part(s)",
                    pending.len()
                ));
            }
            return result.is_ok();
        }
        if pending.is_empty() {
            result = err(format!(
                "{path}[{index}]: functionResponse without preceding functionCall"
            ));
            return false;
        }
        if responses.len() != pending.len() {
            result = err(format!(
                "{path}[{index}]: functionResponse count {} does not match pending functionCall count {}",
                responses.len(),
                pending.len()
            ));
            return false;
        }
        for ((response_id, response_name, part_path), call) in responses.iter().zip(&pending) {
            let failure = if !call.id.is_empty() && response_id.is_empty() {
                Some(format!("{part_path}: missing functionResponse.id for {}", call.path))
            } else if !call.id.is_empty() && *response_id != call.id {
                Some(format!(
                    "{part_path}: functionResponse.id {} does not match functionCall.id {} at {}",
                    crate::gostr::quote(response_id),
                    crate::gostr::quote(&call.id),
                    call.path
                ))
            } else if response_name.is_empty() {
                Some(format!("{part_path}: missing functionResponse.name"))
            } else if !call.name.is_empty() && *response_name != call.name {
                Some(format!(
                    "{part_path}: functionResponse.name {} does not match functionCall.name {} at {}",
                    crate::gostr::quote(response_name),
                    crate::gostr::quote(&call.name),
                    call.path
                ))
            } else {
                None
            };
            if let Some(message) = failure {
                result = err(message);
                return false;
            }
        }
        pending.clear();
        true
    });
    result
}

fn classify_envelope(decoded: &[u8]) -> (GeminiEnvelope, bool) {
    if decoded.is_empty() {
        return (GeminiEnvelope::Unknown, false);
    }
    if is_ascii_uuid(decoded) {
        return (GeminiEnvelope::AsciiUuid, false);
    }
    if field2_envelope(decoded).is_some_and(|len| len > 0) {
        return (GeminiEnvelope::ProtobufField2, true);
    }
    (GeminiEnvelope::Unknown, false)
}

/// `inspectGeminiField2Envelope`: the opaque payload length of `{2: {1: payload}}`.
fn field2_envelope(decoded: &[u8]) -> Option<usize> {
    let value = field2_field1_value(decoded)?;
    (opaque_payload(value) || is_ascii_uuid(value) || tool_invocation_payload(value)).then_some(value.len())
}

fn field2_field1_value(decoded: &[u8]) -> Option<&[u8]> {
    let ((num, typ), n) = wire::consume_tag(decoded).ok()?;
    if num != 2 || typ != BYTES {
        return None;
    }
    let (container, m) = wire::consume_bytes(&decoded[n..]).ok()?;
    if n + m != decoded.len() {
        return None;
    }
    let ((num, typ), n) = wire::consume_tag(container).ok()?;
    if num != 1 || typ != BYTES {
        return None;
    }
    let (value, m) = wire::consume_bytes(&container[n..]).ok()?;
    (n + m == container.len()).then_some(value)
}

fn opaque_payload(value: &[u8]) -> bool {
    value.first() == Some(&0x01)
}

fn tool_invocation_payload(value: &[u8]) -> bool {
    if value.is_empty() {
        return false;
    }
    let mut offset = 0;
    let mut tink = false;
    while offset < value.len() {
        let Ok(((_, typ), n)) = wire::consume_tag(&value[offset..]) else {
            return false;
        };
        offset += n;
        let consumed = match typ {
            VARINT => wire::consume_varint(&value[offset..]).map(|(_, n)| n),
            BYTES => wire::consume_bytes(&value[offset..]).map(|(v, n)| {
                tink |= opaque_payload(v);
                n
            }),
            FIXED32 => wire::consume_fixed32(&value[offset..]).map(|(_, n)| n),
            FIXED64 => wire::consume_fixed64(&value[offset..]).map(|(_, n)| n),
            _ => return false,
        };
        match consumed {
            Ok(n) => offset += n,
            Err(_) => return false,
        }
    }
    tink && offset == value.len()
}

fn is_ascii_uuid(decoded: &[u8]) -> bool {
    super::claude::is_canonical_uuid(decoded)
}
