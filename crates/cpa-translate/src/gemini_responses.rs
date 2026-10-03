//! OpenAI Responses -> Gemini request (internal/translator/gemini/openai/responses:
//! gemini_openai-responses_request.go, signature_carrier.go, trailing_signature.go).
//!
//! Reasoning signatures travel in `encrypted_content`. Signatures that cannot sit on the
//! reasoning item itself are "carriers": `cpa-gemini-responses-carrier-v1:` +
//! direction + target + base64, which the response side emits and this side restores
//! next to the text or function call they belong to.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};

use base64::Engine as _;
use cpa_common::json::{self as gj, Kind, Res};
use cpa_common::signature::{self as sig, BlockKind, Provider};
use sha2::{Digest, Sha256};

use crate::claude_responses::{extract_call_id, normalize_tool_call_outputs, qualify_namespace_name};
use crate::common::{file_ext, go_lower, normalize_openai_file_data, sanitize_function_name, trim_space};
use crate::gemini::{set_function_response_raw, set_function_response_result};
use crate::gemini_chat_request::{content_node as content, text_part};
use crate::gemini_web_search as ws;
use crate::responses_tools::{gemini_function_declarations, map_tool_name, tool_choice_to_gemini};

pub(crate) const BYPASS_SIGNATURE: &[u8] = b"skip_thought_signature_validator";

const CARRIER_PREFIX: &[u8] = b"cpa-gemini-responses-carrier-v1:";
pub(crate) const NEXT: &str = "next";
pub(crate) const PREVIOUS: &str = "previous";
pub(crate) const STANDALONE: &str = "standalone";
pub(crate) const TEXT: &str = "text";
pub(crate) const FUNCTION: &str = "function";
pub(crate) const ANY: &str = "any";
const DIRECTION_FIELD: &str = "_cpa_reasoning_direction";
const TARGET_FIELD: &str = "_cpa_reasoning_target";
const SIGNATURE_FIELD: &str = "_cpa_reasoning_signature";
const SUMMARY_FIELD: &str = "_cpa_reasoning_summary";

// ---------------------------------------------------------------------------------------
// Go base64 (encoding/base64 skips CR and LF and accepts non-zero trailing bits)

pub(crate) fn go_base64(padded: bool) -> base64::engine::GeneralPurpose {
    use base64::engine::{DecodePaddingMode, GeneralPurposeConfig};
    base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_encode_padding(padded)
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(if padded {
                DecodePaddingMode::RequireCanonical
            } else {
                DecodePaddingMode::RequireNone
            }),
    )
}

pub(crate) fn go_base64_decode(input: &[u8], padded: bool) -> Option<Vec<u8>> {
    let input: Vec<u8> = input.iter().copied().filter(|&c| c != b'\r' && c != b'\n').collect();
    go_base64(padded).decode(input).ok()
}

// ---------------------------------------------------------------------------------------
// Carriers (signature_carrier.go)

/// encodeGeminiResponsesCarrier.
pub(crate) fn encode_carrier(raw_signature: &[u8], direction: &str, target: &str) -> Vec<u8> {
    let raw = trim_space(raw_signature);
    if raw.is_empty() {
        return vec![];
    }
    let mut out = CARRIER_PREFIX.to_vec();
    out.extend_from_slice(direction.as_bytes());
    out.push(b':');
    out.extend_from_slice(target.as_bytes());
    out.push(b':');
    out.extend_from_slice(go_base64(false).encode(raw).as_bytes());
    out
}

struct Decoded {
    signature: Vec<u8>,
    direction: Vec<u8>,
    target: Vec<u8>,
    marked: bool,
    ok: bool,
}

/// decodeGeminiResponsesCarrier.
fn decode_carrier(raw_signature: &[u8]) -> Decoded {
    let raw = trim_space(raw_signature);
    let fail = Decoded {
        signature: vec![],
        direction: vec![],
        target: vec![],
        marked: true,
        ok: false,
    };
    let Some(rest) = raw.strip_prefix(CARRIER_PREFIX) else {
        return Decoded {
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
    match go_base64_decode(payload, false) {
        Some(decoded) if !decoded.is_empty() && !decoded.starts_with(CARRIER_PREFIX) => Decoded {
            signature: decoded,
            direction: direction.to_vec(),
            target: target.to_vec(),
            marked: true,
            ok: true,
        },
        _ => fail,
    }
}

/// compatibleGeminiResponsesCarrierSignature: a Gemini-compatible, non-bypass signature.
pub(crate) fn compatible_carrier_signature(raw: &[u8], target: &[u8]) -> Option<Vec<u8>> {
    let kind = if target == FUNCTION.as_bytes() {
        BlockKind::GeminiFunctionCall
    } else {
        BlockKind::GeminiModelPart
    };
    let normalized = sig::compatible_signature_for_provider_block(Provider::Gemini, raw, kind)?;
    if sig::is_gemini_bypass(sig::payload_without_provider_prefix(normalized.as_bytes())) {
        return None;
    }
    Some(normalized.into_bytes())
}

fn field<'a>(item: &Res<'a>, path: &str) -> Cow<'a, [u8]> {
    item.get(path).bytes()
}

fn trimmed(item: &Res<'_>, path: &str) -> Vec<u8> {
    trim_space(&field(item, path)).to_vec()
}

fn item_type(item: &Res<'_>) -> Vec<u8> {
    field(item, "type").into_owned()
}

fn direction(item: &Res<'_>) -> Vec<u8> {
    field(item, DIRECTION_FIELD).into_owned()
}

fn target(item: &Res<'_>) -> Vec<u8> {
    field(item, TARGET_FIELD).into_owned()
}

fn is(value: &[u8], token: &str) -> bool {
    value == token.as_bytes()
}

fn is_tool_call(item: &Res<'_>) -> bool {
    matches!(item_type(item).as_slice(), b"function_call" | b"custom_tool_call")
}

fn is_tool_output(item: &Res<'_>) -> bool {
    matches!(
        item_type(item).as_slice(),
        b"function_call_output" | b"custom_tool_call_output"
    )
}

/// isOpenAIResponsesDetachedCarrier: a signature-only reasoning item.
fn is_detached_carrier(item: &Res<'_>) -> bool {
    item_type(item) == b"reasoning"
        && !trimmed(item, "encrypted_content").is_empty()
        && trimmed(item, "summary.0.text").is_empty()
}

fn semantic_target(item: &Res<'_>) -> &'static str {
    match item_type(item).as_slice() {
        b"function_call" | b"custom_tool_call" => return FUNCTION,
        b"reasoning" if !trimmed(item, "summary.0.text").is_empty() => return TEXT,
        _ => {}
    }
    if assistant_visible_text(item).is_some() {
        TEXT
    } else {
        ""
    }
}

/// geminiResponsesCarrierMatchesAdjacent: the nearest non-carrier item in the carrier's
/// direction has the carrier's target kind.
fn matches_adjacent(items: &[Res<'_>], index: usize, direction: &[u8], target: &[u8]) -> bool {
    let step: isize = if is(direction, PREVIOUS) { -1 } else { 1 };
    let mut adjacent = index as isize + step;
    while adjacent >= 0 && (adjacent as usize) < items.len() {
        let item = &items[adjacent as usize];
        let kind = semantic_target(item);
        if !kind.is_empty() {
            return is(target, ANY) || is(target, kind);
        }
        if !is_detached_carrier(item) {
            return false;
        }
        adjacent += step;
    }
    false
}

fn has_internal_carrier_fields(item: &Res<'_>) -> bool {
    [DIRECTION_FIELD, TARGET_FIELD, SIGNATURE_FIELD, SUMMARY_FIELD]
        .iter()
        .any(|f| item.get(*f).exists())
}

/// stripGeminiResponsesCarrierMetadata: `json.Unmarshal` into `map[string]json.RawMessage`,
/// drop the internal fields, `json.Marshal` (sorted keys, compacted HTML-escaped values;
/// the last duplicate key wins).
fn strip_carrier_metadata(raw: &[u8]) -> Option<Vec<u8>> {
    if !gj::std_valid(raw) {
        return None;
    }
    let root = gj::parse(raw);
    if !root.is_object() {
        return None;
    }
    let mut fields: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    root.each(|key, value| {
        let key = gj::go_unquote(&key.raw).map(String::into_bytes).unwrap_or_default();
        fields.insert(key, value.raw.to_vec());
        true
    });
    for f in [DIRECTION_FIELD, TARGET_FIELD, SIGNATURE_FIELD, SUMMARY_FIELD] {
        fields.remove(f.as_bytes());
    }
    let mut out = vec![b'{'];
    for (i, (key, value)) in fields.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        gj::marshal_str(&mut out, key, true);
        out.push(b':');
        out.extend_from_slice(&gj::compact(value, true));
    }
    out.push(b'}');
    Some(out)
}

fn owned(raw: &[u8]) -> Res<'static> {
    gj::parse(raw).into_owned()
}

