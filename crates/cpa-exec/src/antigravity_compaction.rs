//! Responses compaction for Antigravity (helps/antigravity_compaction.go): a summary
//! turn sealed into an opaque `compaction` item, and expanded again when a later
//! request carries it.
//!
//! Capsules are AES-256-GCM under a key derived from a fixed secret, so any
//! CLIProxyAPI instance (Go or Rust) can open another's: `cpa-ag-compact-v1:` then
//! unpadded URL-safe base64 of nonce, ciphertext and tag.

use btls::symm::{Cipher, decrypt_aead, encrypt_aead};
use cpa_common::json as gj;
use sha2::{Digest, Sha256};

const PREFIX: &str = "cpa-ag-compact-v1:";
const KEY_SECRET: &[u8] = b"CLIProxyAPI";
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const SUMMARY_PROMPT: &str = r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"Please provide a concise and comprehensive summary of the preceding conversation and task progress so far, including user goals, key findings, actions taken, and current status, so that work can continue smoothly."}]}"#;

fn has_input_type(payload: &[u8], kind: &[u8]) -> bool {
    let input = gj::get(payload, "input");
    input.is_array() && input.array().iter().any(|item| *item.get("type").bytes() == *kind)
}

/// `HasResponsesCompactionTrigger`.
pub(crate) fn has_trigger(payload: &[u8]) -> bool {
    has_input_type(payload, b"compaction_trigger")
}

/// `HasResponsesCompactionItem`.
pub(crate) fn has_item(payload: &[u8]) -> bool {
    has_input_type(payload, b"compaction")
}

/// The `fmt.Errorf` of a failed summary extraction or seal: no status and not a
/// transport fault, so the scheduler classifies it like any statusless model failure.
pub(crate) fn statusless(message: impl Into<String>) -> cpa_core::exec::ExecError {
    cpa_core::exec::ExecError::local(0, cpa_core::exec::FailureScope::Model, message)
}

/// `ResponsesCompactionSourcePayload`: the body, unless only the original request
/// carries the trigger or the body is empty.
pub(crate) fn source_payload<'a>(body: &'a [u8], original: &'a [u8]) -> &'a [u8] {
    if (!has_trigger(body) && has_trigger(original)) || (body.is_empty() && !original.is_empty()) {
        original
    } else {
        body
    }
}

/// `RecognizedAntigravityCompactionCapsule`.
fn is_capsule(item: &gj::Res<'_>) -> bool {
    item.get("encrypted_content").str().trim().starts_with(PREFIX)
}

/// `DropForeignResponsesCompactionItems`: compaction items whose `encrypted_content`
/// is not a CPA capsule (`RecognizedAntigravityCompactionCapsule`) are removed. `None`
/// when nothing was dropped.
fn drop_foreign(payload: &[u8], provider: &str) -> Option<Vec<u8>> {
    let input = gj::get(payload, "input");
    if !input.is_array() {
        return None;
    }
    let items = input.array();
    let kept: Vec<&[u8]> = items
        .iter()
        .filter(|item| *item.get("type").bytes() != *b"compaction" || is_capsule(item))
        .map(|item| item.raw())
        .collect();
    let dropped = items.len() - kept.len();
    if dropped == 0 {
        return None;
    }
    tracing::warn!("{provider} compaction: dropped {dropped} non-CPA compaction item(s)");
    let mut out = payload.to_vec();
    gj::set_raw(&mut out, "input", gj::join(&kept));
    Some(out)
}

/// `ExpandResponsesCompactionCapsules`: drops foreign compaction items and expands CPA
/// capsules in the body and the original request.
pub(crate) fn expand_request(req: &mut cpa_core::exec::ExecRequest, provider: &str) -> Result<(), String> {
    rewrite_request(req, |payload| {
        let dropped = drop_foreign(payload, provider);
        let current = dropped.as_deref().unwrap_or(payload);
        if has_item(current) {
            return expand(current).map(Some);
        }
        Ok(dropped)
    })
}

