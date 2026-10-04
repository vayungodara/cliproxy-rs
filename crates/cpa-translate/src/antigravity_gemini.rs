//! Gemini client, Antigravity upstream (internal/translator/antigravity/gemini):
//! ConvertGeminiRequestToAntigravity, ConvertAntigravityResponseToGemini, its NonStream
//! and GeminiTokenCount. The OpenAI Chat and Responses Antigravity translators reuse the
//! request envelope.

use std::collections::{HashMap, HashSet, VecDeque};

use cpa_common::json::{self as gj, GoValue, Kind, Res};
use cpa_common::signature::{
    compatible_antigravity_claude_thinking_signature, sanitize_gemini_request_thought_signatures,
};

use crate::common::{go_lower, restore_sanitized_tool_name, trim_space};
use crate::gemini::{attach_default_safety_settings, has_function_response};
use crate::responses_tools::{disambiguated_tool_name_map, map_sanitized_function_name, sanitized_function_name_map};
use crate::stream::GoStream;
use crate::{Error, Registered, ResponseCtx};

pub static PAIR: Registered = registered!(
    Gemini -> Antigravity,
    request: |ctx, body| Ok(convert(ctx.model, body)),
    non_stream: |ctx, body| Ok(non_stream(ctx.original_request, body)),
    go_stream: go_stream,
    token_count: Some(crate::gemini::token_count),
);

/// The part fields that can carry a function name.
pub(crate) const NAME_FIELDS: [&str; 4] = ["functionCall", "functionResponse", "function_call", "function_response"];

// ---------------------------------------------------------------------------------------
// Request

/// ConvertGeminiRequestToAntigravity: wraps the Gemini request in the Antigravity
/// envelope, groups function responses with their calls, normalizes roles, schemas and
/// tool declarations, maps function names to Gemini-safe names and sanitizes thought
/// signatures. A request without `contents` becomes an empty body.
pub(crate) fn convert(model: &str, input: &[u8]) -> Vec<u8> {
    let names = sanitized_function_name_map(input);
    let mut raw = br#"{"project":"","request":{},"model":""}"#.to_vec();
    gj::set_str(&mut raw, "model", model);
    gj::set_raw(&mut raw, "request", input);
    if gj::get(&raw, "request.model").exists() {
        gj::delete(&mut raw, "request.model");
    }
    let Some(mut raw) = fix_cli_tool_response(&raw) else {
        return vec![];
    };

    let system = gj::get(&raw, "request.system_instruction").into_owned();
    if system.exists() {
        gj::set_raw(&mut raw, "request.systemInstruction", &system.raw);
        gj::delete(&mut raw, "request.system_instruction");
    }
    normalize_response_schema(&mut raw);
    normalize_roles(&mut raw);

    let tools = gj::get(&raw, "request.tools").into_owned();
    if tools.is_array() {
        normalize_tools(&mut raw, &tools, &names);
        remove_empty_function_tools(&mut raw);
    }
    rewrite_function_names(
        &mut raw,
        &names,
        "request.contents",
        &NAME_FIELDS,
        &[
            "request.toolConfig.functionCallingConfig.allowedFunctionNames",
            "request.tool_config.function_calling_config.allowed_function_names",
        ],
    );

    let raw = if go_lower(model.as_bytes()).windows(6).any(|w| w == b"claude") {
        sanitize_claude_signatures(raw)
    } else {
        sanitize_gemini_request_thought_signatures(&raw, "request.contents")
    };
    attach_default_safety_settings(raw, "request.safetySettings")
}

/// normalizeGeminiGenerationConfigResponseSchema: `responseJsonSchema` (either spelling)
/// becomes `responseSchema` unless one is already set.
fn normalize_response_schema(raw: &mut Vec<u8>) {
    for container in ["request.generationConfig", "request.generation_config"] {
        if !gj::get(raw, container).exists() {
            continue;
        }
        for key in ["responseJsonSchema", "response_json_schema"] {
            let old = format!("{container}.{key}");
            let schema = gj::get(raw, &old).into_owned();
            if schema.exists() {
                let target = format!("{container}.responseSchema");
                if !gj::get(raw, &target).exists() {
                    gj::set_raw(raw, &target, &schema.raw);
                }
                gj::delete(raw, &old);
            }
        }
    }
}