/// normalizeGeminiResponsesCarriers: decodes carriers, keeps those whose signature is
/// Gemini-compatible and whose neighbour matches, and reports whether any valid carrier
/// (or compatible raw signature) is present.
fn normalize_carriers<'a>(items: &[Res<'a>]) -> (Vec<Res<'a>>, bool) {
    let mut normalized = Vec::with_capacity(items.len());
    let mut has_valid = false;
    for (index, original) in items.iter().enumerate() {
        let mut item = original.clone();
        let mut item_json: Option<Vec<u8>> = None;
        if has_internal_carrier_fields(original)
            && let Some(stripped) = strip_carrier_metadata(&original.raw)
        {
            item = owned(&stripped);
            item_json = Some(stripped);
        }
        if item_type(&item) != b"reasoning" {
            normalized.push(item);
            continue;
        }
        let mut item_json = item_json.unwrap_or_else(|| item.raw.to_vec());
        let raw_signature = trimmed(&item, "encrypted_content");
        let decoded = decode_carrier(&raw_signature);
        if !decoded.marked {
            if !raw_signature.is_empty() {
                has_valid |= compatible_carrier_signature(&raw_signature, ANY.as_bytes()).is_some();
            }
            normalized.push(item);
            continue;
        }
        let (direction, target) = (decoded.direction, decoded.target);
        let mut signature = None;
        if decoded.ok {
            signature = compatible_carrier_signature(&decoded.signature, &target);
        }
        let mut ok = signature.is_some();
        if ok && !is(&direction, STANDALONE) {
            ok = matches_adjacent(items, index, &direction, &target);
        }
        let has_summary = !trimmed(&item, "summary.0.text").is_empty();
        let valid_summary_carrier = has_summary
            && ((is(&direction, STANDALONE) && (is(&target, TEXT) || is(&target, ANY))) || is(&direction, NEXT));
        if !ok || (!is_detached_carrier(&item) && !valid_summary_carrier) {
            if !has_summary {
                continue;
            }
            gj::delete(&mut item_json, "encrypted_content");
            normalized.push(owned(&item_json));
            continue;
        }
        has_valid = true;
        gj::set_str(&mut item_json, "encrypted_content", signature.unwrap_or_default());
        gj::set_str(&mut item_json, DIRECTION_FIELD, &direction);
        gj::set_str(&mut item_json, TARGET_FIELD, &target);
        normalized.push(owned(&item_json));
    }
    (normalized, has_valid)
}

// ---------------------------------------------------------------------------------------
// Trailing text signatures (trailing_signature.go)

fn sha256_hex(text: &[u8]) -> String {
    crate::common::hex(&Sha256::digest(text))
}

fn replay_model(model: &str) -> String {
    cpa_common::thinking::parse_suffix(model).model_name
}

fn replay_session(message_id: &[u8]) -> String {
    format!("gemini-responses-text:{}", String::from_utf8_lossy(message_id))
}

/// cacheGeminiResponsesTextSignatures: remembers signatures that trailed a visible message
/// so the next request can restore them; false when any is not a usable text signature.
pub(crate) fn cache_text_signatures(model: &str, message_id: &[u8], text: &[u8], signatures: &[Vec<u8>]) -> bool {
    if message_id.is_empty() || text.is_empty() {
        return false;
    }
    let hash = sha256_hex(text);
    let mut items = vec![];
    for signature in signatures {
        if compatible_carrier_signature(signature, TEXT.as_bytes()).is_none() {
            return false;
        }
        let mut item = br#"{"type":"thought_signature","targetKind":"text"}"#.to_vec();
        gj::set_str(&mut item, "thoughtSignature", signature);
        gj::set_str(&mut item, "targetHash", &hash);
        items.push(item);
    }
    crate::replay_cache::put(&replay_model(model), &replay_session(message_id), &items)
}

/// restoreGeminiResponsesTextSignatures: re-inserts cached signatures as `previous`
/// carriers after the assistant message whose text they were cached for.
fn restore_text_signatures<'a>(model: &str, items: &[Res<'a>]) -> Vec<Res<'a>> {
    let mut restored = Vec::with_capacity(items.len());
    let mut skip: HashSet<usize> = HashSet::new();
    for (index, item) in items.iter().enumerate() {
        if skip.contains(&index) {
            continue;
        }
        restored.push(item.clone());
        let message_id = trimmed(item, "id");
        let Some(text) = assistant_visible_text(item) else {
            continue;
        };
        if message_id.is_empty() {
            continue;
        }
        let Some(cached) = crate::replay_cache::get(&replay_model(model), &replay_session(&message_id)) else {
            continue;
        };
        let mut replayed: HashSet<Vec<u8>> = HashSet::new();
        let hash = sha256_hex(&text);
        for raw in cached {
            let entry = gj::parse(&raw);
            let signature = entry.get("thoughtSignature").bytes().into_owned();
            if entry.get("targetHash").bytes().as_ref() != hash.as_bytes() {
                continue;
            }
            let mut carrier = br#"{"type":"reasoning","summary":[]}"#.to_vec();
            gj::set_str(
                &mut carrier,
                "encrypted_content",
                encode_carrier(&signature, PREVIOUS, TEXT),
            );
            restored.push(owned(&carrier));
            replayed.insert(signature);
        }
        let mut adjacent = index + 1;
        while adjacent < items.len() && is_detached_carrier(&items[adjacent]) {
            let d = decode_carrier(&field(&items[adjacent], "encrypted_content"));
            if d.ok && is(&d.direction, PREVIOUS) && is(&d.target, TEXT) && replayed.contains(&d.signature) {
                skip.insert(adjacent);
            }
            adjacent += 1;
        }
    }
    restored
}

// ---------------------------------------------------------------------------------------
// Request conversion

/// openAIResponsesAssistantVisibleText: the model-visible text of a message (string
/// content of an assistant/model message, or its `output_text` parts joined).
pub(crate) fn assistant_visible_text(item: &Res<'_>) -> Option<Vec<u8>> {
    let mut kind = item_type(item);
    let role = field(item, "role");
    if kind.is_empty() && !role.is_empty() {
        kind = b"message".to_vec();
    }
    if kind != b"message" {
        return None;
    }
    let content = item.get("content");
    if !content.exists() {
        return None;
    }
    if content.kind == Kind::String {
        return matches!(go_lower(trim_space(&role)).as_slice(), b"assistant" | b"model").then(|| content.s.to_vec());
    }
    if !content.is_array() {
        return None;
    }
    let mut texts: Vec<Vec<u8>> = vec![];
    let mut has_output_text = false;
    content.each(|_, part| {
        let kind = part.get("type").bytes();
        if kind.as_ref() == b"output_text" {
            has_output_text = true;
            texts.push(part.get("text").bytes().into_owned());
        }
        true
    });
    has_output_text.then(|| texts.join(&b'\n'))
}

fn is_content_part_type(kind: &[u8]) -> bool {
    matches!(
        go_lower(trim_space(kind)).as_slice(),
        b"input_text"
            | b"output_text"
            | b"text"
            | b"input_image"
            | b"image_url"
            | b"image"
            | b"input_audio"
            | b"audio"
            | b"input_video"
            | b"video_url"
            | b"video"
            | b"input_file"
            | b"file"
    )
}

fn with_field(item: &Res<'_>, path: &str, value: &[u8]) -> Res<'static> {
    let mut raw = item.raw.to_vec();
    gj::set_str(&mut raw, path, value);
    owned(&raw)
}

/// pairOpenAIResponsesReasoningWithFunctionCalls: moves reasoning signatures onto the
/// function calls they belong to (`_cpa_reasoning_signature`, `_cpa_reasoning_summary`).
fn pair_reasoning_with_function_calls<'a>(items: &[Res<'a>]) -> Vec<Res<'a>> {
    let mut post_call_signature: HashMap<usize, Vec<u8>> = HashMap::new();
    let mut post_call_carrier: HashSet<usize> = HashSet::new();
    let mut consumed_post_call_carrier: HashSet<usize> = HashSet::new();
    let mut group_start = 0;
    while group_start < items.len() {
        if !is_tool_call(&items[group_start]) && !is_detached_carrier(&items[group_start]) {
            group_start += 1;
            continue;
        }
        let mut group_end = group_start;
        let mut has_call = false;
        while group_end < items.len() && (is_tool_call(&items[group_end]) || is_detached_carrier(&items[group_end])) {
            has_call |= is_tool_call(&items[group_end]);
            group_end += 1;
        }
        if !has_call || group_end >= items.len() || !is_tool_output(&items[group_end]) {
            group_start = group_end;
            continue;
        }
        let mut output_end = group_end;
        while output_end < items.len() && is_tool_output(&items[output_end]) {
            output_end += 1;
        }
        if is_tool_call(&items[group_start]) {
            for call_index in group_start..group_end {
                let item = &items[call_index];
                if !is_tool_call(item)
                    || !trimmed(item, SIGNATURE_FIELD).is_empty()
                    || call_index + 1 >= group_end
                    || !is_detached_carrier(&items[call_index + 1])
                {
                    continue;
                }
                let (dir, tgt) = (direction(&items[call_index + 1]), target(&items[call_index + 1]));
                if !dir.is_empty() && (!is(&dir, PREVIOUS) || (!is(&tgt, FUNCTION) && !is(&tgt, ANY))) {
                    continue;
                }
                let mut carrier_end = call_index + 1;
                while carrier_end < group_end && is_detached_carrier(&items[carrier_end]) {
                    post_call_carrier.insert(carrier_end);
                    carrier_end += 1;
                }
                let call_id = extract_call_id(item);
                if call_id.is_empty() {
                    continue;
                }
                if (group_end..output_end).any(|o| extract_call_id(&items[o]) == call_id) {
                    post_call_signature.insert(call_index, trimmed(&items[call_index + 1], "encrypted_content"));
                    consumed_post_call_carrier.insert(call_index + 1);
                }
            }
        }
        group_start = output_end;
    }

    let mut paired = Vec::with_capacity(items.len());
    let mut index = 0;
    while index < items.len() {
        let item = &items[index];
        if let Some(signature) = post_call_signature.get(&index).filter(|s| !s.is_empty()) {
            paired.push(with_field(item, SIGNATURE_FIELD, signature));
            index += 1;
            continue;
        }
        if consumed_post_call_carrier.contains(&index) {
            index += 1;
            continue;
        }
        let (dir, tgt) = (direction(item), target(item));
        let can_bind_following = dir.is_empty() || (is(&dir, NEXT) && (is(&tgt, FUNCTION) || is(&tgt, ANY)));
        if item_type(item) == b"reasoning"
            && !post_call_carrier.contains(&index)
            && can_bind_following
            && !contains(&field(item, "id"), b"_detached_after_")
            && index + 1 < items.len()
            && is_tool_call(&items[index + 1])
        {
            let raw_signature = trimmed(item, "encrypted_content");
            if !raw_signature.is_empty() {
                let mut call = items[index + 1].raw.to_vec();
                gj::set_str(&mut call, SIGNATURE_FIELD, &raw_signature);
                let summary = field(item, "summary.0.text");
                if !summary.is_empty() {
                    gj::set_str(&mut call, SUMMARY_FIELD, &summary);
                }
                paired.push(owned(&call));
                index += 2;
                continue;
            }
        }
        paired.push(item.clone());
        index += 1;
    }
    paired
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn message_kind(item: &Res<'_>) -> Vec<u8> {
    let kind = item_type(item);
    if kind.is_empty() && !item.get("role").bytes().is_empty() {
        b"message".to_vec()
    } else {
        kind
    }
}