/// cliproxy-rs only, for OpenAI-compatible upstreams (Go never expands there): expands
/// CPA capsules in the body and the original request and leaves every other
/// compaction item as it is, so a native `/responses/compact` context reaches the
/// upstream unchanged.
pub(crate) fn expand_capsules(req: &mut cpa_core::exec::ExecRequest) -> Result<(), String> {
    rewrite_request(req, |payload| expand_items(payload, true))
}

/// Applies `one` to the body and the original request; `None` leaves a payload as it is.
fn rewrite_request(
    req: &mut cpa_core::exec::ExecRequest,
    one: impl Fn(&[u8]) -> Result<Option<Vec<u8>>, String>,
) -> Result<(), String> {
    let one = |payload: &bytes::Bytes| one(&payload[..]).map(|out| out.map(bytes::Bytes::from));
    let same_original = !req.original_body.is_empty() && req.body == req.original_body;
    if let Some(body) = one(&req.body)? {
        req.body = body;
    }
    if same_original {
        req.original_body = req.body.clone();
    } else if !req.original_body.is_empty()
        && let Some(original) = one(&req.original_body)?
    {
        req.original_body = original;
    }
    Ok(())
}

/// `PrepareAntigravityCompactionSummaryPayload`: the input without triggers plus the
/// summary prompt, without tools and other fields summary generation does not take.
pub(crate) fn summary_payload(payload: &[u8]) -> Vec<u8> {
    let mut out = payload.to_vec();
    let input = gj::get(payload, "input");
    let items: Vec<Vec<u8>> = if input.is_array() {
        let mut items: Vec<Vec<u8>> = input
            .array()
            .iter()
            .filter(|item| *item.get("type").bytes() != *b"compaction_trigger")
            .map(|item| item.raw().to_vec())
            .collect();
        items.push(SUMMARY_PROMPT.as_bytes().to_vec());
        items
    } else if input.kind == gj::Kind::String && !input.bytes().is_empty() {
        let mut message = br#"{"type":"message","role":"user","content":[{"type":"input_text"}]}"#.to_vec();
        gj::set_str(&mut message, "content.0.text", input.bytes());
        vec![message, SUMMARY_PROMPT.as_bytes().to_vec()]
    } else {
        vec![SUMMARY_PROMPT.as_bytes().to_vec()]
    };
    gj::set_raw(&mut out, "input", gj::join(&items));
    for field in [
        "stream",
        "tools",
        "tool_choice",
        "previous_response_id",
        "parallel_tool_calls",
        "additional_tools",
        "truncation",
        "metadata",
    ] {
        gj::delete(&mut out, field);
    }
    gj::set_bool(&mut out, "stream", false);
    out
}

fn key() -> [u8; 32] {
    Sha256::digest(KEY_SECRET).into()
}

/// `SealAntigravityCompaction`.
pub(crate) fn seal(summary: &str, model: &str) -> Result<String, String> {
    let mut plaintext = b"{\"summary\":".to_vec();
    gj::marshal_str(&mut plaintext, summary.as_bytes(), true);
    plaintext.extend_from_slice(b",\"model\":");
    gj::marshal_str(&mut plaintext, model.as_bytes(), true);
    plaintext.extend_from_slice(format!(",\"created_at\":{}}}", chrono::Utc::now().timestamp()).as_bytes());
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|e| format!("generate nonce: {e}"))?;
    let mut tag = [0u8; TAG_LEN];
    let ciphertext = encrypt_aead(Cipher::aes_256_gcm(), &key(), Some(&nonce), &[], &plaintext, &mut tag)
        .map_err(|e| format!("create gcm: {e}"))?;
    let sealed = [&nonce[..], &ciphertext, &tag].concat();
    use base64::Engine;
    Ok(format!(
        "{PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sealed)
    ))
}

