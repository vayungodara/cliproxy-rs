//! Gemini replay policy for request history (gemini_sanitize.go).

use super::{
    Action, BlockKind, GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR, Provider, compatible_signature_for_provider_block,
    decide_compatibility, is_gemini_bypass, payload_without_provider_prefix,
};
use crate::json::{self, Res};

/// `GeminiReplaySignatureOrBypass`: a Gemini-replayable signature, or the bypass
/// sentinel for missing, unknown or foreign signatures.
pub fn gemini_replay_signature_or_bypass(raw: impl AsRef<[u8]>, block_kind: BlockKind) -> String {
    let raw = raw.as_ref();
    if let Some(signature) = compatible_signature_for_provider_block(Provider::Gemini, raw, block_kind) {
        return signature;
    }
    let decision = decide_compatibility(Provider::Gemini, raw, block_kind);
    if decision.action == Action::ReplaceWithGeminiBypass && !decision.replacement_signature.is_empty() {
        return decision.replacement_signature;
    }
    GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.into()
}

const PART_SIGNATURE_PATHS: [&str; 7] = [
    "thoughtSignature",
    "thought_signature",
    "functionCall.thoughtSignature",
    "functionCall.thought_signature",
    "functionResponse.thoughtSignature",
    "functionResponse.thought_signature",
    "extra_content.google.thought_signature",
];

/// The first signature-carrying field of a part, as Go's `geminiPartThoughtSignature`.
pub(crate) fn part_thought_signature(part: &Res<'_>) -> (Vec<u8>, bool) {
    for path in PART_SIGNATURE_PATHS {
        let v = part.get(path);
        if v.exists() {
            return (v.bytes().into_owned(), true);
        }
    }
    (Vec::new(), false)
}

/// Exactly one top-level string `thoughtSignature` equal to `replay`, and no aliases.
pub(crate) fn has_normalized_part_signature(part: &Res<'_>, replay: &[u8]) -> bool {
    let mut count = 0;
    part.each(|k, _| {
        if &*k.bytes() == b"thoughtSignature" {
            count += 1;
        }
        true
    });
    let canonical = part.get("thoughtSignature");
    if count != 1 || canonical.kind != json::Kind::String || &*canonical.bytes() != replay {
        return false;
    }
    PART_SIGNATURE_PATHS[1..].iter().all(|p| !part.get(p).exists())
}

fn delete_part_signature_fields(payload: &mut Vec<u8>) {
    for path in PART_SIGNATURE_PATHS {
        while json::get(payload, path).exists() {
            match json::try_delete(payload, path) {
                Ok(updated) if updated.len() < payload.len() => *payload = updated,
                _ => break,
            }
        }
    }
}

fn is_server_tool_part(part: &Res<'_>) -> bool {
    ["toolCall", "tool_call", "toolResponse", "tool_response"]
        .iter()
        .any(|k| part.get(k).exists())
}

/// `SanitizeGeminiRequestThoughtSignatures`: keeps native signatures on their parts,
/// echoes server-side tool blocks untouched, gives a missing or foreign first
/// functionCall the bypass sentinel and leaves sibling calls unsigned.
///
/// ponytail: `contents_path` must be a simple dotted path (Go's callers pass `contents`
/// and `request.contents`). A gjson query path is read but not written back, because the
/// sjson port has no `setComplexPath`; add it to `json.rs` if a caller needs one.
pub fn sanitize_gemini_request_thought_signatures(payload: &[u8], contents_path: &str) -> Vec<u8> {
    let path = match contents_path.trim() {
        "" => "contents",
        p => p,
    };
    let contents = json::get(payload, path);
    if !contents.is_array() || !needs_sanitize(&contents) {
        return payload.to_vec();
    }
    let mut changed_any = false;
    let mut items = Vec::new();
    for content in contents.array() {
        let parts = content.get("parts");
        if !parts.is_array() {
            items.push(content.raw().to_vec());
            continue;
        }
        let model_turn = &*content.get("role").bytes() == b"model";
        let mut first_call_seen = false;
        let mut parts_changed = false;
        let mut part_items = Vec::new();
        for part in parts.array() {
            let mut part_json = part.raw().to_vec();
            let (raw, has_sig) = part_thought_signature(&part);
            if part.get("functionResponse").exists() {
                if has_sig {
                    delete_part_signature_fields(&mut part_json);
                    parts_changed = true;
                }
                part_items.push(part_json);
                continue;
            }
            if !model_turn || is_server_tool_part(&part) {
                part_items.push(part_json);
                continue;
            }
            let has_call = part.get("functionCall").exists();
            let first_call = has_call && !first_call_seen;
            if has_call {
                first_call_seen = true;
            }
            if !has_call && !has_sig {
                part_items.push(part_json);
                continue;
            }
            let kind = if has_call {
                BlockKind::GeminiFunctionCall
            } else {
                BlockKind::GeminiModelPart
            };
            let decision = decide_compatibility(Provider::Gemini, &raw, kind);
            let replay = if first_call {
                gemini_replay_signature_or_bypass(&raw, kind)
            } else if has_sig
                && decision.action == Action::Preserve
                && !is_gemini_bypass(payload_without_provider_prefix(&raw))
            {
                decision.normalized_signature.clone()
            } else {
                String::new()
            };
            if !replay.is_empty() {
                if !has_normalized_part_signature(&part, replay.as_bytes()) {
                    delete_part_signature_fields(&mut part_json);
                    json::set_str(&mut part_json, "thoughtSignature", &replay);
                    parts_changed = true;
                }
            } else if has_sig {
                delete_part_signature_fields(&mut part_json);
                parts_changed = true;
            }
            part_items.push(part_json);
        }
        if parts_changed {
            let mut updated = content.raw().to_vec();
            json::set_raw(&mut updated, "parts", json::join(&part_items));
            items.push(updated);
            changed_any = true;
        } else {
            items.push(content.raw().to_vec());
        }
    }
    if !changed_any {
        return payload.to_vec();
    }
    let mut out = payload.to_vec();
    if json::try_set_raw(payload, path, json::join(&items))
        .map(|next| out = next)
        .is_err()
    {
        return payload.to_vec();
    }
    out
}

fn needs_sanitize(contents: &Res<'_>) -> bool {
    for content in contents.array() {
        let parts = content.get("parts");
        if !parts.is_array() {
            continue;
        }
        let model_turn = &*content.get("role").bytes() == b"model";
        let mut first_call_seen = false;
        for part in parts.array() {
            let (raw, has_sig) = part_thought_signature(&part);
            if part.get("functionResponse").exists() {
                if has_sig {
                    return true;
                }
                continue;
            }
            if !model_turn || is_server_tool_part(&part) {
                continue;
            }
            let has_call = part.get("functionCall").exists();
            let first_call = has_call && !first_call_seen;
            if has_call {
                first_call_seen = true;
            }
            if first_call {
                let replay = gemini_replay_signature_or_bypass(&raw, BlockKind::GeminiFunctionCall);
                if !has_normalized_part_signature(&part, replay.as_bytes()) {
                    return true;
                }
                continue;
            }
            if !has_sig {
                continue;
            }
            let kind = if has_call {
                BlockKind::GeminiFunctionCall
            } else {
                BlockKind::GeminiModelPart
            };
            let decision = decide_compatibility(Provider::Gemini, &raw, kind);
            if decision.action != Action::Preserve || is_gemini_bypass(payload_without_provider_prefix(&raw)) {
                return true;
            }
            if !has_normalized_part_signature(&part, decision.normalized_signature.as_bytes()) {
                return true;
            }
        }
    }
    false
}