/// reorderOpenAIResponsesDetachedReasoning: in the native Gemini layout a carrier goes
/// before the text or call it signs.
fn reorder_detached_reasoning<'a>(items: &[Res<'a>]) -> Vec<Res<'a>> {
    let mut reordered: Vec<Res<'a>> = Vec::with_capacity(items.len());
    for (item_index, item) in items.iter().enumerate() {
        let is_carrier = is_detached_carrier(item);
        let marked_detached = contains(&field(item, "id"), b"_detached_after_");
        if is_carrier && let Some(previous) = reordered.last().cloned() {
            let previous_type = message_kind(&previous);
            let mut is_assistant_message = previous_type == b"message" && assistant_visible_text(&previous).is_some();
            let (dir, tgt) = (direction(item), target(item));
            let previous_is_call = matches!(previous_type.as_slice(), b"function_call" | b"custom_tool_call");
            if !dir.is_empty() {
                let (mut paired_text, mut paired_function) = (false, false);
                if reordered.len() > 1 {
                    let prior = &reordered[reordered.len() - 2];
                    let (pd, pt) = (direction(prior), target(prior));
                    let binds = is_detached_carrier(prior) && (is(&pd, NEXT) || is(&pd, PREVIOUS));
                    paired_text = binds && (is(&pt, TEXT) || is(&pt, ANY));
                    paired_function = binds && (is(&pt, FUNCTION) || is(&pt, ANY));
                }
                let bind_message =
                    is(&dir, PREVIOUS) && (is(&tgt, TEXT) || is(&tgt, ANY)) && is_assistant_message && !paired_text;
                let bind_function = is(&dir, PREVIOUS)
                    && (is(&tgt, FUNCTION) || is(&tgt, ANY))
                    && previous_is_call
                    && trimmed(&previous, SIGNATURE_FIELD).is_empty()
                    && !paired_function;
                if bind_message || bind_function {
                    let moved = with_field(item, DIRECTION_FIELD, NEXT.as_bytes());
                    *reordered.last_mut().unwrap() = moved;
                    reordered.push(previous);
                    continue;
                }
                reordered.push(item.clone());
                continue;
            }
            if is_assistant_message && !marked_detached && item_index + 1 < items.len() {
                is_assistant_message = assistant_visible_text(&items[item_index + 1]).is_none();
            }
            let already_paired = reordered.len() > 1 && {
                let prior = &reordered[reordered.len() - 2];
                is_detached_carrier(prior) && contains(&field(prior, "id"), b"_detached_after_")
            };
            if !already_paired
                && (is_assistant_message
                    || (marked_detached && previous_is_call && trimmed(&previous, SIGNATURE_FIELD).is_empty()))
            {
                *reordered.last_mut().unwrap() = item.clone();
                reordered.push(previous);
                continue;
            }
        }
        reordered.push(item.clone());
    }
    reordered
}

/// buildOpenAIResponsesFunctionCallPart.
fn function_call_part(item: &Res<'_>, signature: &[u8], forward: &HashMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
    let mut name = field(item, "name").into_owned();
    let namespace = field(item, "namespace");
    if !namespace.is_empty() {
        name = qualify_namespace_name(&namespace, &name);
    }
    let name = map_tool_name(forward, &name);
    let mut part = br#"{"functionCall":{"name":"","args":{}}}"#.to_vec();
    gj::set_str(&mut part, "functionCall.name", &name);
    gj::set_str(&mut part, "thoughtSignature", signature);
    gj::set_str(&mut part, "functionCall.id", extract_call_id(item));
    if item_type(item) == b"custom_tool_call" {
        let input = item.get("input");
        if !input.exists() {
            gj::set_str(&mut part, "functionCall.args.input", "");
        } else if input.kind == Kind::String {
            gj::set_str(&mut part, "functionCall.args.input", &input.s);
        } else {
            gj::set_raw(&mut part, "functionCall.args.input", &input.raw);
        }
    } else {
        let arguments = field(item, "arguments");
        if !arguments.is_empty() {
            let args = gj::parse(&arguments);
            if args.is_object() || args.is_array() {
                gj::set_raw(&mut part, "functionCall.args", &args.raw);
            } else {
                gj::set_str(&mut part, "functionCall.args.arguments", &arguments);
            }
        }
    }
    part
}

fn inline_data_part(mime: &[u8], data: &[u8]) -> Vec<u8> {
    let mut part = br#"{"inline_data":{"mime_type":"","data":""}}"#.to_vec();
    gj::set_str(&mut part, "inline_data.mime_type", mime);
    gj::set_str(&mut part, "inline_data.data", data);
    part
}

fn file_data_part(mime: &[u8], uri: &[u8]) -> Vec<u8> {
    let mut part = br#"{"file_data":{"mime_type":"","file_uri":""}}"#.to_vec();
    gj::set_str(&mut part, "file_data.mime_type", mime);
    gj::set_str(&mut part, "file_data.file_uri", uri);
    part
}

/// firstNonEmpty over the values at `paths`: the first non-blank one, trimmed.
fn first_non_empty(block: &Res<'_>, paths: &[&str]) -> Vec<u8> {
    paths
        .iter()
        .map(|p| trimmed(block, p))
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

fn has_prefix_fold(s: &[u8], prefixes: &[&[u8]]) -> bool {
    let s = trim_space(s);
    prefixes
        .iter()
        .any(|p| s.len() >= p.len() && s[..p.len()].eq_ignore_ascii_case(p))
}

fn is_data_url(raw: &[u8]) -> bool {
    has_prefix_fold(raw, &[b"data:"])
}

fn is_remote_url(raw: &[u8]) -> bool {
    has_prefix_fold(raw, &[b"http://", b"https://", b"gs://"])
}

fn is_generic_mime(mime: &[u8]) -> bool {
    matches!(
        go_lower(trim_space(mime)).as_slice(),
        b"" | b"application/octet-stream" | b"binary/octet-stream"
    )
}

/// firstNonGenericFormat.
fn first_non_generic(block: &Res<'_>, paths: &[&str]) -> Vec<u8> {
    paths
        .iter()
        .map(|p| trimmed(block, p))
        .find(|v| !v.is_empty() && !is_generic_mime(v))
        .unwrap_or_default()
}

fn ext_lower(filename: &[u8]) -> Vec<u8> {
    let ext = file_ext(filename);
    go_lower(ext.strip_prefix(b".").unwrap_or(ext))
}

/// parseOpenAIResponsesDataURL: MIME type and payload of a valid base64 data URL.
fn parse_data_url(raw: &[u8]) -> (Vec<u8>, Vec<u8>) {
    use cpa_common::gostr::GoStr;
    let raw = trim_space(raw);
    if raw.len() < 5 || !raw[..5].eq_ignore_ascii_case(b"data:") {
        return (vec![], vec![]);
    }
    let rest = &raw[5..];
    let Some(comma) = rest.iter().position(|&c| c == b',') else {
        return (vec![], vec![]);
    };
    let (metadata, payload) = (&rest[..comma], trim_space(&rest[comma + 1..]));
    if payload.is_empty() {
        return (vec![], vec![]);
    }
    let mut fields = metadata.split(|&c| c == b';');
    let mime = trim_space(fields.next().unwrap_or_default()).to_vec();
    if !fields.any(|f| String::from_utf8_lossy(trim_space(f)).go_eq_fold("base64")) {
        return (vec![], vec![]);
    }
    if go_base64_decode(payload, true).is_none() && go_base64_decode(payload, false).is_none() {
        return (vec![], vec![]);
    }
    (mime, payload.to_vec())
}

fn mapped_mime(lower: &[u8], prefix: &str) -> Option<&'static str> {
    crate::mime::mime_type(lower).filter(|m| !m.is_empty() && m.starts_with(prefix))
}