/// `UnsealAntigravityCompaction`, with Go's error text.
pub(crate) fn unseal(capsule: &str) -> Result<String, String> {
    let Some(raw) = capsule.strip_prefix(PREFIX) else {
        return Err("unrecognized compaction capsule format".into());
    };
    let sealed = cpa_common::signature::raw_url_base64_decode(raw.as_bytes())
        .map_err(|e| format!("decode compaction capsule: {e}"))?;
    if sealed.len() < NONCE_LEN + TAG_LEN {
        return Err("compaction capsule ciphertext too short".into());
    }
    let (nonce, rest) = sealed.split_at(NONCE_LEN);
    let (ciphertext, tag) = rest.split_at(rest.len() - TAG_LEN);
    let plaintext = decrypt_aead(Cipher::aes_256_gcm(), &key(), Some(nonce), &[], ciphertext, tag)
        .map_err(|_| "invalid or corrupted compaction capsule: cipher: message authentication failed".to_owned())?;
    // ponytail: json.Unmarshal's error text for a forged capsule (one that decrypts but
    // is not the capsule JSON) is not reproduced; Go and Rust capsules always parse.
    if !gj::std_valid(&plaintext) || !gj::parse(&plaintext).is_object() {
        return Err("unmarshal compaction capsule: invalid capsule data".into());
    }
    Ok(gj::get(&plaintext, "summary").str().into_owned())
}

/// `ExpandAntigravityCompactionCapsules`: each compaction item becomes a developer
/// message carrying its summary.
pub(crate) fn expand(payload: &[u8]) -> Result<Vec<u8>, String> {
    expand_items(payload, false).map(|out| out.unwrap_or_else(|| payload.to_vec()))
}

/// [`expand`]; `only_capsules` leaves compaction items that are not CPA capsules in
/// place. `None` when nothing changed.
fn expand_items(payload: &[u8], only_capsules: bool) -> Result<Option<Vec<u8>>, String> {
    let input = gj::get(payload, "input");
    if !input.is_array() {
        return Ok(None);
    }
    let mut changed = false;
    let mut items = Vec::new();
    for item in input.array() {
        if *item.get("type").bytes() != *b"compaction" || (only_capsules && !is_capsule(&item)) {
            items.push(item.raw().to_vec());
            continue;
        }
        let summary =
            unseal(&item.get("encrypted_content").str()).map_err(|e| format!("invalid compaction capsule: {e}"))?;
        let mut message = br#"{"type":"message","role":"developer","content":[{"type":"input_text"}]}"#.to_vec();
        gj::set_str(
            &mut message,
            "content.0.text",
            format!("Context summary from previous turns:\n{summary}"),
        );
        items.push(message);
        changed = true;
    }
    if !changed {
        return Ok(None);
    }
    gj::try_set_raw(payload, "input", gj::join(&items)).map(Some)
}

/// `ExtractAntigravitySummaryText`: Responses output text, else Gemini parts (no
/// thoughts), else Claude text blocks, else the chat message.
pub(crate) fn summary_text(response: &[u8]) -> Result<String, String> {
    let join = |parts: Vec<String>| (!parts.is_empty()).then(|| parts.join("\n"));
    let output = gj::get(response, "output");
    if output.is_array() {
        let mut texts = Vec::new();
        for item in output.array() {
            if *item.get("type").bytes() != *b"message" {
                continue;
            }
            let content = item.get("content");
            if content.is_array() {
                for part in content.array() {
                    let text = part.get("text").str().into_owned();
                    let kind = part.get("type").bytes();
                    if (*kind == *b"output_text" || *kind == *b"text") && !text.is_empty() {
                        texts.push(text);
                    }
                }
            } else if content.kind == gj::Kind::String && !content.bytes().is_empty() {
                texts.push(content.str().into_owned());
            }
        }
        if let Some(text) = join(texts) {
            return Ok(text);
        }
    }
    let mut candidates = gj::get(response, "response.candidates");
    if !candidates.exists() {
        candidates = gj::get(response, "candidates");
    }
    if candidates.is_array()
        && let Some(first) = candidates.array().first()
    {
        let parts = first.get("content.parts");
        let texts: Vec<String> = if parts.is_array() {
            parts
                .array()
                .iter()
                .filter(|p| !p.get("thought").bool())
                .map(|p| p.get("text").str().into_owned())
                .filter(|t| !t.is_empty())
                .collect()
        } else {
            Vec::new()
        };
        if let Some(text) = join(texts) {
            return Ok(text);
        }
    }
    let content = gj::get(response, "content");
    if content.is_array() {
        let texts: Vec<String> = content
            .array()
            .iter()
            .filter(|b| *b.get("type").bytes() == *b"text")
            .map(|b| b.get("text").str().into_owned())
            .filter(|t| !t.is_empty())
            .collect();
        if let Some(text) = join(texts) {
            return Ok(text);
        }
    }
    let chat = gj::get(response, "choices.0.message.content").str().into_owned();
    if !chat.is_empty() {
        return Ok(chat);
    }
    Err("no summary text found in upstream response".into())
}