/// Missing or invalid content roles: function responses are user turns, anything else
/// alternates from the previous role. The array is only rebuilt when a role changes.
fn normalize_roles(raw: &mut Vec<u8>) {
    let contents = gj::get(raw, "request.contents").into_owned();
    if !contents.is_array() {
        return;
    }
    let valid = |c: &Res<'_>| matches!(c.get("role").bytes().as_ref(), b"user" | b"model");
    let mut needed = false;
    contents.each(|_, c| {
        needed = !valid(&c);
        !needed
    });
    if !needed {
        return;
    }
    let mut items = vec![];
    let mut previous: Vec<u8> = vec![];
    contents.each(|_, value| {
        let mut content = value.raw.to_vec();
        let role = if valid(&value) {
            value.get("role").bytes().into_owned()
        } else {
            let role: &[u8] = if has_function_response(&value.raw) || previous.is_empty() || previous == b"model" {
                b"user"
            } else {
                b"model"
            };
            gj::set_str(&mut content, "role", role);
            role.to_vec()
        };
        previous = role;
        items.push(content);
        true
    });
    gj::set_raw(raw, "request.contents", gj::join(&items));
}

/// Declarations get their request-specific names (duplicates after mapping are dropped,
/// non-string names become strings) and `parameters` moves to `parametersJsonSchema`.
fn normalize_tools(raw: &mut Vec<u8>, tools: &Res<'_>, names: &HashMap<Vec<u8>, Vec<u8>>) {
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut tools_changed = false;
    let mut items = vec![];
    tools.each(|_, tool| {
        let mut tool_json = tool.raw.to_vec();
        for key in ["functionDeclarations", "function_declarations"] {
            let declarations = tool.get(key);
            if !declarations.is_array() {
                continue;
            }
            let mut changed = false;
            let mut kept = vec![];
            declarations.each(|_, declaration| {
                let name = declaration.get("name");
                let original = name.bytes();
                let mapped = map_sanitized_function_name(names, &original);
                if !mapped.is_empty() && !seen.insert(mapped.clone()) {
                    changed = true;
                    return true;
                }
                let mut out = declaration.raw.to_vec();
                if name.kind != Kind::String || mapped != *original {
                    gj::set_str(&mut out, "name", &mapped);
                    changed = true;
                }
                let parameters = declaration.get("parameters");
                if parameters.exists() {
                    gj::set_raw(&mut out, "parametersJsonSchema", &parameters.raw);
                    gj::delete(&mut out, "parameters");
                    changed = true;
                }
                kept.push(out);
                true
            });
            if changed && gj::set_raw(&mut tool_json, key, gj::join(&kept)) {
                tools_changed = true;
            }
        }
        items.push(tool_json);
        true
    });
    if tools_changed {
        gj::set_raw(raw, "request.tools", gj::join(&items));
    }
}

/// removeEmptyGeminiFunctionTools: empty declaration lists, then tools left empty, then
/// an empty `tools` array are removed.
pub(crate) fn remove_empty_function_tools(raw: &mut Vec<u8>) {
    let tools = gj::get(raw, "request.tools").into_owned();
    if tools.is_array() && tools.array().is_empty() {
        gj::delete(raw, "request.tools");
        return;
    }
    let mut changed = false;
    let mut cleaned = vec![];
    for tool in tools.array() {
        let mut tool_json = tool.raw.to_vec();
        if tool.is_object() {
            for key in ["functionDeclarations", "function_declarations"] {
                let declarations = tool.get(key);
                if declarations.is_array() && declarations.array().is_empty() {
                    gj::delete(&mut tool_json, key);
                    changed = true;
                }
            }
            if gj::parse(&tool_json).map().is_empty() {
                changed = true;
                continue;
            }
        }
        cleaned.push(tool_json);
    }
    if !changed {
        return;
    }
    if cleaned.is_empty() {
        gj::delete(raw, "request.tools");
    } else {
        gj::set_raw(raw, "request.tools", gj::join(&cleaned));
    }
}