/// openAIResponsesAudioMimeType.
fn audio_mime(format: &[u8]) -> Vec<u8> {
    let format = trim_space(format);
    if is_generic_mime(format) {
        return b"audio/wav".to_vec();
    }
    if format.contains(&b'/') {
        return format.to_vec();
    }
    let lower = go_lower(format);
    let mime: &str = match lower.as_slice() {
        b"wav" => "audio/wav",
        b"mp3" | b"mpeg" => "audio/mpeg",
        b"ogg" => "audio/ogg",
        b"flac" => "audio/flac",
        b"aac" => "audio/aac",
        b"webm" => "audio/webm",
        b"pcm16" | b"pcm" => "audio/pcm",
        b"g711_ulaw" | b"g711_alaw" => "audio/basic",
        b"opus" => "audio/opus",
        b"m4a" => "audio/mp4",
        b"wma" => "audio/x-ms-wma",
        _ => mapped_mime(&lower, "audio/").unwrap_or("audio/wav"),
    };
    mime.as_bytes().to_vec()
}

/// openAIResponsesVideoMimeType.
fn video_mime(format: &[u8]) -> Vec<u8> {
    let format = trim_space(format);
    if is_generic_mime(format) {
        return b"video/mp4".to_vec();
    }
    if format.contains(&b'/') {
        return format.to_vec();
    }
    let lower = go_lower(format);
    let mime: &str = match lower.as_slice() {
        b"mp4" => "video/mp4",
        b"webm" => "video/webm",
        b"mov" | b"quicktime" => "video/quicktime",
        b"avi" | b"x-msvideo" => "video/x-msvideo",
        b"mpeg" => "video/mpeg",
        b"ogg" => "video/ogg",
        b"mkv" | b"x-matroska" => "video/x-matroska",
        b"flv" | b"x-flv" => "video/x-flv",
        b"3gpp" => "video/3gpp",
        _ => mapped_mime(&lower, "video/").unwrap_or("video/mp4"),
    };
    mime.as_bytes().to_vec()
}

/// The data-URL branch shared by audio and video: the URL's MIME type, else the format,
/// the filename's extension, then `default`.
fn media_data_url(
    url: &[u8],
    format: &[u8],
    filename: &[u8],
    mime_of: fn(&[u8]) -> Vec<u8>,
    default: &[u8],
) -> Option<(Vec<u8>, Vec<u8>)> {
    let (mut mime, data) = parse_data_url(url);
    if data.is_empty() {
        return None;
    }
    if is_generic_mime(&mime) {
        mime = if !format.is_empty() && !is_generic_mime(format) {
            mime_of(format)
        } else if !filename.is_empty() {
            mime_of(&ext_lower_raw(filename))
        } else {
            default.to_vec()
        };
    }
    Some((mime, data))
}

/// `strings.TrimPrefix(filepath.Ext(filename), ".")` without lower-casing.
fn ext_lower_raw(filename: &[u8]) -> Vec<u8> {
    let ext = file_ext(filename);
    ext.strip_prefix(b".").unwrap_or(ext).to_vec()
}

fn media_mime(format: &[u8], filename: &[u8], mime_of: fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
    if is_generic_mime(format) && !filename.is_empty() {
        mime_of(&ext_lower_raw(filename))
    } else {
        mime_of(format)
    }
}

type Media = Option<(Vec<u8>, Vec<u8>)>;

/// openAIResponsesAudioFromBlock.
fn audio_from_block(block: &Res<'_>) -> Media {
    let kind = go_lower(trim_space(&field(block, "type")));
    if kind != b"input_audio" && kind != b"audio" {
        return None;
    }
    let filename = first_non_empty(block, &["filename", "file.filename"]);
    let mut audio = block.get("input_audio");
    if !audio.exists() {
        audio = block.get("audio");
    }
    let mut format = [
        audio.get("format"),
        audio.get("mime_type"),
        block.get("format"),
        block.get("mime_type"),
    ]
    .iter()
    .map(|v| trim_space(&v.bytes()).to_vec())
    .find(|v| !v.is_empty() && !is_generic_mime(v))
    .unwrap_or_default();
    let mut data = audio.get("data").bytes().into_owned();
    if data.is_empty() {
        data = field(block, "data").into_owned();
    }
    if data.is_empty() {
        let url = first_non_empty(block, &["audio_url.url", "audio_url", "url"]);
        if !url.is_empty() {
            if is_data_url(&url) {
                return media_data_url(&url, &format, &filename, audio_mime, b"audio/wav");
            } else if !is_remote_url(&url) {
                return Some((media_mime(&format, &filename, audio_mime), url));
            }
        }
    }
    if data.is_empty() && field(block, "source.type").as_ref() == b"base64" {
        data = field(block, "source.data").into_owned();
        if format.is_empty() {
            format = field(block, "source.media_type").into_owned();
        }
    }
    if data.is_empty() {
        return None;
    }
    if is_data_url(&data) {
        return media_data_url(&data, &format, &filename, audio_mime, b"audio/wav");
    }
    Some((media_mime(&format, &filename, audio_mime), data))
}

/// openAIResponsesVideoFromBlock.
fn video_from_block(block: &Res<'_>) -> Media {
    let kind = go_lower(trim_space(&field(block, "type")));
    if kind != b"input_video" && kind != b"video_url" && kind != b"video" {
        return None;
    }
    let filename = first_non_empty(block, &["filename", "file.filename"]);
    let mut video = block.get("input_video");
    if !video.exists() {
        video = block.get("video");
    }
    let mut format = [
        video.get("format"),
        video.get("mime_type"),
        block.get("format"),
        block.get("mime_type"),
    ]
    .iter()
    .map(|v| trim_space(&v.bytes()).to_vec())
    .find(|v| !v.is_empty() && !is_generic_mime(v))
    .unwrap_or_default();
    let url = first_non_empty(block, &["video_url.url", "video_url", "url"]);
    if !url.is_empty() {
        if is_data_url(&url) {
            return media_data_url(&url, &format, &filename, video_mime, b"video/mp4");
        } else if !is_remote_url(&url) {
            return Some((media_mime(&format, &filename, video_mime), url));
        }
    }
    let mut data = video.get("data").bytes().into_owned();
    if data.is_empty() {
        data = field(block, "data").into_owned();
    }
    if data.is_empty() && field(block, "source.type").as_ref() == b"base64" {
        data = field(block, "source.data").into_owned();
        if format.is_empty() {
            format = field(block, "source.media_type").into_owned();
        }
    }
    if data.is_empty() {
        return None;
    }
    if is_data_url(&data) {
        return media_data_url(&data, &format, &filename, video_mime, b"video/mp4");
    }
    Some((media_mime(&format, &filename, video_mime), data))
}

/// normalizeFormatToMIME.
fn format_to_mime(format: &[u8]) -> Vec<u8> {
    let format = trim_space(format);
    if format.is_empty() || is_generic_mime(format) {
        return vec![];
    }
    if format.contains(&b'/') {
        return format.to_vec();
    }
    let lower = go_lower(format);
    let mime = match lower.as_slice() {
        b"jpg" | b"jpeg" => "image/jpeg",
        b"wav" => "audio/wav",
        b"mp3" => "audio/mpeg",
        b"mp4" => "video/mp4",
        b"webm" => "video/webm",
        b"pdf" => "application/pdf",
        _ => crate::mime::mime_type(&lower).unwrap_or_default(),
    };
    mime.as_bytes().to_vec()
}

/// openAIResponsesFileFromBlock.
fn file_from_block(block: &Res<'_>) -> Media {
    let kind = go_lower(trim_space(&field(block, "type")));
    if kind != b"input_file" && kind != b"file" {
        return None;
    }
    let filename = first_non_empty(block, &["filename", "file.filename"]);
    let mut data = first_non_empty(block, &["file_data", "file.file_data", "data"]);
    if data.is_empty() {
        let url = first_non_empty(block, &["file_url.url", "file_url", "file.file_url", "url"]);
        if is_data_url(&url) {
            data = url;
        }
    }
    let mut fallback = first_non_generic(block, &["mime_type", "file.mime_type", "format", "file.format"]);
    if !fallback.is_empty() {
        fallback = format_to_mime(&fallback);
    }
    if is_generic_mime(&fallback) && !filename.is_empty() {
        let ext = ext_lower(&filename);
        if !ext.is_empty() {
            fallback = format_to_mime(&ext);
        }
    }
    if is_data_url(&data) {
        let (mut mime, payload) = parse_data_url(&data);
        if payload.is_empty() {
            return None;
        }
        if is_generic_mime(&mime) && !fallback.is_empty() {
            mime = fallback;
        }
        if is_generic_mime(&mime) && !filename.is_empty() {
            let ext = ext_lower(&filename);
            if !ext.is_empty() {
                let norm = format_to_mime(&ext);
                if !norm.is_empty() {
                    mime = norm;
                }
            }
        }
        if is_generic_mime(&mime) {
            mime = b"application/octet-stream".to_vec();
        }
        return Some((mime, payload));
    }
    normalize_openai_file_data(&filename, &fallback, &data)
}

