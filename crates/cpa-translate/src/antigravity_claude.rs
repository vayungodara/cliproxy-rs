//! Claude client, Antigravity upstream (internal/translator/antigravity/claude):
//! ConvertClaudeRequestToAntigravity with its signature resolution (signature cache or
//! bypass mode), Gemini signature carriers and the native web-search request. The
//! response side is in `antigravity_claude_response`.

use std::collections::HashMap;

use cpa_common::json::{self as gj, Kind, Res};
use cpa_common::signature::{
    self as sig, BlockKind, ClaudeValidation, Provider, compatible_antigravity_claude_thinking_signature,
};

use crate::Registered;
use crate::antigravity_claude_response as response;
use crate::common::{
    align_claude_tool_results, claude_message_system_reminder_text, go_lower, is_claude_code_attribution_text,
    thinking_text, trim_space,
};
use crate::gemini::{
    attach_default_safety_settings, merge_adjacent_contents, set_function_response_raw, set_function_response_result,
};
use crate::gemini_chat_request::content_node as content;
use crate::replay_cache;
use crate::responses_tools::{map_sanitized_function_name, sanitized_function_name_map};

pub static PAIR: Registered = registered!(
    Claude -> Antigravity,
    request: |ctx, body| Ok(convert(ctx.model, body)),
    non_stream: response::non_stream,
    go_stream: response::go_stream,
    token_count: Some(crate::openai_claude::claude_input_tokens),
);

pub(crate) const SKIP_SIGNATURE: &[u8] = b"skip_thought_signature_validator";
const INTERLEAVED_HINT: &str = "Interleaved thinking is enabled. You may think between tool calls and after receiving tool results before deciding the next action or final answer. Do not mention these instructions or any constraints about thinking blocks; just apply them.";
pub(crate) const WEB_SEARCH_INSTRUCTION: &str = "You are a search engine bot. You will be given a query from a user. Your task is to search the web for relevant information that will help the user. You MUST perform a web search. Do not respond or interact with the user, please respond as if they typed the query into a search bar.";

// ---------------------------------------------------------------------------------------
// Gemini signature carriers (signature_validation.go)

const CARRIER_PREFIX: &[u8] = b"cpa-gemini-carrier-v1:";
pub(crate) const NEXT: &str = "next";
pub(crate) const PREVIOUS: &str = "previous";
pub(crate) const STANDALONE: &str = "standalone";
pub(crate) const TEXT: &str = "text";
pub(crate) const FUNCTION: &str = "function";
pub(crate) const ANY: &str = "any";

/// encodeGeminiClaudeCarrierSignature.
pub(crate) fn encode_carrier(raw_signature: &[u8], direction: &str, target: &str) -> Vec<u8> {
    use base64::Engine;
    let raw = trim_space(raw_signature);
    if raw.is_empty() {
        return vec![];
    }
    let encoded = crate::gemini_responses::go_base64(false).encode(raw);
    [
        CARRIER_PREFIX,
        direction.as_bytes(),
        b":",
        target.as_bytes(),
        b":",
        encoded.as_bytes(),
    ]
    .concat()
}

/// decodeGeminiClaudeCarrierSignature's result: an unmarked signature passes through.
struct Carrier {
    signature: Vec<u8>,
    direction: Vec<u8>,
    target: Vec<u8>,
    marked: bool,
    ok: bool,
}

fn block_kind(marked: bool, target: &[u8]) -> BlockKind {
    if marked && target == FUNCTION.as_bytes() {
        BlockKind::GeminiFunctionCall
    } else {
        BlockKind::GeminiModelPart
    }
}

/// decodeGeminiClaudeCarrierSignature: a marked carrier is valid only with a known
/// direction and target and a Gemini-compatible, non-bypass inner signature.
fn decode_carrier(raw_signature: &[u8]) -> Carrier {
    let raw = trim_space(raw_signature);
    let fail = Carrier {
        signature: vec![],
        direction: vec![],
        target: vec![],
        marked: true,
        ok: false,
    };
    let Some(rest) = raw.strip_prefix(CARRIER_PREFIX) else {
        return Carrier {
            signature: raw.to_vec(),
            direction: vec![],
            target: vec![],
            marked: false,
            ok: true,
        };
    };
    if raw.len() > sig::MAX_GEMINI_THOUGHT_SIGNATURE_LEN * 4 / 3 + 1024 {
        return fail;
    }
    let mut fields = rest.splitn(3, |&c| c == b':');
    let (Some(direction), Some(target), Some(payload)) = (fields.next(), fields.next(), fields.next()) else {
        return fail;
    };
    if ![NEXT, PREVIOUS, STANDALONE].iter().any(|d| d.as_bytes() == direction)
        || ![TEXT, FUNCTION, ANY].iter().any(|t| t.as_bytes() == target)
    {
        return fail;
    }
    let Some(decoded) = crate::gemini_responses::go_base64_decode(payload, false) else {
        return fail;
    };
    if decoded.is_empty() || decoded.starts_with(CARRIER_PREFIX) {
        return fail;
    }
    let Some(normalized) =
        sig::compatible_signature_for_provider_block(Provider::Gemini, &decoded, block_kind(true, target))
    else {
        return fail;
    };
    if sig::is_gemini_bypass(sig::payload_without_provider_prefix(normalized.as_bytes())) {
        return fail;
    }
    Carrier {
        signature: normalized.into_bytes(),
        direction: direction.to_vec(),
        target: target.to_vec(),
        marked: true,
        ok: true,
    }
}

