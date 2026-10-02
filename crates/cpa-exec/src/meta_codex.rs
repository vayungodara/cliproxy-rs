//! Adapters for the Codex request helpers the Meta executor calls but this thread does
//! not own, plus Go's `bytes.TrimSpace`. Each names its owner; at integration the owner's
//! function replaces it and the Meta fixtures (tests/device_fixtures/meta) are the
//! acceptance test.

use cpa_common::json::{self as gj, Kind};
use http::HeaderMap;

/// Go `bytes.TrimSpace`: ASCII space, `\v` and Unicode White_Space at both ends.
pub(crate) fn go_trim_space(b: &[u8]) -> &[u8] {
    if let Ok(text) = std::str::from_utf8(b) {
        return text.trim().as_bytes();
    }
    let space = |c: &u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let start = b.iter().position(|c| !space(c)).unwrap_or(b.len());
    let end = b.iter().rposition(|c| !space(c)).map_or(start, |e| e + 1);
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

/// ponytail: adapter, owner the server thread (cpa-common::payload
/// `NormalizeCodexToolIntegerTypes`). Identity until it lands: Go only rewrites tool
/// schemas for Codex CLI User-Agents.
pub(crate) fn normalize_codex_tool_integer_types(body: Vec<u8>, _headers: &HeaderMap) -> Vec<u8> {
    body
}