/// openAIResponsesImageMimeType.
fn image_mime(format: &[u8], filename: &[u8]) -> Vec<u8> {
    let format = trim_space(format);
    if !format.is_empty() && !is_generic_mime(format) {
        if format.contains(&b'/') {
            return format.to_vec();
        }
        let lower = go_lower(format);
        if lower == b"jpg" || lower == b"jpeg" {
            return b"image/jpeg".to_vec();
        }
        if let Some(mapped) = crate::mime::mime_type(&lower).filter(|m| !m.is_empty()) {
            return mapped.as_bytes().to_vec();
        }
        return [b"image/", format].concat();
    }
    if !filename.is_empty() {
        let ext = ext_lower(filename);
        if ext == b"jpg" || ext == b"jpeg" {
            return b"image/jpeg".to_vec();
        }
        if !ext.is_empty()
            && let Some(mapped) = crate::mime::mime_type(&ext).filter(|m| !m.is_empty())
        {
            return mapped.as_bytes().to_vec();
        }
    }
    b"image/png".to_vec()
}

const IMAGE_FORMAT_PATHS: [&str; 6] = [
    "format",
    "mime_type",
    "input_image.format",
    "input_image.mime_type",
    "image.format",
    "image.mime_type",
];

/// openAIResponsesImageFromBlock.
fn image_from_block(block: &Res<'_>) -> Media {
    let kind = go_lower(trim_space(&field(block, "type")));
    if !matches!(kind.as_slice(), b"input_image" | b"image_url" | b"image") {
        return None;
    }
    let mut format = first_non_generic(block, &IMAGE_FORMAT_PATHS);
    let filename = first_non_empty(block, &["filename", "file.filename"]);
    let url = first_non_empty(block, &["image_url.url", "image_url", "url"]);
    if !url.is_empty() {
        if is_data_url(&url) {
            let (mut mime, data) = parse_data_url(&url);
            if data.is_empty() {
                return None;
            }
            if is_generic_mime(&mime) {
                mime = image_mime(&format, &filename);
            }
            return Some((mime, data));
        } else if !is_remote_url(&url) {
            return Some((image_mime(&format, &filename), url));
        }
    }
    let mut data = vec![];
    if field(block, "source.type").as_ref() == b"base64" {
        data = field(block, "source.data").into_owned();
        if format.is_empty() {
            format = field(block, "source.media_type").into_owned();
        }
    }
    if data.is_empty() && block.get("data").exists() {
        data = field(block, "data").into_owned();
    }
    if data.is_empty() {
        return None;
    }
    if is_data_url(&data) {
        let (mut mime, payload) = parse_data_url(&data);
        if payload.is_empty() {
            return None;
        }
        if is_generic_mime(&mime) {
            mime = image_mime(&format, &filename);
        }
        return Some((mime, payload));
    }
    Some((image_mime(&format, &filename), data))
}

/// openAIResponsesMediaFromBlock.
fn media_from_block(block: &Res<'_>) -> Media {
    image_from_block(block)
        .or_else(|| audio_from_block(block))
        .or_else(|| video_from_block(block))
        .or_else(|| file_from_block(block))
}

/// `filepath.Base(url.Parse(raw).Path)`, or empty when Go's url.Parse fails.
// ponytail: url.Parse is reduced to what decides this path: control bytes, invalid
// percent escapes in the authority, path or fragment, and a non-numeric port fail;
// Go's full host-character validation is not ported.
fn url_path_base(raw: &[u8]) -> Vec<u8> {
    if raw.iter().any(|&c| c < 0x20 || c == 0x7f) {
        return vec![];
    }
    fn unescape(s: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(s.len());
        let mut i = 0;
        while i < s.len() {
            if s[i] == b'%' {
                let hex = |c: u8| (c as char).to_digit(16);
                let (h, l) = (hex(*s.get(i + 1)?)?, hex(*s.get(i + 2)?)?);
                out.push((h * 16 + l) as u8);
                i += 3;
            } else {
                out.push(s[i]);
                i += 1;
            }
        }
        Some(out)
    }
    let (rest, fragment) = match raw.iter().position(|&c| c == b'#') {
        Some(i) => (&raw[..i], &raw[i + 1..]),
        None => (raw, &b""[..]),
    };
    if unescape(fragment).is_none() {
        return vec![];
    }
    let rest = match rest.iter().position(|&c| c == b'?') {
        Some(i) => &rest[..i],
        None => rest,
    };
    let Some(colon) = rest.iter().position(|&c| c == b':') else {
        return vec![];
    };
    let mut rest = &rest[colon + 1..];
    if let Some(after) = rest.strip_prefix(b"//") {
        let end = after.iter().position(|&c| c == b'/').unwrap_or(after.len());
        let authority = &after[..end];
        let host = match authority.iter().rposition(|&c| c == b'@') {
            Some(at) => &authority[at + 1..],
            None => authority,
        };
        if unescape(host).is_none() {
            return vec![];
        }
        if !host.starts_with(b"[")
            && let Some(port) = host.iter().rposition(|&c| c == b':').map(|i| &host[i + 1..])
            && !port.iter().all(u8::is_ascii_digit)
        {
            return vec![];
        }
        rest = &after[end..];
    }
    let Some(path) = unescape(rest) else {
        return vec![];
    };
    // filepath.Base
    let mut path = &path[..];
    if path.is_empty() {
        return b".".to_vec();
    }
    while let Some(stripped) = path.strip_suffix(b"/") {
        path = stripped;
    }
    if let Some(slash) = path.iter().rposition(|&c| c == b'/') {
        path = &path[slash + 1..];
    }
    if path.is_empty() { b"/".to_vec() } else { path.to_vec() }
}

/// openAIResponsesPartFromBlock: a `file_data` part for remote URLs, else an
/// `inline_data` part.
fn part_from_block(block: &Res<'_>) -> Option<Vec<u8>> {
    let kind = go_lower(trim_space(&field(block, "type")));
    let url = first_non_empty(
        block,
        &[
            "video_url.url",
            "video_url",
            "audio_url.url",
            "audio_url",
            "image_url.url",
            "image_url",
            "file_url.url",
            "file_url",
            "file.file_url",
            "url",
        ],
    );
    if is_remote_url(&url) {
        let mut filename = first_non_empty(block, &["filename", "file.filename"]);
        if filename.is_empty() {
            filename = url_path_base(&url);
        }
        let format = first_non_generic(
            block,
            &[
                "format",
                "mime_type",
                "input_video.format",
                "input_video.mime_type",
                "video.format",
                "video.mime_type",
                "input_audio.format",
                "input_audio.mime_type",
                "audio.format",
                "audio.mime_type",
                "input_image.format",
                "input_image.mime_type",
                "image.format",
                "image.mime_type",
                "file.format",
                "file.mime_type",
            ],
        );
        let from_ext = |mime_of: fn(&[u8]) -> Vec<u8>, default: &[u8]| {
            let mut mime = vec![];
            if !format.is_empty() && !is_generic_mime(&format) {
                mime = mime_of(&format);
            } else if !filename.is_empty() {
                let ext = ext_lower(&filename);
                if !ext.is_empty() {
                    mime = mime_of(&ext);
                }
            }
            if is_generic_mime(&mime) {
                mime = default.to_vec();
            }
            mime
        };
        let mime = match kind.as_slice() {
            b"input_video" | b"video_url" | b"video" => from_ext(video_mime, b"video/mp4"),
            b"input_audio" | b"audio" => from_ext(audio_mime, b"audio/wav"),
            b"input_image" | b"image_url" | b"image" => image_mime(&format, &filename),
            _ => {
                let mut mime = if format.is_empty() {
                    vec![]
                } else {
                    format_to_mime(&format)
                };
                if is_generic_mime(&mime) && !filename.is_empty() {
                    let ext = ext_lower(&filename);
                    if !ext.is_empty() {
                        mime = format_to_mime(&ext);
                    }
                }
                if is_generic_mime(&mime) {
                    mime = b"application/octet-stream".to_vec();
                }
                mime
            }
        };
        return Some(file_data_part(&mime, &url));
    }
    media_from_block(block).map(|(mime, data)| inline_data_part(&mime, &data))
}

/// parseOpenAIResponsesArrayOutput: (result, is_raw, image parts) of an array tool output.
fn parse_array_output(output: &Res<'_>) -> (Vec<u8>, bool, Vec<Vec<u8>>) {
    let mut images = vec![];
    // (text, is_text, raw)
    let mut entries: Vec<(Vec<u8>, bool, Vec<u8>)> = vec![];
    let (mut has_content_block, mut has_non_text) = (false, false);
    output.each(|_, block| {
        if let Some((mime, data)) = media_from_block(&block) {
            has_content_block = true;
            images.push(inline_data_part(&mime, &data));
            return true;
        }
        let kind = block.get("type").bytes();
        if matches!(kind.as_ref(), b"input_text" | b"output_text" | b"text") {
            has_content_block = true;
            entries.push((field(&block, "text").into_owned(), true, block.raw.to_vec()));
        } else if block.kind == Kind::String {
            entries.push((block.s.to_vec(), true, block.raw.to_vec()));
        } else {
            has_non_text = true;
            entries.push((block.raw.to_vec(), false, block.raw.to_vec()));
        }
        true
    });
    if !has_content_block {
        return (output.raw.to_vec(), true, vec![]);
    }
    match entries.len() {
        0 => (vec![], false, images),
        1 => {
            let (text, is_text, raw) = entries.pop().unwrap();
            if is_text {
                (text, false, images)
            } else {
                (raw, true, images)
            }
        }
        _ if !has_non_text => (
            entries.iter().map(|e| e.0.clone()).collect::<Vec<_>>().join(&b'\n'),
            false,
            images,
        ),
        _ => (
            gj::join(&entries.iter().map(|e| e.2.clone()).collect::<Vec<_>>()),
            true,
            images,
        ),
    }
}