/// Input, output and total tokens of a Chat completion's `usage`: the summary answer
/// of an OpenAI-compatible upstream is always a Chat completion.
pub(crate) fn chat_usage(response: &[u8]) -> (i64, i64, i64) {
    let usage = gj::get(response, "usage");
    (
        usage.get("prompt_tokens").int(),
        usage.get("completion_tokens").int(),
        usage.get("total_tokens").int(),
    )
}

/// Token counts of the summary answer: its Responses usage, else Go's OpenAI usage
/// parse when both input and total are zero.
pub(crate) fn summary_usage(response: &[u8]) -> (i64, i64, i64) {
    let field = |path: &str| gj::get(response, path).int();
    let (input, output, total) = (
        field("usage.input_tokens"),
        field("usage.output_tokens"),
        field("usage.total_tokens"),
    );
    if total != 0 || input != 0 {
        return (input, output, total);
    }
    let usage = gj::get(response, "usage");
    let bucket = [
        "prompt_tokens",
        "input_tokens",
        "completion_tokens",
        "output_tokens",
        "prompt_tokens_details.cached_tokens",
        "input_tokens_details.cached_tokens",
        "prompt_tokens_details.cache_write_tokens",
        "prompt_tokens_details.cache_creation_tokens",
        "input_tokens_details.cache_write_tokens",
        "input_tokens_details.cache_creation_tokens",
        "completion_tokens_details.reasoning_tokens",
        "output_tokens_details.reasoning_tokens",
    ];
    if !usage.is_object() || !(usage.get("total_tokens").exists() || bucket.iter().any(|p| usage.get(p).exists())) {
        return (0, 0, 0);
    }
    let first = |a: &str, b: &str| {
        let node = usage.get(a);
        if node.exists() { node.int() } else { usage.get(b).int() }
    };
    (
        first("prompt_tokens", "input_tokens"),
        first("completion_tokens", "output_tokens"),
        usage.get("total_tokens").int(),
    )
}