/// The mapped name for a part's function-name field, when it must be rewritten.
fn renamed(part: &Res<'_>, field: &str, names: &HashMap<Vec<u8>, Vec<u8>>) -> Option<Vec<u8>> {
    let name = part.get(&format!("{field}.name"));
    let current = name.bytes();
    if current.is_empty() {
        return None;
    }
    let mapped = map_sanitized_function_name(names, &current);
    (name.kind != Kind::String || mapped != *current).then_some(mapped)
}

/// rewriteGeminiFunctionNames: function names in `fields` of the contents' parts and the
/// allowed function names become their request-specific Gemini-safe names.
pub(crate) fn rewrite_function_names(
    raw: &mut Vec<u8>,
    names: &HashMap<Vec<u8>, Vec<u8>>,
    contents_path: &str,
    fields: &[&str],
    allowed_paths: &[&str],
) {
    let contents = gj::get(raw, contents_path).into_owned();
    let mut can_batch = contents.is_array();
    if can_batch {
        contents.each(|_, content| {
            let parts = content.get("parts");
            can_batch = !(parts.exists() && !parts.is_array());
            can_batch
        });
    }
    if can_batch {
        let mut needed = false;
        contents.each(|_, content| {
            content.get("parts").each(|_, part| {
                needed = fields.iter().any(|f| renamed(&part, f, names).is_some());
                !needed
            });
            !needed
        });
        if needed {
            let mut items = vec![];
            contents.each(|_, content| {
                let mut content_json = content.raw.to_vec();
                let mut changed = false;
                let mut parts = vec![];
                content.get("parts").each(|_, part| {
                    let mut part_json = part.raw.to_vec();
                    for field in fields {
                        if let Some(mapped) = renamed(&part, field, names) {
                            gj::set_str(&mut part_json, &format!("{field}.name"), &mapped);
                            changed = true;
                        }
                    }
                    parts.push(part_json);
                    true
                });
                if changed {
                    gj::set_raw(&mut content_json, "parts", gj::join(&parts));
                }
                items.push(content_json);
                true
            });
            gj::set_raw(raw, contents_path, gj::join(&items));
        }
    } else {
        for (ci, content) in contents.array().iter().enumerate() {
            for (pi, part) in content.get("parts").array().iter().enumerate() {
                for field in fields {
                    if let Some(mapped) = renamed(part, field, names) {
                        gj::set_str(raw, &format!("{contents_path}.{ci}.parts.{pi}.{field}.name"), &mapped);
                    }
                }
            }
        }
    }

    for &path in allowed_paths {
        let allowed = gj::get(raw, path).into_owned();
        if allowed.is_array() {
            let mut changed = false;
            let mut items = vec![];
            allowed.each(|_, name| {
                let current = name.bytes();
                let mapped = map_sanitized_function_name(names, &current);
                changed |= name.kind != Kind::String || mapped != *current;
                items.push(gj::quote(&mapped));
                true
            });
            if changed {
                gj::set_raw(raw, path, gj::join(&items));
            }
        } else {
            for (i, name) in allowed.array().iter().enumerate() {
                let current = name.bytes();
                let mapped = map_sanitized_function_name(names, &current);
                if name.kind == Kind::String && mapped == *current {
                    continue;
                }
                gj::set_str(raw, &format!("{path}.{i}"), &mapped);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Function response grouping (fixCLIToolResponse)

/// normalizeAntigravityInlineDataPart: an inline image part in canonical form; a missing
/// MIME type defaults to `image/png`.
fn inline_data_part(part: &Res<'_>) -> Option<Vec<u8>> {
    let mut inline = part.get("inlineData");
    if !inline.exists() {
        inline = part.get("inline_data");
    }
    if !inline.exists() {
        return None;
    }
    let data = inline.get("data").bytes();
    if data.is_empty() {
        return None;
    }
    let mut mime = inline.get("mimeType").bytes();
    if mime.is_empty() {
        mime = inline.get("mime_type").bytes();
    }
    let mut out = br#"{"inlineData":{"mimeType":"","data":""}}"#.to_vec();
    gj::set_str(
        &mut out,
        "inlineData.mimeType",
        if mime.is_empty() { &b"image/png"[..] } else { &mime },
    );
    gj::set_str(&mut out, "inlineData.data", &data);
    Some(out)
}

fn attach_images(response: &mut Vec<u8>, images: &[Vec<u8>]) {
    for image in images {
        gj::set_raw(response, "functionResponse.parts.-1", image);
    }
}

/// collectFunctionResponsesWithSiblingInlineData: the function-response parts, with each
/// sibling inline image attached to the nearest preceding response (leading images go to
/// the first one).
fn collect_function_responses(parts: &Res<'_>) -> Vec<Vec<u8>> {
    let mut responses: Vec<Vec<u8>> = vec![];
    let mut leading: Vec<Vec<u8>> = vec![];
    parts.each(|_, part| {
        if part.get("functionResponse").exists() {
            let mut response = part.raw.to_vec();
            attach_images(&mut response, &std::mem::take(&mut leading));
            responses.push(response);
            return true;
        }
        if let Some(image) = inline_data_part(&part) {
            match responses.last_mut() {
                Some(current) => attach_images(current, &[image]),
                None => leading.push(image),
            }
        }
        true
    });
    responses
}

/// parseFunctionResponseRaw: a valid response object (with `fallback` for an empty
/// name), else a minimal response rebuilt from what can be read.
fn function_response_raw(response: &[u8], fallback: &[u8]) -> Vec<u8> {
    let parsed = gj::parse(response);
    if parsed.is_object() && gj::valid(&parsed.raw) {
        let mut raw = parsed.raw.to_vec();
        if trim_space(&parsed.get("functionResponse.name").bytes()).is_empty() && !fallback.is_empty() {
            gj::set_str(&mut raw, "functionResponse.name", fallback);
        }
        return raw;
    }
    let mut out = br#"{"functionResponse":{"name":"","response":{"result":""}}}"#.to_vec();
    let function_response = parsed.get("functionResponse");
    if function_response.exists() {
        let mut name = function_response.get("name").bytes().into_owned();
        if trim_space(&name).is_empty() {
            name = fallback.to_vec();
        }
        gj::set_str(&mut out, "functionResponse.name", &name);
        gj::set_str(
            &mut out,
            "functionResponse.response.result",
            function_response.get("response").bytes(),
        );
        let id = function_response.get("id").bytes();
        if !id.is_empty() {
            gj::set_str(&mut out, "functionResponse.id", &id);
        }
        return out;
    }
    gj::set_str(
        &mut out,
        "functionResponse.name",
        if fallback.is_empty() { b"unknown" } else { fallback },
    );
    gj::set_str(&mut out, "functionResponse.response.result", parsed.bytes());
    out
}

/// fixCLIToolResponse: function responses leave their contents and follow the model turn
/// whose calls they answer (oldest pending group first) as one `function` content.
/// Contents that are not objects are dropped. `None` when there are no contents.
pub(crate) fn fix_cli_tool_response(input: &[u8]) -> Option<Vec<u8>> {
    let contents = gj::get(input, "request.contents");
    if !contents.exists() {
        return None;
    }
    let mut grouping = false;
    let mut all_objects = true;
    contents.each(|_, content| {
        if !content.is_object() {
            all_objects = false;
            return true;
        }
        content.get("parts").each(|_, part| {
            grouping = part.get("functionResponse").exists();
            !grouping
        });
        !grouping
    });
    if contents.is_array() && all_objects && !grouping {
        return Some(input.to_vec());
    }

    let mut items: Vec<Vec<u8>> = vec![];
    let mut pending: VecDeque<Vec<Vec<u8>>> = VecDeque::new();
    let mut collected: VecDeque<Vec<u8>> = VecDeque::new();
    let append = |items: &mut Vec<Vec<u8>>, responses: Vec<Vec<u8>>, names: &[Vec<u8>]| {
        let parts: Vec<Vec<u8>> = responses
            .iter()
            .zip(names)
            .map(|(response, name)| function_response_raw(response, name))
            .collect();
        if !parts.is_empty() {
            let mut content = br#"{"parts":[],"role":"function"}"#.to_vec();
            gj::set_raw(&mut content, "parts", gj::join(&parts));
            items.push(content);
        }
    };
    contents.each(|_, value| {
        let parts = value.get("parts");
        let responses = collect_function_responses(&parts);
        if !responses.is_empty() {
            collected.extend(responses);
            while let Some(group) = pending.front()
                && collected.len() >= group.len()
            {
                let group = pending.pop_front().unwrap_or_default();
                let taken = collected.drain(..group.len()).collect();
                append(&mut items, taken, &group);
            }
            return true;
        }
        if !value.is_object() {
            return true;
        }
        items.push(value.raw.to_vec());
        if value.get("role").bytes().as_ref() == b"model" {
            let mut calls = vec![];
            parts.each(|_, part| {
                if part.get("functionCall").exists() {
                    calls.push(part.get("functionCall.name").bytes().into_owned());
                }
                true
            });
            if !calls.is_empty() {
                pending.push_back(calls);
            }
        }
        true
    });
    for group in pending {
        if collected.len() >= group.len() {
            let taken = collected.drain(..group.len()).collect();
            append(&mut items, taken, &group);
        }
    }
    let mut out = input.to_vec();
    gj::set_raw(&mut out, "request.contents", gj::join(&items));
    Some(out)
}

// ---------------------------------------------------------------------------------------
// Claude thinking signatures (SanitizeAntigravityClaudeGeminiRequestSignatures)

/// Where a part can carry a thought signature.
const SIGNATURE_PATHS: [&[&str]; 7] = [
    &["thoughtSignature"],
    &["thought_signature"],
    &["functionCall", "thoughtSignature"],
    &["functionCall", "thought_signature"],
    &["functionResponse", "thoughtSignature"],
    &["functionResponse", "thought_signature"],
    &["extra_content", "google", "thought_signature"],
];

type GoMap = std::collections::BTreeMap<String, GoValue>;

fn string_at(part: &GoMap, path: &[&str]) -> Option<String> {
    let (last, parents) = path.split_last()?;
    let mut current = part;
    for key in parents {
        match current.get(*key) {
            Some(GoValue::Object(next)) => current = next,
            _ => return None,
        }
    }
    match current.get(*last) {
        Some(GoValue::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn delete_at(part: &mut GoMap, path: &[&str]) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut current = part;
    for key in parents {
        match current.get_mut(*key) {
            Some(GoValue::Object(next)) => current = next,
            _ => return,
        }
    }
    current.remove(*last);
}

/// antigravityClaudeGeminiPartHasThoughtSignatureKeyInRaw: a signature key on any object
/// at any depth (which covers every fixed signature path too).
fn has_signature_key(value: &Res<'_>) -> bool {
    let mut found = false;
    if value.is_object() {
        value.each(|key, item| {
            found =
                matches!(key.bytes().as_ref(), b"thoughtSignature" | b"thought_signature") || has_signature_key(&item);
            !found
        });
    } else if value.is_array() {
        value.each(|_, item| {
            found = has_signature_key(&item);
            !found
        });
    }
    found
}

/// What happens to one part.
enum PartFate {
    Keep,
    Drop,
    /// The re-marshaled part, and whether it counts as a change. A thinking part whose
    /// signature is already normalized does not: its content keeps its raw bytes unless
    /// another part changes.
    Rewrite(GoValue, bool),
}

fn sanitize_part(part: &Res<'_>, model_turn: bool) -> PartFate {
    // A json.Decoder into map[string]any: anything but an object fails and is kept.
    let Some(GoValue::Object(mut map)) = GoValue::parse(&part.raw) else {
        return PartFate::Keep;
    };
    let signature = SIGNATURE_PATHS.iter().find_map(|p| string_at(&map, p));
    let has_key = signature.is_some() || has_signature_key(part);
    let strip = |mut map: GoMap| {
        for path in SIGNATURE_PATHS {
            delete_at(&mut map, path);
        }
        map
    };
    let response = map.contains_key("functionResponse") || map.contains_key("function_response");
    if response || !model_turn || map.get("thought") != Some(&GoValue::Bool(true)) {
        return if has_key {
            PartFate::Rewrite(GoValue::Object(strip(map)), true)
        } else {
            PartFate::Keep
        };
    }
    let Some(normalized) = compatible_antigravity_claude_thinking_signature(signature.as_deref().unwrap_or_default())
    else {
        return PartFate::Drop;
    };
    let text = match map.get("text") {
        Some(GoValue::String(t)) => t.as_str(),
        _ => "",
    };
    if trim_space(text.as_bytes()).is_empty() {
        return PartFate::Drop;
    }
    let changed = Some(&normalized) != signature.as_ref();
    map = strip(map);
    map.insert("thoughtSignature".into(), GoValue::String(normalized));
    PartFate::Rewrite(GoValue::Object(map), changed)
}

/// SanitizeAntigravityClaudeGeminiRequestSignatures: only model thinking parts with a
/// Claude-compatible signature and non-empty text keep (a normalized) signature; other
/// signature fields are removed and contents left without parts are dropped.
pub(crate) fn sanitize_claude_signatures(raw: Vec<u8>) -> Vec<u8> {
    let contents = gj::get(&raw, "request.contents");
    if !contents.is_array() {
        return raw;
    }
    let mut changed = false;
    let mut items = vec![];
    for content in contents.array() {
        let parts = content.get("parts");
        if !parts.is_array() {
            items.push(content.raw.to_vec());
            continue;
        }
        let model_turn = content.get("role").bytes().as_ref() == b"model";
        let mut content_changed = false;
        let mut kept = vec![];
        for part in parts.array() {
            match sanitize_part(&part, model_turn) {
                PartFate::Keep => kept.push(part.raw.to_vec()),
                PartFate::Drop => content_changed = true,
                PartFate::Rewrite(value, changed) => {
                    content_changed |= changed;
                    kept.push(value.marshal());
                }
            }
        }
        changed |= content_changed;
        if kept.is_empty() {
            changed = true;
            continue;
        }
        let mut content_json = content.raw.to_vec();
        if content_changed {
            gj::set_raw(&mut content_json, "parts", gj::join(&kept));
        }
        items.push(content_json);
    }
    if !changed {
        return raw;
    }
    let mut out = raw.clone();
    gj::set_raw(&mut out, "request.contents", gj::join(&items));
    out
}

// ---------------------------------------------------------------------------------------
// Responses

/// restoreUsageMetadata: the executor's `cpaUsageMetadata` back to `usageMetadata`.
pub(crate) fn restore_usage(mut chunk: Vec<u8>) -> Vec<u8> {
    let usage = gj::get(&chunk, "cpaUsageMetadata").into_owned();
    if usage.exists() {
        gj::set_raw(&mut chunk, "usageMetadata", &usage.raw);
        gj::delete(&mut chunk, "cpaUsageMetadata");
    }
    chunk
}

/// restoreGeminiFunctionNames: sanitized function names in `fields` of every candidate
/// part back to the declared names.
pub(crate) fn restore_names(mut chunk: Vec<u8>, original: &[u8], fields: &[&str]) -> Vec<u8> {
    let map = disambiguated_tool_name_map(original);
    if map.is_empty() {
        return chunk;
    }
    let candidates = gj::get(&chunk, "candidates").into_owned();
    for (ci, candidate) in candidates.array().iter().enumerate() {
        for (pi, part) in candidate.get("content.parts").array().iter().enumerate() {
            for field in fields {
                let name = part.get(&format!("{field}.name"));
                let current = name.bytes();
                if current.is_empty() {
                    continue;
                }
                let restored = restore_sanitized_tool_name(Some(&map), &current);
                if name.kind == Kind::String && restored == *current {
                    continue;
                }
                gj::set_str(
                    &mut chunk,
                    &format!("candidates.{ci}.content.parts.{pi}.{field}.name"),
                    &restored,
                );
            }
        }
    }
    chunk
}

/// ConvertAntigravityResponseToGeminiNonStream: the unwrapped response with restored
/// usage and names; every candidate without a finish reason gets `STOP`.
fn non_stream(original: &[u8], body: &[u8]) -> Vec<u8> {
    let response = gj::get(body, "response");
    let mut chunk = if response.exists() {
        restore_names(restore_usage(response.raw.to_vec()), original, &NAME_FIELDS)
    } else {
        restore_names(body.to_vec(), original, &NAME_FIELDS)
    };
    let candidates = gj::get(&chunk, "candidates").into_owned();
    if candidates.is_array() {
        for (i, candidate) in candidates.array().iter().enumerate() {
            if candidate.get("finishReason").bytes().is_empty() {
                gj::set_str(&mut chunk, &format!("candidates.{i}.finishReason"), "STOP");
            }
        }
    }
    chunk
}

pub fn go_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    Box::new(Stream {
        original: ctx.original_request.to_vec(),
        ..Stream::default()
    })
}

/// ConvertAntigravityResponseToGemini as the Antigravity executor runs it (`alt` is
/// always empty there): each chunk is the unwrapped `response`, and `[DONE]` closes a
/// stream that produced output but no finish reason with a synthetic terminal chunk.
// ponytail: the non-empty `alt` branch (JSON-array chunks) is unreachable from the
// Antigravity executor, which sets `alt` to "" for streams; it is not ported.
#[derive(Default)]
struct Stream {
    original: Vec<u8>,
    saw_response: bool,
    saw_finish_reason: bool,
    model_version: Vec<u8>,
    response_id: Vec<u8>,
    usage: Vec<u8>,
}

const USAGE_PATHS: [&str; 4] = [
    "response.usageMetadata",
    "response.cpaUsageMetadata",
    "usageMetadata",
    "cpaUsageMetadata",
];

/// hasAntigravityResponsePayload: candidates or non-empty usage, not a bare envelope.
pub(crate) fn has_response_payload(raw: &[u8]) -> bool {
    ["response.candidates", "candidates"].iter().any(|p| {
        let candidates = gj::get(raw, *p);
        candidates.is_array() && !candidates.array().is_empty()
    }) || USAGE_PATHS.iter().any(|p| {
        let usage = gj::get(raw, *p);
        usage.is_object() && !usage.map().is_empty()
    })
}

fn first_string(raw: &[u8], paths: [&str; 2]) -> Vec<u8> {
    paths
        .iter()
        .map(|p| gj::get(raw, *p).bytes().into_owned())
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

impl Stream {
    fn observe(&mut self, raw: &[u8]) {
        if !self.saw_response {
            self.saw_response = has_response_payload(raw);
        }
        if !self.saw_finish_reason {
            self.saw_finish_reason = ["response.candidates", "candidates"].iter().any(|p| {
                gj::get(raw, *p)
                    .array()
                    .iter()
                    .any(|c| !c.get("finishReason").bytes().is_empty())
            });
        }
        if self.model_version.is_empty() {
            self.model_version = first_string(raw, ["response.modelVersion", "modelVersion"]);
        }
        if self.response_id.is_empty() {
            self.response_id = first_string(raw, ["response.responseId", "responseId"]);
        }
        if let Some(usage) = USAGE_PATHS.iter().map(|p| gj::get(raw, *p)).find(Res::exists) {
            self.usage = usage.raw.to_vec();
        }
    }

    /// syntheticTerminalChunk, in upstream key order.
    fn terminal_chunk(&self) -> Vec<u8> {
        let mut chunk =
            br#"{"candidates":[{"content":{"role":"model","parts":[{"text":""}]},"finishReason":"STOP"}]}"#.to_vec();
        if !self.usage.is_empty() {
            gj::set_raw(&mut chunk, "usageMetadata", &self.usage);
        }
        if !self.model_version.is_empty() {
            gj::set_str(&mut chunk, "modelVersion", &self.model_version);
        }
        if !self.response_id.is_empty() {
            gj::set_str(&mut chunk, "responseId", &self.response_id);
        }
        chunk
    }
}

impl GoStream for Stream {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let raw = match line.strip_prefix(b"data:") {
            Some(rest) => trim_space(rest),
            None => line,
        };
        if raw == b"[DONE]" {
            if !self.saw_response || self.saw_finish_reason {
                return Ok(vec![]);
            }
            self.saw_finish_reason = true;
            return Ok(vec![self.terminal_chunk()]);
        }
        self.observe(raw);
        let response = gj::get(raw, "response");
        if !response.exists() {
            return Ok(vec![vec![]]);
        }
        let chunk = restore_names(restore_usage(response.raw.to_vec()), &self.original, &NAME_FIELDS);
        Ok(vec![chunk])
    }
}