/// buildOpenAIResponsesSynthesizedFunctionResponsePart: an interrupted call's response.
fn synthesized_function_response(call_id: &[u8], names: &HashMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
    let name = names
        .get(call_id)
        .filter(|n| !n.is_empty())
        .cloned()
        .unwrap_or_else(|| b"unknown".to_vec());
    let mut part = br#"{"functionResponse":{"name":"","response":{"result":"call interrupted, no output"}}}"#.to_vec();
    gj::set_str(&mut part, "functionResponse.name", sanitize_function_name(&name));
    if !call_id.is_empty() {
        gj::set_str(&mut part, "functionResponse.id", call_id);
    }
    part
}

fn has_matching_output(items: &[Res<'_>], call_id: &[u8]) -> bool {
    !call_id.is_empty() && items.iter().any(|i| is_tool_output(i) && extract_call_id(i) == call_id)
}

fn has_subsequent_turn(items: &[Res<'_>]) -> bool {
    items.iter().any(|item| {
        let kind = item_type(item);
        kind == b"message" || (kind.is_empty() && !field(item, "role").is_empty()) || is_tool_call(item)
    })
}

/// buildOpenAIResponsesStandaloneToolOutputTextParts: an orphan tool output as user text.
fn standalone_output_text_parts(item: &Res<'_>) -> Vec<Vec<u8>> {
    let output = item.get("output");
    if !output.exists() {
        return vec![];
    }
    if output.is_array() {
        let mut parts = vec![];
        output.each(|_, part| {
            let text = field(&part, "text");
            if !trim_space(&text).is_empty() {
                parts.push(text_part(&text));
            }
            true
        });
        return parts;
    }
    let text = output.bytes();
    if trim_space(&text).is_empty() {
        vec![]
    } else {
        vec![text_part(&text)]
    }
}

/// buildOpenAIResponsesFunctionResponseParts.
fn function_response_part(item: &Res<'_>, names: &HashMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
    let call_id = extract_call_id(item);
    let name = match names.get(&call_id) {
        Some(name) => name.clone(),
        None => {
            let name = trimmed(item, "name");
            if name.is_empty() { b"unknown".to_vec() } else { name }
        }
    };
    let mut part = br#"{"functionResponse":{"name":"","response":{}}}"#.to_vec();
    gj::set_str(&mut part, "functionResponse.name", sanitize_function_name(&name));
    gj::set_str(&mut part, "functionResponse.id", &call_id);
    let output = item.get("output");
    if output.kind == Kind::String {
        if output.s.is_empty() || output.s.as_ref() == b"null" {
            return part;
        }
        gj::set_str(&mut part, "functionResponse.response.result", &output.s);
        return part;
    }
    let mut images = vec![];
    if output.is_array() {
        let (result, raw, imgs) = parse_array_output(&output);
        images = imgs;
        if raw {
            set_function_response_raw(&mut part, "functionResponse.response.result", &result);
        } else {
            gj::set_str(&mut part, "functionResponse.response.result", &result);
        }
    } else if output.is_object() {
        if let Some((mime, data)) = media_from_block(&output) {
            images.push(inline_data_part(&mime, &data));
            gj::set_str(&mut part, "functionResponse.response.result", "");
        } else {
            set_function_response_result(&mut part, "functionResponse.response.result", &output);
        }
    } else if !output.raw.is_empty() && output.raw.as_ref() != b"null" {
        gj::set_str(&mut part, "functionResponse.response.result", output.bytes());
    }
    for image in images {
        let inline = gj::get(&image, "inline_data");
        let file = gj::get(&image, "file_data");
        if inline.exists() {
            let mut data = br#"{"inlineData":{"mimeType":"","data":""}}"#.to_vec();
            gj::set_str(&mut data, "inlineData.mimeType", field(&inline, "mime_type"));
            gj::set_str(&mut data, "inlineData.data", field(&inline, "data"));
            gj::set_raw(&mut part, "functionResponse.parts.-1", data);
        } else if file.exists() {
            let mut data = br#"{"fileData":{"mimeType":"","fileUri":""}}"#.to_vec();
            gj::set_str(&mut data, "fileData.mimeType", field(&file, "mime_type"));
            gj::set_str(&mut data, "fileData.fileUri", field(&file, "file_uri"));
            gj::set_raw(&mut part, "functionResponse.parts.-1", data);
        }
    }
    part
}

/// orderOpenAIResponsesFunctionCallOutputs: outputs in pending-call order, then the rest.
fn order_outputs<'a>(outputs: &[Res<'a>], pending: &[Vec<u8>]) -> Vec<Res<'a>> {
    let mut ordered = Vec::with_capacity(outputs.len());
    let mut used = vec![false; outputs.len()];
    for id in pending {
        if let Some(m) = (0..outputs.len()).find(|&i| !used[i] && extract_call_id(&outputs[i]) == *id) {
            used[m] = true;
            ordered.push(outputs[m].clone());
        }
    }
    ordered.extend(outputs.iter().zip(&used).filter(|(_, u)| !**u).map(|(o, _)| o.clone()));
    ordered
}

fn model_content(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut content = br#"{"role":"model","parts":[]}"#.to_vec();
    gj::set_raw(&mut content, "parts", gj::join(parts));
    content
}

/// buildOpenAIResponsesEmptyReasoningFunctionCallModelContent.
fn empty_reasoning_function_call_content(
    item: &Res<'_>,
    signature: &[u8],
    forward: &HashMap<Vec<u8>, Vec<u8>>,
) -> Vec<u8> {
    let mut thought = br#"{"text":"","thought":true,"thoughtSignature":""}"#.to_vec();
    gj::set_str(&mut thought, "thoughtSignature", signature);
    model_content(&[thought, function_call_part(item, signature, forward)])
}

/// buildOpenAIResponsesReasoningFunctionCallModelContent.
fn reasoning_function_call_content(
    thought_text: &[u8],
    item: &Res<'_>,
    signature: &[u8],
    forward: &HashMap<Vec<u8>, Vec<u8>>,
) -> Vec<u8> {
    let mut parts = vec![];
    if !thought_text.is_empty() {
        let mut thought = br#"{"text":"","thought":true}"#.to_vec();
        gj::set_str(&mut thought, "text", thought_text);
        parts.push(thought);
    }
    parts.push(function_call_part(item, signature, forward));
    model_content(&parts)
}

/// buildOpenAIResponsesReasoningModelContent (`None`: nothing to send).
fn reasoning_model_content(
    thought_text: &[u8],
    visible_text: &[u8],
    signature: &[u8],
    native: bool,
) -> Option<Vec<u8>> {
    let mut content = br#"{"role":"model","parts":[]}"#.to_vec();
    let real_signature = !signature.is_empty() && signature != BYPASS_SIGNATURE;
    let mut parts = vec![];
    if native {
        if thought_text.is_empty() && visible_text.is_empty() {
            if !real_signature {
                return None;
            }
            let mut carrier = br#"{"text":"","thoughtSignature":""}"#.to_vec();
            gj::set_str(&mut carrier, "thoughtSignature", signature);
            parts.push(carrier);
        } else {
            if !thought_text.is_empty() {
                let mut thought = br#"{"text":"","thought":true}"#.to_vec();
                gj::set_str(&mut thought, "text", thought_text);
                if visible_text.is_empty() && real_signature {
                    gj::set_str(&mut thought, "thoughtSignature", signature);
                }
                parts.push(thought);
            }
            if !visible_text.is_empty() {
                let mut visible = text_part(visible_text);
                if real_signature {
                    gj::set_str(&mut visible, "thoughtSignature", signature);
                }
                parts.push(visible);
            }
        }
    } else {
        let mut thought = br#"{"text":"","thought":true}"#.to_vec();
        gj::set_str(&mut thought, "text", thought_text);
        if real_signature {
            gj::set_str(&mut thought, "thoughtSignature", signature);
        }
        parts.push(thought);
    }
    gj::set_items(&mut content, "parts", &parts);
    Some(content)
}

/// openAIResponsesGeminiThoughtSignature.
fn gemini_thought_signature(raw: &[u8]) -> Vec<u8> {
    sig::compatible_signature_for_provider_block(Provider::Gemini, raw, BlockKind::GeminiModelPart)
        .map(String::into_bytes)
        .unwrap_or_default()
}

/// coalesceAdjacentOpenAIResponsesModelContents: consecutive model turns share one.
fn coalesce_model_contents(contents: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let is_model = |c: &[u8]| eq_fold(trim_space(&gj::get(c, "role").bytes()), "model");
    let mut coalesced: Vec<Vec<u8>> = Vec::with_capacity(contents.len());
    for content in contents {
        let parts = gj::get(&content, "parts");
        match coalesced.last_mut() {
            Some(last) if is_model(&content) && is_model(last) && parts.is_array() => {
                let extra: Vec<Vec<u8>> = parts.array().iter().map(|p| p.raw.to_vec()).collect();
                if !extra.is_empty() {
                    let mut existing: Vec<Vec<u8>> = vec![];
                    gj::get(last, "parts").each(|_, p| {
                        existing.push(p.raw.to_vec());
                        true
                    });
                    existing.extend(extra);
                    gj::set_items(last, "parts", &existing);
                }
            }
            _ => coalesced.push(content),
        }
    }
    coalesced
}