fn nano_id(prefix: &str) -> String {
    format!(
        "{prefix}{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    )
}

fn item(status: &str, id: &str, capsule: &str) -> Vec<u8> {
    let mut item = format!(r#"{{"type":"compaction","status":"{status}"}}"#).into_bytes();
    gj::set_str(&mut item, "id", id);
    gj::set_str(&mut item, "encrypted_content", capsule);
    item
}

fn usage(input: i64, output: i64, total: i64) -> Vec<u8> {
    let mut usage =
        br#"{"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}"#.to_vec();
    gj::set_int(&mut usage, "input_tokens", input);
    gj::set_int(&mut usage, "output_tokens", output);
    gj::set_int(&mut usage, "total_tokens", total);
    usage
}

/// `BuildAntigravityCompactionResponse`.
pub(crate) fn response(model: &str, capsule: &str, (input, output, total): (i64, i64, i64)) -> Vec<u8> {
    let now = chrono::Utc::now().timestamp();
    let response_id = nano_id("resp_ag_compact_");
    let item = item("completed", &nano_id("cmp_ag_compact_"), capsule);
    let mut out = br#"{"object":"response.compaction","status":"completed"}"#.to_vec();
    gj::set_str(&mut out, "id", response_id);
    gj::set_int(&mut out, "created_at", now);
    gj::set_str(&mut out, "model", model);
    gj::set_raw(&mut out, "output", gj::join(&[item]));
    gj::set_raw(&mut out, "usage", usage(input, output, total));
    out
}

fn frame(event: &str, data: &[u8]) -> Vec<u8> {
    [b"event: ", event.as_bytes(), b"\ndata: ", data, b"\n\n"].concat()
}

/// `BuildAntigravityCompactionStreamChunks`: the five Responses events of one
/// compaction item.
pub(crate) fn stream_chunks(model: &str, capsule: &str, (input, output, total): (i64, i64, i64)) -> Vec<Vec<u8>> {
    let now = chrono::Utc::now().timestamp();
    let response_id = nano_id("resp_ag_compact_");
    let item_id = nano_id("cmp_ag_compact_");
    let in_progress = item("in_progress", &item_id, capsule);
    let completed = item("completed", &item_id, capsule);

    let mut created =
        br#"{"object":"response","status":"in_progress","background":false,"error":null,"output":[]}"#.to_vec();
    gj::set_str(&mut created, "id", &response_id);
    gj::set_int(&mut created, "created_at", now);
    gj::set_str(&mut created, "model", model);

    let mut done = br#"{"object":"response","status":"completed","background":false,"error":null}"#.to_vec();
    gj::set_str(&mut done, "id", &response_id);
    gj::set_int(&mut done, "created_at", now);
    gj::set_int(&mut done, "completed_at", now);
    gj::set_str(&mut done, "model", model);
    gj::set_raw(&mut done, "output", gj::join(&[&completed]));
    gj::set_raw(&mut done, "usage", usage(input, output, total));

    let event = |template: &[u8], key: &str, value: &[u8]| {
        let mut out = template.to_vec();
        gj::set_raw(&mut out, key, value);
        out
    };
    vec![
        frame(
            "response.created",
            &event(
                br#"{"type":"response.created","sequence_number":0}"#,
                "response",
                &created,
            ),
        ),
        frame(
            "response.in_progress",
            &event(
                br#"{"type":"response.in_progress","sequence_number":1}"#,
                "response",
                &created,
            ),
        ),
        frame(
            "response.output_item.added",
            &event(
                br#"{"type":"response.output_item.added","sequence_number":2,"output_index":0}"#,
                "item",
                &in_progress,
            ),
        ),
        frame(
            "response.output_item.done",
            &event(
                br#"{"type":"response.output_item.done","sequence_number":3,"output_index":0}"#,
                "item",
                &completed,
            ),
        ),
        frame(
            "response.completed",
            &event(
                br#"{"type":"response.completed","sequence_number":4}"#,
                "response",
                &done,
            ),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capsules_round_trip_and_reject_tampering() {
        let capsule = seal("plan <a> & \"b\"", "gemini-3-flash").unwrap();
        assert!(capsule.starts_with(PREFIX));
        assert_eq!(unseal(&capsule).unwrap(), "plan <a> & \"b\"");
        // A character inside the ciphertext: the last one may carry only padding bits,
        // which the decoder ignores.
        let mut tampered = capsule.clone().into_bytes();
        let middle = PREFIX.len() + (tampered.len() - PREFIX.len()) / 2;
        tampered[middle] = if tampered[middle] == b'A' { b'B' } else { b'A' };
        assert_eq!(
            unseal(std::str::from_utf8(&tampered).unwrap()).unwrap_err(),
            "invalid or corrupted compaction capsule: cipher: message authentication failed"
        );
        assert_eq!(unseal("x").unwrap_err(), "unrecognized compaction capsule format");
        assert_eq!(
            unseal("cpa-ag-compact-v1:AA=A").unwrap_err(),
            "decode compaction capsule: illegal base64 data at input byte 2"
        );
    }
}