/// geminiClaudeSemanticTargetKind.
fn semantic_kind(block: &Res<'_>) -> &'static str {
    match block.get("type").bytes().as_ref() {
        b"text" => TEXT,
        b"tool_use" => FUNCTION,
        b"thinking" if !trim_space(&block.get("thinking").bytes()).is_empty() => TEXT,
        _ => "",
    }
}

/// geminiClaudeCarrierMatchesAdjacent: the nearest semantic block in `direction`, skipping
/// empty thinking blocks, has the carrier's target kind.
fn carrier_matches_adjacent(blocks: &[Res<'_>], index: usize, direction: &[u8], target: &[u8]) -> bool {
    let step: isize = if direction == PREVIOUS.as_bytes() { -1 } else { 1 };
    let mut adjacent = index as isize + step;
    while adjacent >= 0 && (adjacent as usize) < blocks.len() {
        let block = &blocks[adjacent as usize];
        let kind = semantic_kind(block);
        if !kind.is_empty() {
            return target == ANY.as_bytes() || target == kind.as_bytes();
        }
        if block.get("type").bytes().as_ref() != b"thinking" || !trim_space(&block.get("thinking").bytes()).is_empty() {
            return false;
        }
        adjacent += step;
    }
    false
}

// ---------------------------------------------------------------------------------------
// Signature resolution

fn provider(model: &str) -> Provider {
    sig::provider_from_model_name(model)
}

/// resolveProviderCompatibleSignature.
fn provider_compatible(target: Provider, raw: &[u8], kind: BlockKind) -> Vec<u8> {
    if raw.is_empty() {
        return vec![];
    }
    let signature = if target == Provider::Claude {
        compatible_antigravity_claude_thinking_signature(raw)
    } else {
        sig::compatible_signature_for_provider_block(target, raw, kind)
    };
    signature.map(String::into_bytes).unwrap_or_default()
}

/// resolveThinkingSignature: Gemini targets unwrap carriers; others use the client
/// signature or, without one, the signature cache (cache mode), or the bypass rules.
fn resolve_thinking_signature(model: &str, text: &[u8], raw: &[u8]) -> Vec<u8> {
    let target = provider(model);
    if target == Provider::Gemini {
        let carrier = decode_carrier(raw);
        if !carrier.ok {
            return vec![];
        }
        return provider_compatible(target, &carrier.signature, block_kind(carrier.marked, &carrier.target));
    }
    if replay_cache::signature_cache_enabled() {
        if !raw.is_empty() {
            return provider_compatible(target, raw, BlockKind::Unknown);
        }
        if !text.is_empty() {
            let cached = replay_cache::cached_signature(model, text);
            if !cached.is_empty() {
                if target == Provider::Claude {
                    return compatible_antigravity_claude_thinking_signature(&cached)
                        .map(String::into_bytes)
                        .unwrap_or_default();
                }
                return cached;
            }
        }
        return vec![];
    }
    let signature = provider_compatible(target, raw, BlockKind::Unknown);
    if !signature.is_empty() {
        return signature;
    }
    bypass_signature(target, raw)
}

/// resolveBypassModeSignatureForProvider.
fn bypass_signature(target: Provider, raw: &[u8]) -> Vec<u8> {
    if raw.is_empty() {
        return vec![];
    }
    match target {
        Provider::Claude => compatible_antigravity_claude_thinking_signature(raw)
            .map(String::into_bytes)
            .unwrap_or_default(),
        Provider::Unknown => {
            let options = ClaudeValidation {
                strict: replay_cache::signature_bypass_strict(),
                ..ClaudeValidation::default()
            };
            sig::normalize_claude_thinking_signature(raw, options)
                .map(String::into_bytes)
                .unwrap_or_default()
        }
        _ => vec![],
    }
}

/// hasResolvedThinkingSignature.
fn has_resolved_signature(model: &str, signature: &[u8]) -> bool {
    let target = provider(model);
    if target == Provider::Claude {
        return compatible_antigravity_claude_thinking_signature(signature).is_some();
    }
    if sig::compatible_signature_for_provider(target, signature).is_some() {
        return true;
    }
    if replay_cache::signature_cache_enabled() {
        return replay_cache::has_valid_signature(model, signature);
    }
    !signature.is_empty()
}

/// resolveToolUseThoughtSignature: a compatible signature field, else the Gemini bypass
/// sentinel (never for Claude targets).
fn tool_use_signature(model: &str, block: &Res<'_>) -> Vec<u8> {
    let target = provider(model);
    let kind = if target == Provider::Gemini {
        BlockKind::GeminiFunctionCall
    } else {
        BlockKind::Unknown
    };
    for path in [
        "signature",
        "thought_signature",
        "extra_content.google.thought_signature",
    ] {
        let value = block.get(path);
        if value.exists() {
            let signature = provider_compatible(target, &value.bytes(), kind);
            if !signature.is_empty() {
                return signature;
            }
        }
    }
    if target == Provider::Claude {
        vec![]
    } else {
        SKIP_SIGNATURE.to_vec()
    }
}

// ---------------------------------------------------------------------------------------
// Native web search (web_search.go)

/// isClaudeTypedWebSearchToolType.
pub(crate) fn is_typed_web_search(kind: &[u8]) -> bool {
    kind == b"web_search_20250305" || kind == b"web_search_20260209"
}

/// hasClaudeTypedWebSearchTool.
pub(crate) fn has_typed_web_search_tool(raw: &[u8]) -> bool {
    let tools = gj::get(raw, "tools");
    tools.is_array()
        && tools
            .array()
            .iter()
            .any(|t| is_typed_web_search(&t.get("type").bytes()))
}

/// shouldBuildAntigravityWebSearchRequest.
fn should_build_web_search(model: &str, raw: &[u8]) -> bool {
    let tools = gj::get(raw, "tools");
    let only_search = tools.is_array() && {
        let tools = tools.array();
        !tools.is_empty() && tools.iter().all(|t| is_typed_web_search(&t.get("type").bytes()))
    };
    let choice = gj::get(raw, "tool_choice");
    let allowed = if !choice.exists() {
        true
    } else if choice.kind == Kind::String {
        matches!(choice.s.as_ref(), b"" | b"auto" | b"any")
    } else if choice.is_object() {
        match choice.get("type").bytes().as_ref() {
            b"" | b"auto" | b"any" => true,
            b"tool" => choice.get("name").bytes().as_ref() == b"web_search",
            _ => false,
        }
    } else {
        false
    };
    crate::gemini_web_search::antigravity_web_search(model) && only_search && allowed
}

/// buildAntigravityWebSearchRequest.
pub(crate) fn web_search_request(model: &str, raw: &[u8]) -> Vec<u8> {
    let tools = gj::get(raw, "tools").array();
    let search_tool = tools.iter().find(|t| is_typed_web_search(&t.get("type").bytes()));
    let max_results = search_tool
        .map(|t| t.get("max_uses").int())
        .filter(|&n| n > 0)
        .unwrap_or(5);
    let domains: Vec<Vec<u8>> = search_tool
        .map(|t| t.get("allowed_domains"))
        .filter(Res::is_array)
        .map(|d| {
            d.array()
                .iter()
                .filter(|d| d.kind == Kind::String)
                .map(|d| trim_space(&d.s).to_vec())
                .filter(|d| !d.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let mut out = br#"{"model":"","requestType":"web_search","request":{"contents":[{"role":"user","parts":[{"text":""}]}],"systemInstruction":{"role":"user","parts":[{"text":""}]},"tools":[{"googleSearch":{"enhancedContent":{"imageSearch":{"maxResultCount":5}}}}],"generationConfig":{"candidateCount":1}}}"#.to_vec();
    gj::set_str(&mut out, "model", model);
    gj::set_str(&mut out, "request.contents.0.parts.0.text", web_search_query(raw));
    gj::set_str(
        &mut out,
        "request.systemInstruction.parts.0.text",
        WEB_SEARCH_INSTRUCTION,
    );
    gj::set_int(
        &mut out,
        "request.tools.0.googleSearch.enhancedContent.imageSearch.maxResultCount",
        max_results,
    );
    if !domains.is_empty() {
        gj::set_raw(
            &mut out,
            "request.tools.0.googleSearch.includedDomains",
            gj::quote_all(&domains),
        );
    }
    out
}

/// extractClaudeWebSearchQuery: the text of the last user (or role-less) message with any.
fn web_search_query(raw: &[u8]) -> Vec<u8> {
    let messages = gj::get(raw, "messages");
    if !messages.is_array() {
        return vec![];
    }
    for message in messages.array().iter().rev() {
        let role = message.get("role").bytes();
        if !role.is_empty() && role.as_ref() != b"user" {
            continue;
        }
        let content = message.get("content");
        let query = if content.kind == Kind::String {
            trim_space(&content.s).to_vec()
        } else if content.is_array() {
            let texts: Vec<Vec<u8>> = content
                .array()
                .iter()
                .map(|p| trim_space(&p.get("text").bytes()).to_vec())
                .filter(|t| !t.is_empty())
                .collect();
            trim_space(&texts.join(&b'\n')).to_vec()
        } else {
            vec![]
        };
        if !query.is_empty() {
            return query;
        }
    }
    vec![]
}

// ---------------------------------------------------------------------------------------
// Request

const SKIP: &str = "skip_thought_signature_validator";

/// The parts of one message, with the detached signature waiting for its target part.
struct Parts {
    items: Vec<Vec<u8>>,
    pending: Vec<u8>,
    pending_kind: Vec<u8>,
}

impl Parts {
    /// A detached signature carrier part.
    fn carrier(&mut self, signature: &[u8]) {
        let mut carrier = br#"{"text":"","thoughtSignature":""}"#.to_vec();
        gj::set_str(&mut carrier, "thoughtSignature", signature);
        self.items.push(carrier);
    }

    fn clear_pending(&mut self) {
        self.pending.clear();
        self.pending_kind.clear();
    }

    fn set_pending(&mut self, signature: &[u8], kind: &[u8]) {
        if !self.pending.is_empty() {
            let pending = std::mem::take(&mut self.pending);
            self.carrier(&pending);
        }
        self.pending = signature.to_vec();
        self.pending_kind = kind.to_vec();
    }

    fn pending_targets(&self, kind: &str) -> bool {
        matches!(self.pending_kind.as_slice(), b"" | b"any") || self.pending_kind == kind.as_bytes()
    }
}

/// A thinking block of an assistant message (Go's `thinking` branch).
fn thinking_block(model: &str, blocks: &[Res<'_>], j: usize, parts: &mut Parts, translate_thoughts: &mut bool) {
    let block = &blocks[j];
    let text = thinking_text(block);
    let raw_signature = block.get("signature").bytes().into_owned();
    let mut signature = resolve_thinking_signature(model, &text, &raw_signature);
    if !signature.is_empty() && !parts.pending.is_empty() {
        if parts.pending != signature {
            let pending = parts.pending.clone();
            parts.carrier(&pending);
        }
        parts.clear_pending();
    }
    let mut from_pending = false;
    if signature.is_empty() && !text.is_empty() && !parts.pending.is_empty() {
        if parts.pending_targets(TEXT) {
            signature = parts.pending.clone();
            from_pending = true;
        } else {
            let pending = parts.pending.clone();
            parts.carrier(&pending);
        }
        parts.clear_pending();
    }
    let gemini = provider(model) == Provider::Gemini;
    if !has_resolved_signature(model, &signature) && !gemini {
        *translate_thoughts = false;
        return;
    }
    let (next_accepts, next_kind) = match blocks.get(j + 1).map(|b| b.get("type").bytes().into_owned()) {
        Some(t) if t == b"text" => (true, TEXT),
        Some(t) if t == b"tool_use" => (true, FUNCTION),
        _ => (false, ANY),
    };
    let carrier = decode_carrier(&raw_signature);
    let (direction, target) = (carrier.direction.as_slice(), carrier.target.as_slice());
    if !text.is_empty() {
        let mut part = b"{}".to_vec();
        gj::set_bool(&mut part, "thought", true);
        gj::set_str(&mut part, "text", &text);
        if from_pending {
            gj::set_str(&mut part, "thoughtSignature", &signature);
        } else if carrier.marked {
            let targets_next = target == ANY.as_bytes() || target == next_kind.as_bytes();
            if carrier.ok
                && direction == STANDALONE.as_bytes()
                && (target == TEXT.as_bytes() || target == ANY.as_bytes())
            {
                gj::set_str(&mut part, "thoughtSignature", &signature);
            } else if carrier.ok && direction == NEXT.as_bytes() && next_accepts && targets_next {
                parts.set_pending(&signature, target);
            }
        } else if gemini && next_accepts {
            parts.set_pending(&signature, next_kind.as_bytes());
        } else if !signature.is_empty() {
            gj::set_str(&mut part, "thoughtSignature", &signature);
        }
        parts.items.push(part);
        return;
    }
    if !gemini || (carrier.marked && !carrier.ok) {
        return;
    }
    if carrier.marked && direction == NEXT.as_bytes() {
        if carrier_matches_adjacent(blocks, j, direction, target) {
            parts.set_pending(&signature, target);
        }
        return;
    }
    if carrier.marked && direction == STANDALONE.as_bytes() {
        parts.carrier(&signature);
        return;
    }
    let backward = carrier.marked && direction == PREVIOUS.as_bytes();
    if backward && !carrier_matches_adjacent(blocks, j, direction, target) {
        return;
    }
    if !backward && next_accepts {
        parts.set_pending(&signature, next_kind.as_bytes());
        return;
    }
    let mut attached = false;
    let mut found_semantic = false;
    for index in (0..parts.items.len()).rev() {
        let part = gj::parse(&parts.items[index]);
        let kind = if part.get("functionCall").exists() {
            FUNCTION
        } else if !part.get("text").bytes().is_empty() {
            TEXT
        } else {
            continue;
        };
        found_semantic = true;
        if carrier.marked && target != ANY.as_bytes() && target != kind.as_bytes() {
            break;
        }
        let part_signature = trim_space(&part.get("thoughtSignature").bytes()).to_vec();
        let replace_fallback = backward && kind == FUNCTION && part_signature == SKIP.as_bytes();
        if part_signature.is_empty() || replace_fallback {
            gj::set_str(&mut parts.items[index], "thoughtSignature", &signature);
            attached = true;
        }
        break;
    }
    if !attached && (found_semantic || backward) {
        parts.carrier(&signature);
    } else if !attached {
        parts.set_pending(&signature, target);
    }
}

/// An `inlineData` object from a base64 image source.
fn inline_data(source: &Res<'_>) -> Vec<u8> {
    let mut inline = b"{}".to_vec();
    let mime = source.get("media_type").bytes();
    if !mime.is_empty() {
        gj::set_str(&mut inline, "mimeType", &mime);
    }
    let data = source.get("data").bytes();
    if !data.is_empty() {
        gj::set_str(&mut inline, "data", &data);
    }
    let mut part = b"{}".to_vec();
    gj::set_raw(&mut part, "inlineData", inline);
    part
}

fn is_base64_image(block: &Res<'_>) -> bool {
    block.get("type").bytes().as_ref() == b"image" && block.get("source.type").bytes().as_ref() == b"base64"
}

/// A tool_result block as a `functionResponse` part; images go to its `parts`.
fn tool_result_part(
    block: &Res<'_>,
    names: &HashMap<Vec<u8>, Vec<u8>>,
    tool_names: &HashMap<Vec<u8>, Vec<u8>>,
) -> Option<Vec<u8>> {
    let call_id = block.get("tool_use_id").bytes().into_owned();
    if call_id.is_empty() {
        return None;
    }
    let name = tool_names.get(&call_id).cloned().unwrap_or_else(|| {
        // The ID minus its last two dash-separated segments, else the ID itself.
        let segments: Vec<&[u8]> = call_id.split(|&c| c == b'-').collect();
        let derived = if segments.len() > 2 {
            segments[..segments.len() - 2].join(&b'-')
        } else {
            vec![]
        };
        if derived.is_empty() { call_id.clone() } else { derived }
    });
    let result = block.get("content");
    let mut response = b"{}".to_vec();
    gj::set_str(&mut response, "id", &call_id);
    gj::set_str(&mut response, "name", map_sanitized_function_name(names, &name));
    if result.kind == Kind::String {
        gj::set_str(&mut response, "response.result", &result.s);
    } else if result.is_array() {
        let (images, others): (Vec<Res<'_>>, Vec<Res<'_>>) = result.array().into_iter().partition(is_base64_image);
        let others: Vec<Vec<u8>> = others.iter().map(|r| r.raw.to_vec()).collect();
        match others.len() {
            0 => {
                gj::set_str(&mut response, "response.result", "");
            }
            1 => set_function_response_raw(&mut response, "response.result", &others[0]),
            _ => set_function_response_raw(&mut response, "response.result", &gj::join(&others)),
        }
        if !images.is_empty() {
            let images: Vec<Vec<u8>> = images.iter().map(|i| inline_data(&i.get("source"))).collect();
            gj::set_raw(&mut response, "parts", gj::join(&images));
        }
    } else if result.is_object() {
        if is_base64_image(&result) {
            gj::set_raw(&mut response, "parts", gj::join(&[inline_data(&result.get("source"))]));
            gj::set_str(&mut response, "response.result", "");
        } else {
            set_function_response_result(&mut response, "response.result", &result);
        }
    } else if !result.raw.is_empty() {
        set_function_response_result(&mut response, "response.result", &result);
    } else {
        gj::set_str(&mut response, "response.result", "");
    }
    let mut part = b"{}".to_vec();
    gj::set_raw(&mut part, "functionResponse", response);
    Some(part)
}

/// The function-call arguments Go keeps: objects as is, JSON-object strings parsed, null
/// as `{}`, anything else raw; absent input gives no call.
fn tool_use_args(input: &Res<'_>) -> Vec<u8> {
    if input.is_object() {
        return input.raw.to_vec();
    }
    if !input.exists() {
        return vec![];
    }
    match input.kind {
        Kind::String => {
            let parsed = gj::parse(&input.s);
            if parsed.is_object() {
                parsed.raw.to_vec()
            } else {
                input.raw.to_vec()
            }
        }
        Kind::Null => b"{}".to_vec(),
        _ => input.raw.to_vec(),
    }
}

/// Model parts reordered: thinking, then regular content, then function calls and the
/// signature carriers that follow a call.
fn reorder_model_parts(content: &mut Vec<u8>, parts: &[Vec<u8>]) {
    let (mut thinking, mut regular, mut trailing) = (vec![], vec![], vec![]);
    let mut needs_reorder = false;
    let mut previous = -1;
    let mut seen_call = false;
    for raw in parts {
        let part = gj::parse(raw);
        let text = part.get("text");
        let carrier =
            text.exists() && text.bytes().is_empty() && !trim_space(&part.get("thoughtSignature").bytes()).is_empty();
        let call = part.get("functionCall").exists();
        let category = if part.get("thought").bool() {
            thinking.push(raw.clone());
            0
        } else if call || (carrier && seen_call) {
            trailing.push(raw.clone());
            seen_call |= call;
            2
        } else {
            regular.push(raw.clone());
            1
        };
        needs_reorder |= category < previous;
        previous = category;
    }
    if needs_reorder {
        thinking.extend(regular);
        thinking.extend(trailing);
        gj::set_raw(content, "parts", gj::join(&thinking));
    }
}

/// ConvertClaudeRequestToAntigravity.
fn convert(model: &str, raw: &[u8]) -> Vec<u8> {
    if should_build_web_search(model, raw) {
        return web_search_request(model, raw);
    }
    let mut translate_thoughts = true;
    let names = sanitized_function_name_map(raw);

    let mut system_parts = vec![];
    let system = gj::get(raw, "system");
    if system.is_array() {
        for item in system.array() {
            let kind = item.get("type");
            if kind.kind != Kind::String || kind.s.as_ref() != b"text" {
                continue;
            }
            let prompt = item.get("text").bytes();
            if is_claude_code_attribution_text(&prompt) {
                continue;
            }
            let mut part = b"{}".to_vec();
            if !prompt.is_empty() {
                gj::set_str(&mut part, "text", &prompt);
            }
            system_parts.push(part);
        }
    } else if system.kind == Kind::String && !is_claude_code_attribution_text(&system.s) {
        let mut part = br#"{"text":""}"#.to_vec();
        gj::set_str(&mut part, "text", &system.s);
        system_parts.push(part);
    }

    let mut contents = vec![];
    let mut tool_names: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut pending_tool_use_ids: Vec<Vec<u8>> = vec![];
    let messages = gj::get(raw, "messages");
    if messages.is_array() {
        for message in messages.array() {
            let role = message.get("role");
            if role.kind != Kind::String {
                continue;
            }
            let original = role.s.to_vec();
            let system_like = original == b"system" || original == b"developer";
            let preceding = if system_like {
                vec![]
            } else {
                std::mem::take(&mut pending_tool_use_ids)
            };
            let role: &[u8] = match original.as_slice() {
                b"assistant" => b"model",
                b"system" | b"developer" => b"user",
                other => other,
            };
            let body = message.get("content");
            if system_like {
                if let Some(text) = claude_message_system_reminder_text(&body) {
                    let mut part = b"{}".to_vec();
                    gj::set_str(&mut part, "text", text);
                    contents.push(content(role, &[part]));
                }
                continue;
            }
            if body.kind == Kind::String {
                let mut part = b"{}".to_vec();
                if !body.s.is_empty() {
                    gj::set_str(&mut part, "text", &body.s);
                }
                contents.push(content(role, &[part]));
                continue;
            }
            if !body.is_array() {
                continue;
            }
            let mut blocks = body.array();
            if original == b"user" {
                blocks = align_claude_tool_results(blocks, &preceding);
            }
            let mut parts = Parts {
                items: vec![],
                pending: vec![],
                pending_kind: vec![],
            };
            for (j, block) in blocks.iter().enumerate() {
                let kind = block.get("type");
                if kind.kind != Kind::String {
                    continue;
                }
                match kind.s.as_ref() {
                    b"thinking" => {
                        if original == b"assistant" {
                            thinking_block(model, &blocks, j, &mut parts, &mut translate_thoughts);
                        }
                    }
                    b"text" => {
                        let prompt = block.get("text").bytes();
                        if prompt.is_empty() {
                            continue;
                        }
                        let mut part = b"{}".to_vec();
                        gj::set_str(&mut part, "text", &prompt);
                        if !parts.pending.is_empty() {
                            if parts.pending_targets(TEXT) {
                                gj::set_str(&mut part, "thoughtSignature", &parts.pending);
                            } else {
                                let pending = parts.pending.clone();
                                parts.carrier(&pending);
                            }
                            parts.clear_pending();
                        }
                        parts.items.push(part);
                    }
                    b"tool_use" => {
                        let original_name = block.get("name").bytes().into_owned();
                        let name = map_sanitized_function_name(&names, &original_name);
                        let id = block.get("id").bytes().into_owned();
                        if !id.is_empty() && !original_name.is_empty() {
                            tool_names.insert(id.clone(), original_name);
                        }
                        let args = tool_use_args(&block.get("input"));
                        if args.is_empty() {
                            continue;
                        }
                        let mut part = b"{}".to_vec();
                        let mut signature = tool_use_signature(model, block);
                        if !parts.pending.is_empty() {
                            if parts.pending_targets(FUNCTION) && (signature.is_empty() || signature == SKIP_SIGNATURE)
                            {
                                signature = parts.pending.clone();
                            } else {
                                let pending = parts.pending.clone();
                                parts.carrier(&pending);
                            }
                            parts.clear_pending();
                        }
                        if !signature.is_empty() {
                            gj::set_str(&mut part, "thoughtSignature", &signature);
                        }
                        if !id.is_empty() {
                            gj::set_str(&mut part, "functionCall.id", &id);
                        }
                        gj::set_str(&mut part, "functionCall.name", &name);
                        gj::set_raw(&mut part, "functionCall.args", &args);
                        parts.items.push(part);
                        if original == b"assistant" {
                            pending_tool_use_ids.push(id);
                        }
                    }
                    b"tool_result" => parts.items.extend(tool_result_part(block, &names, &tool_names)),
                    b"image" => {
                        let source = block.get("source");
                        if source.get("type").bytes().as_ref() == b"base64" {
                            parts.items.push(inline_data(&source));
                        }
                    }
                    _ => {}
                }
            }
            if !parts.pending.is_empty() {
                let pending = std::mem::take(&mut parts.pending);
                parts.carrier(&pending);
            }
            if parts.items.is_empty() {
                continue;
            }
            let mut turn = content(role, &parts.items);
            if role == b"model" && parts.items.len() > 1 {
                reorder_model_parts(&mut turn, &parts.items);
            }
            contents.push(turn);
        }
    }

    let tools_json = function_tools(raw, &names);
    let mut out = br#"{"model":"","request":{"contents":[]}}"#.to_vec();
    gj::set_str(&mut out, "model", model);

    let choice = gj::get(raw, "tool_choice");
    let (choice_type, choice_name) = if choice.is_object() {
        (
            choice.get("type").bytes().into_owned(),
            choice.get("name").bytes().into_owned(),
        )
    } else if choice.kind == Kind::String {
        (choice.s.to_vec(), vec![])
    } else {
        (vec![], vec![])
    };
    let choice_type = go_lower(trim_space(&choice_type));
    let choice_none = choice_type == b"none";

    let thinking = gj::get(raw, "thinking");
    let thinking_type = thinking.get("type").bytes().into_owned();
    let has_thinking = thinking.is_object() && matches!(thinking_type.as_slice(), b"enabled" | b"adaptive" | b"auto");
    let lower_model = go_lower(model.as_bytes());
    let has = |needle: &[u8]| lower_model.windows(needle.len()).any(|w| w == needle);
    if tools_json.is_some() && !choice_none && has_thinking && has(b"claude") && has(b"thinking") {
        let mut hint = br#"{"text":""}"#.to_vec();
        gj::set_str(&mut hint, "text", INTERLEAVED_HINT);
        system_parts.push(hint);
    }
    if !system_parts.is_empty() {
        gj::set_raw(&mut out, "request.systemInstruction", content("user", &system_parts));
    }
    if !contents.is_empty() {
        let contents = if has(b"claude") {
            crate::gemini_responses::merge_adjacent_user_contents(split_function_response_turns(contents))
        } else {
            merge_adjacent_contents(contents)
        };
        gj::set_items(&mut out, "request.contents", &contents);
    }
    if let Some(tools) = &tools_json
        && !choice_none
    {
        gj::set_raw(&mut out, "request.tools", tools);
    }
    if choice.exists() {
        const MODE: &str = "request.toolConfig.functionCallingConfig.mode";
        match choice_type.as_slice() {
            b"auto" => {
                gj::set_str(&mut out, MODE, "AUTO");
            }
            b"none" => {
                gj::set_str(&mut out, MODE, "NONE");
                gj::delete(&mut out, "request.tools");
            }
            b"any" => {
                gj::set_str(&mut out, MODE, "ANY");
            }
            b"tool" => {
                gj::set_str(&mut out, MODE, "ANY");
                if !choice_name.is_empty() {
                    gj::set_strs(
                        &mut out,
                        "request.toolConfig.functionCallingConfig.allowedFunctionNames",
                        &[map_sanitized_function_name(&names, &choice_name)],
                    );
                }
            }
            _ => {}
        }
    }
    if translate_thoughts && thinking.is_object() {
        match thinking_type.as_slice() {
            b"enabled" => {
                let budget = thinking.get("budget_tokens");
                if budget.kind == Kind::Number {
                    gj::set_int(
                        &mut out,
                        "request.generationConfig.thinkingConfig.thinkingBudget",
                        budget.int(),
                    );
                }
            }
            b"adaptive" | b"auto" => {
                let effort = gj::get(raw, "output_config.effort");
                let effort = if effort.kind == Kind::String {
                    go_lower(trim_space(&effort.s))
                } else {
                    vec![]
                };
                let level: &[u8] = if effort.is_empty() { b"high" } else { &effort };
                gj::set_str(&mut out, "request.generationConfig.thinkingConfig.thinkingLevel", level);
            }
            _ => {}
        }
    }
    for (from, to) in [
        ("temperature", "temperature"),
        ("top_p", "topP"),
        ("top_k", "topK"),
        ("max_tokens", "maxOutputTokens"),
    ] {
        let v = gj::get(raw, from);
        if v.kind == Kind::Number {
            gj::set_f64(&mut out, &format!("request.generationConfig.{to}"), v.num);
        }
    }
    let out = attach_default_safety_settings(out, "request.safetySettings");
    if provider(model) == Provider::Gemini {
        return sig::sanitize_gemini_request_thought_signatures(&out, "request.contents");
    }
    out
}

/// The `tools` value for the request: one tool holding the deduplicated declarations made
/// from tools with an object `input_schema` (typed web search tools are skipped).
fn function_tools(raw: &[u8], names: &HashMap<Vec<u8>, Vec<u8>>) -> Option<Vec<u8>> {
    const ALLOWED: [&[u8]; 7] = [
        b"name",
        b"description",
        b"behavior",
        b"parameters",
        b"parametersJsonSchema",
        b"response",
        b"responseJsonSchema",
    ];
    let tools = gj::get(raw, "tools");
    if !tools.is_array() {
        return None;
    }
    let mut declarations = vec![];
    for tool in tools.array() {
        if is_typed_web_search(&tool.get("type").bytes()) {
            continue;
        }
        let schema = tool.get("input_schema");
        if !schema.is_object() {
            continue;
        }
        let cleaned = cpa_common::gemini_schema::for_antigravity(&schema.raw);
        let mut decl = tool.raw.to_vec();
        gj::delete(&mut decl, "input_schema");
        gj::set_raw(&mut decl, "parametersJsonSchema", cleaned);
        let name = gj::get(&decl, "name").into_owned();
        let mapped = map_sanitized_function_name(names, &name.bytes());
        if name.kind != Kind::String || mapped != *name.bytes() {
            gj::set_str(&mut decl, "name", &mapped);
        }
        let keys: Vec<Vec<u8>> = gj::parse(&decl).map().into_iter().map(|(k, _)| k).collect();
        for key in keys {
            if !ALLOWED.contains(&key.as_slice()) {
                gj::delete(&mut decl, String::from_utf8_lossy(&key).as_ref());
            }
        }
        declarations.push(decl);
    }
    if declarations.is_empty() {
        return None;
    }
    let deduplicated = crate::antigravity_chat::deduplicate_declarations(&gj::join(&declarations));
    if gj::parse(&deduplicated).array().is_empty() {
        return None;
    }
    let mut node = br#"{"functionDeclarations":[]}"#.to_vec();
    gj::set_raw(&mut node, "functionDeclarations", &deduplicated);
    Some(gj::join(&[node]))
}

/// common.SplitGeminiFunctionResponseTurns: function responses get their own user turn,
/// and after a model turn with calls, response turns come before other user turns.
pub(crate) fn split_function_response_turns(contents: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    use crate::gemini::has_function_response;
    if contents.is_empty() {
        return contents;
    }
    let is_user = |c: &[u8]| gj::get(c, "role").bytes().as_ref() == b"user";
    let mut split = vec![];
    for turn in contents {
        if !is_user(&turn) || !has_function_response(&turn) {
            split.push(turn);
            continue;
        }
        let (responses, others): (Vec<Res<'_>>, Vec<Res<'_>>) = gj::get(&turn, "parts")
            .array()
            .into_iter()
            .partition(|p| p.get("functionResponse").exists() || p.get("function_response").exists());
        let rebuild = |parts: &[Res<'_>]| -> Option<Result<Vec<u8>, String>> {
            (!parts.is_empty()).then(|| {
                let raws: Vec<Vec<u8>> = parts.iter().map(|p| p.raw.to_vec()).collect();
                gj::try_set_raw(&turn, "parts", gj::join(&raws))
            })
        };
        match (rebuild(&responses), rebuild(&others)) {
            (Some(Err(_)), _) | (_, Some(Err(_))) => split.push(turn.clone()),
            (response_turn, other_turn) => {
                split.extend(response_turn.and_then(Result::ok));
                split.extend(other_turn.and_then(Result::ok));
            }
        }
    }
    let has_call = |c: &[u8]| {
        let mut found = false;
        gj::get(c, "parts").each(|_, part| {
            found = part.get("functionCall").exists() || part.get("function_call").exists();
            !found
        });
        found
    };
    let mut out: Vec<Vec<u8>> = vec![];
    let mut i = 0;
    while i < split.len() {
        if !is_user(&split[i]) {
            out.push(split[i].clone());
            i += 1;
            continue;
        }
        let after_call = out
            .last()
            .is_some_and(|last| gj::get(last, "role").bytes().as_ref() == b"model" && has_call(last));
        let mut j = i;
        let mut has_response = false;
        while j < split.len() && is_user(&split[j]) {
            has_response |= has_function_response(&split[j]);
            j += 1;
        }
        let run = &split[i..j];
        if after_call && has_response && run.len() > 1 {
            let mut response_parts = vec![];
            let mut others = vec![];
            for turn in run {
                if has_function_response(turn) {
                    response_parts.extend(gj::get(turn, "parts").array().iter().map(|p| p.raw.to_vec()));
                } else {
                    others.push(turn.clone());
                }
            }
            if !response_parts.is_empty() {
                let mut turn = br#"{"role":"user","parts":[]}"#.to_vec();
                gj::set_raw(&mut turn, "parts", gj::join(&response_parts));
                out.push(turn);
            }
            out.extend(others);
        } else {
            out.extend(run.iter().cloned());
        }
        i = j;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// buildAntigravityWebSearchRequest as Go 6fecc6e writes it through TranslateRequest
    /// with an Antigravity `gemini-3-flash` registered with web search (the goldens run
    /// without a registry, so they never reach this request).
    #[test]
    fn web_search_request_matches_go() {
        const INSTRUCTION: &str = r#"{"text":"You are a search engine bot. You will be given a query from a user. Your task is to search the web for relevant information that will help the user. You MUST perform a web search. Do not respond or interact with the user, please respond as if they typed the query into a search bar."}"#;
        let cases = [
            (
                r#"{"tools":[{"type":"web_search_20250305","name":"web_search","max_uses":3,"allowed_domains":[" a.com ","",5,"b<c>.com"]}],"tool_choice":{"type":"tool","name":"web_search"},"messages":[{"role":"user","content":"first"},{"role":"assistant","content":"x"},{"role":"user","content":[{"type":"text","text":" find <this> "},{"type":"image"},{"type":"text","text":"too"}]}]}"#,
                format!(
                    r#"{{"model":"gemini-3-flash(high)","requestType":"web_search","request":{{"contents":[{{"role":"user","parts":[{{"text":"find \u003cthis\u003e\ntoo"}}]}}],"systemInstruction":{{"role":"user","parts":[{INSTRUCTION}]}},"tools":[{{"googleSearch":{{"enhancedContent":{{"imageSearch":{{"maxResultCount":3}}}},"includedDomains":["a.com","b\u003cc\u003e.com"]}}}}],"generationConfig":{{"candidateCount":1}}}}}}"#
                ),
            ),
            (
                r#"{"tools":[{"type":"web_search_20260209","name":"web_search","max_uses":0}],"messages":[{"role":"user","content":"  "},{"content":"role-less"}]}"#,
                format!(
                    r#"{{"model":"gemini-3-flash(high)","requestType":"web_search","request":{{"contents":[{{"role":"user","parts":[{{"text":"role-less"}}]}}],"systemInstruction":{{"role":"user","parts":[{INSTRUCTION}]}},"tools":[{{"googleSearch":{{"enhancedContent":{{"imageSearch":{{"maxResultCount":5}}}}}}}}],"generationConfig":{{"candidateCount":1}}}}}}"#
                ),
            ),
        ];
        for (input, want) in &cases {
            let got = web_search_request("gemini-3-flash(high)", input.as_bytes());
            assert_eq!(String::from_utf8_lossy(&got), *want);
        }
        // Without a registered Antigravity model the normal request is built.
        assert!(!should_build_web_search("gemini-3-flash", cases[0].0.as_bytes()));
    }
}