/// common.MergeAdjacentGeminiUserContents: drops contents without parts and merges
/// consecutive user turns that hold no function responses.
pub(crate) fn merge_adjacent_user_contents(contents: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    if contents.len() <= 1 {
        return contents;
    }
    let mut merged: Vec<Vec<u8>> = vec![];
    for content in contents {
        if content.is_empty() {
            continue;
        }
        let parts = gj::get(&content, "parts");
        if !parts.is_array() || parts.raw.as_ref() == b"[]" || !parts.get("0").exists() {
            continue;
        }
        if let Some(last) = merged.last_mut()
            && gj::get(last, "role").bytes().as_ref() == b"user"
            && gj::get(&content, "role").bytes().as_ref() == b"user"
            && !crate::gemini::has_function_response(last)
            && !crate::gemini::has_function_response(&content)
        {
            let mut combined: Vec<Vec<u8>> = gj::get(last, "parts").array().iter().map(|p| p.raw.to_vec()).collect();
            combined.extend(parts.array().iter().map(|p| p.raw.to_vec()));
            if let Ok(updated) = gj::try_set_raw(last, "parts", gj::join(&combined)) {
                *last = updated;
                continue;
            }
        }
        merged.push(content);
    }
    merged
}

/// stripTrailingOpenAIResponsesModelPrefill: a final plain-text model turn is a prefill.
fn strip_trailing_model_prefill(payload: Vec<u8>) -> Vec<u8> {
    let contents = gj::get(&payload, "contents");
    if !contents.is_array() {
        return payload;
    }
    let items = contents.array();
    let Some(last) = items.last() else {
        return payload;
    };
    let parts = last.get("parts");
    if last.get("role").bytes().as_ref() != b"model" || !parts.is_array() {
        return payload;
    }
    if parts.array().iter().any(|p| {
        p.get("thought").bool() || p.get("functionCall").exists() || !trimmed(p, "thoughtSignature").is_empty()
    }) {
        return payload;
    }
    let kept: Vec<Vec<u8>> = items[..items.len() - 1].iter().map(|c| c.raw.to_vec()).collect();
    let mut out = payload.clone();
    if kept.is_empty() {
        return match gj::try_set_raw(&payload, "contents", b"[]") {
            Ok(updated) => updated,
            Err(_) => payload,
        };
    }
    gj::set_items(&mut out, "contents", &kept);
    out
}

/// applyOpenAIResponsesTextFormatToGemini.
fn apply_text_format(out: &mut Vec<u8>, root: &Res<'_>) {
    let format = root.get("text.format");
    if !format.exists() {
        return;
    }
    match go_lower(trim_space(&format.get("type").bytes())).as_slice() {
        b"json_object" => {
            gj::set_str(out, "generationConfig.responseMimeType", "application/json");
        }
        b"json_schema" => {
            gj::set_str(out, "generationConfig.responseMimeType", "application/json");
            let mut schema = format.get("schema");
            if !schema.exists() {
                schema = format.get("json_schema.schema");
            }
            if schema.exists() {
                gj::set_raw(out, "generationConfig.responseJsonSchema", &schema.raw);
            }
        }
        _ => {}
    }
}

/// strings.EqualFold.
fn eq_fold(value: &[u8], token: &str) -> bool {
    use cpa_common::gostr::GoStr;
    String::from_utf8_lossy(value).go_eq_fold(token)
}

fn effective_role(role: &[u8]) -> Vec<u8> {
    if role.is_empty() {
        return b"user".to_vec();
    }
    match go_lower(role).as_slice() {
        b"assistant" | b"model" => b"model".to_vec(),
        other => other.to_vec(),
    }
}

/// ConvertOpenAIResponsesRequestToGemini.
pub(crate) fn convert(model: &str, raw: &[u8]) -> Vec<u8> {
    let mut native = sig::provider_from_model_name(model) == Provider::Gemini;
    let mut out = br#"{"contents":[]}"#.to_vec();
    let root = gj::parse(raw);

    let (declarations, forward, _) = gemini_function_declarations(&root);
    let mut tool_blocks = vec![];
    if ws::has_web_search_tool(&root)
        && ws::model_supports_web_search(model)
        && ws::allows_web_search_tool_choice(&root)
    {
        let mut block = br#"{"googleSearch":{}}"#.to_vec();
        let domains = ws::allowed_domains(&root);
        if !domains.is_empty() {
            gj::set_raw(&mut block, "googleSearch.includedDomains", gj::quote_all(&domains));
        }
        tool_blocks.push(block);
    }
    if !declarations.is_empty() {
        let mut block = br#"{"functionDeclarations":[]}"#.to_vec();
        gj::set_raw(&mut block, "functionDeclarations", gj::join(&declarations));
        tool_blocks.push(block);
    }
    if !tool_blocks.is_empty() {
        gj::set_raw(&mut out, "tools", gj::join(&tool_blocks));
    }
    if !declarations.is_empty() {
        let choice = root.get("tool_choice");
        if choice.exists()
            && let Some(config) = tool_choice_to_gemini(&choice, &forward)
        {
            gj::set_raw(&mut out, "toolConfig.functionCallingConfig", config);
        }
    }

    let mut system_parts: Vec<Vec<u8>> = vec![];
    let instructions = root.get("instructions");
    if instructions.exists() {
        system_parts.push(text_part(&instructions.bytes()));
    }

    let input = root.get("input");
    if input.is_array() {
        let restored = restore_text_signatures(model, &input.array());
        let (items, has_carrier) = normalize_carriers(&restored);
        native |= has_carrier;
        let items = normalize_tool_call_outputs(items);
        let items = pair_reasoning_with_function_calls(&items);
        let mut contents = convert_items(&items, native, &forward, &mut system_parts);
        contents = coalesce_model_contents(contents);
        contents = merge_adjacent_user_contents(contents);
        gj::set_items(&mut out, "contents", &contents);
    } else if input.kind == Kind::String {
        gj::set_items(&mut out, "contents", &[content("user", &[text_part(&input.s)])]);
    }
    if !system_parts.is_empty() {
        let mut system = br#"{"parts":[]}"#.to_vec();
        gj::set_raw(&mut system, "parts", gj::join(&system_parts));
        gj::set_raw(&mut out, "systemInstruction", system);
    }

    let max_output = root.get("max_output_tokens");
    if max_output.exists() {
        let mut config = br#"{"maxOutputTokens":0}"#.to_vec();
        gj::set_int(&mut config, "maxOutputTokens", max_output.int());
        gj::set_raw(&mut out, "generationConfig", config);
    }
    let temperature = root.get("temperature");
    if temperature.exists() {
        gj::set_f64(&mut out, "generationConfig.temperature", temperature.float());
    }
    let top_p = root.get("top_p");
    if top_p.exists() {
        gj::set_f64(&mut out, "generationConfig.topP", top_p.float());
    }
    let stop = root.get("stop_sequences");
    if stop.is_array() {
        let mut sequences: Vec<Vec<u8>> = vec![];
        stop.each(|_, s| {
            sequences.push(s.bytes().into_owned());
            true
        });
        if sequences.is_empty() {
            // json.Marshal of a nil []string.
            gj::set_raw(&mut out, "generationConfig.stopSequences", b"null");
        } else {
            gj::set_strs(&mut out, "generationConfig.stopSequences", &sequences);
        }
    }
    apply_text_format(&mut out, &root);

    let effort = root.get("reasoning.effort");
    if effort.exists() {
        let effort = go_lower(trim_space(&effort.bytes()));
        if effort == b"auto" {
            gj::set_int(&mut out, "generationConfig.thinkingConfig.thinkingBudget", -1);
        } else if !effort.is_empty() {
            gj::set_str(&mut out, "generationConfig.thinkingConfig.thinkingLevel", &effort);
        }
    }

    let mut result = crate::gemini::attach_default_safety_settings(out, "safetySettings");
    if native {
        result = sig::sanitize_gemini_request_thought_signatures(&result, "contents");
    }
    strip_trailing_model_prefill(result)
}

/// The `input` loop of ConvertOpenAIResponsesRequestToGemini.
fn convert_items(
    items: &[Res<'_>],
    native: bool,
    forward: &HashMap<Vec<u8>, Vec<u8>>,
    system_parts: &mut Vec<Vec<u8>>,
) -> Vec<Vec<u8>> {
    let mut contents: Vec<Vec<u8>> = vec![];
    let mut names_by_call: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    for item in items.iter().filter(|i| is_tool_call(i)) {
        let call_id = extract_call_id(item);
        names_by_call.entry(call_id).or_insert_with(|| {
            let mut name = field(item, "name").into_owned();
            let namespace = field(item, "namespace");
            if !namespace.is_empty() {
                name = qualify_namespace_name(&namespace, &name);
            }
            map_tool_name(forward, &name)
        });
    }
    let normalized: Vec<Res<'_>> = if native {
        reorder_detached_reasoning(items)
    } else {
        items.to_vec()
    };
    let mut pending: Vec<Vec<u8>> = vec![];
    let mut consumed_outputs: HashSet<usize> = HashSet::new();
    let mut in_conversation = false;
    let mut pending_developer: Vec<Vec<u8>> = vec![];
    let mut i = 0;
    while i < normalized.len() {
        if consumed_outputs.contains(&i) {
            i += 1;
            continue;
        }
        let item = &normalized[i];
        let mut kind = item_type(item);
        let mut role = field(item, "role").into_owned();
        if kind.is_empty() && !role.is_empty() {
            kind = b"message".to_vec();
        } else if is_content_part_type(&kind) && role.is_empty() {
            kind = b"message".to_vec();
            role = b"user".to_vec();
        }
        match kind.as_slice() {
            b"message" => {
                if eq_fold(&role, "system") || eq_fold(&role, "developer") {
                    let content_value = item.get("content");
                    if !in_conversation {
                        pending.clear();
                        if content_value.is_array() {
                            content_value.each(|_, c| {
                                system_parts.push(text_part(&c.get("text").bytes()));
                                true
                            });
                        } else if content_value.kind == Kind::String {
                            system_parts.push(text_part(&content_value.s));
                        }
                        i += 1;
                        continue;
                    }
                    let mut developer_parts = vec![];
                    if content_value.is_array() {
                        let mut texts: Vec<Vec<u8>> = vec![];
                        content_value.each(|_, c| {
                            let mut text = c.get("text").bytes().into_owned();
                            if text.is_empty() && c.kind == Kind::String {
                                text = c.s.to_vec();
                            }
                            if !text.is_empty() {
                                texts.push(text);
                            }
                            true
                        });
                        let joined = texts.join(&b'\n');
                        if !texts.is_empty() && !trim_space(&joined).is_empty() {
                            developer_parts.push(text_part(&crate::common::system_reminder_text(&joined)));
                        }
                    } else if content_value.kind == Kind::String && !trim_space(&content_value.s).is_empty() {
                        developer_parts.push(text_part(&crate::common::system_reminder_text(&content_value.s)));
                    }
                    if !developer_parts.is_empty() {
                        if pending.is_empty() {
                            contents.push(content("user", &developer_parts));
                        } else {
                            pending_developer.extend(developer_parts);
                        }
                    }
                    i += 1;
                    continue;
                }

                in_conversation = true;
                if assistant_visible_text(item).is_none() {
                    if !pending.is_empty() && !pending.iter().any(|id| has_matching_output(&normalized[i..], id)) {
                        let parts: Vec<Vec<u8>> = pending
                            .iter()
                            .map(|id| synthesized_function_response(id, &names_by_call))
                            .collect();
                        contents.push(content("user", &parts));
                        pending.clear();
                    }
                    if !pending_developer.is_empty() {
                        contents.push(content("user", &pending_developer));
                        pending_developer.clear();
                    }
                }

                let content_value = item.get("content");
                let mut parts_to_process: Vec<Res<'_>> = vec![];
                if content_value.is_array() {
                    parts_to_process = content_value.array();
                } else if is_content_part_type(&item_type(item)) {
                    parts_to_process.push(item.clone());
                    while i + 1 < normalized.len() {
                        let next = &normalized[i + 1];
                        if field(next, "role").is_empty() && is_content_part_type(&item_type(next)) {
                            parts_to_process.push(next.clone());
                            i += 1;
                        } else {
                            break;
                        }
                    }
                }
                if !parts_to_process.is_empty() {
                    let mut current_role: Vec<u8> = vec![];
                    let mut current_parts: Vec<Vec<u8>> = vec![];
                    for part in &parts_to_process {
                        let mut part_type = item_type(part);
                        if part_type.is_empty() {
                            part_type = b"input_text".to_vec();
                        }
                        let mut eff_role = effective_role(&role);
                        if part_type == b"output_text" || eff_role == b"assistant" {
                            eff_role = b"model".to_vec();
                        }
                        if !current_role.is_empty() && eff_role != current_role {
                            if !current_parts.is_empty() {
                                contents.push(content(&current_role, &current_parts));
                            }
                            current_parts.clear();
                            current_role.clear();
                        }
                        if current_role.is_empty() {
                            current_role = eff_role;
                        }
                        let built = match part_type.as_slice() {
                            b"input_text" | b"output_text" | b"text" => {
                                let text = part.get("text");
                                text.exists().then(|| text_part(&text.bytes()))
                            }
                            _ => part_from_block(part),
                        };
                        if let Some(built) = built.filter(|p| !p.is_empty()) {
                            current_parts.push(built);
                        }
                    }
                    if !current_role.is_empty() && !current_parts.is_empty() {
                        contents.push(content(&current_role, &current_parts));
                    }
                } else if content_value.kind == Kind::String {
                    contents.push(content(effective_role(&role), &[text_part(&content_value.s)]));
                }
            }
            b"function_call" | b"custom_tool_call" => {
                in_conversation = true;
                let raw_signature = trimmed(item, SIGNATURE_FIELD);
                let signature = if raw_signature.is_empty() {
                    BYPASS_SIGNATURE.to_vec()
                } else {
                    sig::gemini_replay_signature_or_bypass(&raw_signature, BlockKind::GeminiFunctionCall).into_bytes()
                };
                let thought_text = field(item, SUMMARY_FIELD);
                if !thought_text.is_empty() {
                    contents.push(reasoning_function_call_content(
                        &thought_text,
                        item,
                        &signature,
                        forward,
                    ));
                } else if !native && !raw_signature.is_empty() {
                    contents.push(empty_reasoning_function_call_content(item, &signature, forward));
                } else {
                    contents.push(model_content(&[function_call_part(item, &signature, forward)]));
                }
                let call_id = extract_call_id(item);
                if !call_id.is_empty() {
                    pending.push(call_id);
                }
            }
            b"function_call_output" | b"custom_tool_call_output" => {
                in_conversation = true;
                let mut end = i + 1;
                while end < normalized.len() && is_tool_output(&normalized[end]) {
                    end += 1;
                }
                let ordered = order_outputs(&normalized[i..end], &pending);
                consumed_outputs.extend(i..end);
                let has_subsequent = has_subsequent_turn(&normalized[end..]);
                let mut by_call: HashMap<Vec<u8>, Res<'_>> = HashMap::new();
                let mut extra: Vec<Res<'_>> = vec![];
                for output in &ordered {
                    let id = extract_call_id(output);
                    if id.is_empty() {
                        extra.push(output.clone());
                    } else {
                        by_call.insert(id, output.clone());
                    }
                }
                let any_matched = pending.iter().any(|id| by_call.contains_key(id));
                let mut response_parts = vec![];
                let mut still_pending = vec![];
                for id in &pending {
                    if let Some(output) = by_call.remove(id) {
                        response_parts.push(function_response_part(&output, &names_by_call));
                    } else if (has_subsequent || any_matched) && !has_matching_output(&normalized[end..], id) {
                        response_parts.push(synthesized_function_response(id, &names_by_call));
                    } else {
                        still_pending.push(id.clone());
                    }
                }
                let mut standalone = vec![];
                let mut append_standalone = |output: &Res<'_>| {
                    let parts = standalone_output_text_parts(output);
                    if !parts.is_empty() {
                        standalone.push(content("user", &parts));
                    }
                };
                for output in &ordered {
                    let id = extract_call_id(output);
                    if by_call.remove(&id).is_some() {
                        append_standalone(output);
                    }
                }
                for output in &extra {
                    append_standalone(output);
                }
                pending = still_pending;
                if !response_parts.is_empty() {
                    contents.push(content("user", &response_parts));
                }
                contents.extend(standalone);
                if pending.is_empty() && !pending_developer.is_empty() {
                    contents.push(content("user", &pending_developer));
                    pending_developer.clear();
                }
            }
            b"reasoning" => {
                in_conversation = true;
                let thought_text = field(item, "summary.0.text").into_owned();
                let mut raw_signature = field(item, "encrypted_content").into_owned();
                let (dir, tgt) = (direction(item), target(item));
                if trim_space(&raw_signature).is_empty() && i + 1 < normalized.len() {
                    let next = &normalized[i + 1];
                    if item_type(next) == b"reasoning"
                        && contains(&field(next, "id"), b"_detached_after_")
                        && trimmed(next, "summary.0.text").is_empty()
                        && !trimmed(next, "encrypted_content").is_empty()
                    {
                        raw_signature = field(next, "encrypted_content").into_owned();
                        i += 1;
                    }
                }
                let signature = if trim_space(&raw_signature).is_empty() {
                    vec![]
                } else {
                    gemini_thought_signature(&raw_signature)
                };
                let mut visible_text = vec![];
                if native && i + 1 < normalized.len() {
                    let next = &normalized[i + 1];
                    let binds_next = dir.is_empty() || is(&dir, NEXT);
                    let can_bind_text = binds_next && (tgt.is_empty() || is(&tgt, TEXT) || is(&tgt, ANY));
                    let can_bind_function = binds_next && (tgt.is_empty() || is(&tgt, FUNCTION) || is(&tgt, ANY));
                    let visible = assistant_visible_text(next);
                    if let Some(visible) = visible.filter(|_| can_bind_text) {
                        visible_text = visible;
                        i += 1;
                    } else if is_tool_call(next) && can_bind_function && trimmed(next, SIGNATURE_FIELD).is_empty() {
                        let function_signature = if signature.is_empty() {
                            BYPASS_SIGNATURE.to_vec()
                        } else {
                            signature.clone()
                        };
                        contents.push(reasoning_function_call_content(
                            &thought_text,
                            next,
                            &function_signature,
                            forward,
                        ));
                        let call_id = extract_call_id(next);
                        if !call_id.is_empty() {
                            pending.push(call_id);
                        }
                        i += 2;
                        continue;
                    }
                }
                if let Some(content) = reasoning_model_content(&thought_text, &visible_text, &signature, native) {
                    contents.push(content);
                }
            }
            _ => {}
        }
        i += 1;
    }
    if !pending_developer.is_empty() {
        contents.push(content("user", &pending_developer));
    }
    contents
}
