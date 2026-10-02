//! Gemini -> Gemini (internal/translator/gemini/gemini): a request normalizer, passthrough
//! responses and Gemini's token-count shape.

use crate::{Error, Registered, ResponseCtx, common::trim_space, stream};
use cpa_common::json::{self as gj, Res};

pub static PAIR: Registered = registered!(
    Gemini -> Gemini,
    request: |_, body| Ok(convert(body)),
    non_stream: |_, body| Ok(body.to_vec()),
    go_stream: go_stream,
    token_count: Some(token_count),
);

/// common.GeminiTokenCountJSON (Go's GeminiTokenCount).
pub(crate) fn token_count(count: i64) -> Vec<u8> {
    format!(r#"{{"totalTokens":{count},"promptTokensDetails":[{{"modality":"TEXT","tokenCount":{count}}}]}}"#)
        .into_bytes()
}

/// gemini/common.DefaultSafetySettings as json.Marshal writes it.
const DEFAULT_SAFETY_SETTINGS: &[u8] = br#"[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"},{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"OFF"},{"category":"HARM_CATEGORY_SEXUALLY_EXPLICIT","threshold":"OFF"},{"category":"HARM_CATEGORY_DANGEROUS_CONTENT","threshold":"OFF"},{"category":"HARM_CATEGORY_CIVIC_INTEGRITY","threshold":"BLOCK_NONE"}]"#;

/// gemini/common.AttachDefaultSafetySettings: adds the defaults when `path` is absent.
pub(crate) fn attach_default_safety_settings(mut raw: Vec<u8>, path: &str) -> Vec<u8> {
    if !gj::get(&raw, path).exists() {
        gj::set_raw(&mut raw, path, DEFAULT_SAFETY_SETTINGS);
    }
    raw
}

/// common.ContentHasGeminiFunctionResponse.
pub(crate) fn has_function_response(content: &[u8]) -> bool {
    let mut found = false;
    gj::get(content, "parts").each(|_, part| {
        found = part.get("functionResponse").exists() || part.get("function_response").exists();
        !found
    });
    found
}

fn next_role(previous: &[u8]) -> &'static [u8] {
    if previous.is_empty() || previous == b"model" {
        b"user"
    } else {
        b"model"
    }
}

/// The role a content keeps or is given: valid roles stay, function responses are user
/// turns, anything else alternates from the previous role.
fn fixed_role(content: &Res<'_>, previous: &[u8]) -> Option<Vec<u8>> {
    let role = content.get("role").bytes();
    if role.as_ref() == b"user" || role.as_ref() == b"model" {
        return None;
    }
    Some(if has_function_response(&content.raw) {
        b"user".to_vec()
    } else {
        next_role(previous).to_vec()
    })
}

/// ConvertGeminiRequestToGemini: renames camelCase tool declarations, fixes missing or
/// invalid content roles, sanitizes thought signatures, renames `responseSchema`,
/// backfills empty function-response names and attaches default safety settings.
pub(crate) fn convert(input: &[u8]) -> Vec<u8> {
    let contents = if input.is_empty() {
        Res::default()
    } else {
        gj::get(input, "contents")
    };
    if !contents.exists() {
        return attach_default_safety_settings(input.to_vec(), "safetySettings");
    }
    let mut raw = input.to_vec();
    let tools = gj::get(input, "tools");
    if tools.is_array() {
        let mut items = vec![];
        let mut changed = false;
        tools.each(|_, tool_res| {
            let mut tool = tool_res.raw.to_vec();
            let declarations = tool_res.get("functionDeclarations");
            if declarations.exists() {
                gj::set_raw(&mut tool, "function_declarations", &declarations.raw);
                gj::delete(&mut tool, "functionDeclarations");
                changed = true;
            }
            let declarations = gj::get(&tool, "function_declarations").into_owned();
            if declarations.is_array() {
                let mut decls = vec![];
                let mut decls_changed = false;
                declarations.each(|_, decl_res| {
                    let mut decl = decl_res.raw.to_vec();
                    let parameters = decl_res.get("parameters");
                    if parameters.exists() {
                        gj::set_raw(&mut decl, "parametersJsonSchema", &parameters.raw);
                        gj::delete(&mut decl, "parameters");
                        decls_changed = true;
                    }
                    decls.push(decl);
                    true
                });
                if decls_changed {
                    gj::set_raw(&mut tool, "function_declarations", gj::join(&decls));
                    changed = true;
                }
            }
            items.push(tool);
            true
        });
        if changed {
            gj::set_raw(&mut raw, "tools", gj::join(&items));
        }
    }

    let mut out = raw.clone();
    let mut previous: Vec<u8> = vec![];
    if contents.is_array() {
        let mut changed = false;
        contents.each(|_, content| {
            let role = fixed_role(&content, &previous);
            changed |= role.is_some();
            previous = role.unwrap_or_else(|| content.get("role").bytes().into_owned());
            true
        });
        if changed {
            previous.clear();
            let mut items = vec![];
            contents.each(|_, content| {
                let mut item = content.raw.to_vec();
                let role = match fixed_role(&content, &previous) {
                    Some(role) => {
                        gj::set_str(&mut item, "role", &role);
                        role
                    }
                    None => content.get("role").bytes().into_owned(),
                };
                previous = role;
                items.push(item);
                true
            });
            gj::set_raw(&mut out, "contents", gj::join(&items));
        }
    } else {
        let mut index = 0;
        contents.each(|_, content| {
            let role = match fixed_role(&content, &previous) {
                Some(role) => {
                    gj::set_str(&mut out, &format!("contents.{index}.role"), &role);
                    role
                }
                None => content.get("role").bytes().into_owned(),
            };
            previous = role;
            index += 1;
            true
        });
    }

    out = cpa_common::signature::sanitize_gemini_request_thought_signatures(&out, "contents");

    if gj::get(&raw, "generationConfig.responseSchema").exists() {
        // util.RenameKey; Go keeps its empty result when an edit fails.
        let value = gj::get(&out, "generationConfig.responseSchema").into_owned();
        out = if value.exists() {
            gj::try_set_raw(&out, "generationConfig.responseJsonSchema", &value.raw)
                .and_then(|next| gj::try_delete(&next, "generationConfig.responseSchema"))
                .unwrap_or_default()
        } else {
            vec![]
        };
    }

    out = backfill_function_response_names(out);
    attach_default_safety_settings(out, "safetySettings")
}

/// Function-call names of a model turn, in order.
fn call_names(content: &Res<'_>) -> Vec<Vec<u8>> {
    let mut names = vec![];
    content.get("parts").each(|_, part| {
        if part.get("functionCall").exists() {
            names.push(part.get("functionCall.name").bytes().into_owned());
        }
        true
    });
    names
}

fn blank_response_name(part: &Res<'_>) -> bool {
    trim_space(&part.get("functionResponse.name").bytes()).is_empty()
}

/// backfillEmptyFunctionResponseNames: an empty functionResponse.name takes the name of
/// the matching call in the preceding model turn.
fn backfill_function_response_names(data: Vec<u8>) -> Vec<u8> {
    let contents = gj::get(&data, "contents");
    if !contents.exists() {
        return data;
    }
    let mut can_batch = contents.is_array();
    if can_batch {
        contents.each(|_, content| {
            let parts = content.get("parts");
            can_batch = !(parts.exists() && !parts.is_array());
            can_batch
        });
    }
    if !can_batch {
        return backfill_legacy(&data, &contents);
    }

    // geminiFunctionResponseNamesNeedBackfill.
    let mut pending: Vec<Vec<u8>> = vec![];
    let mut needed = false;
    contents.each(|_, content| {
        if content.get("role").bytes().as_ref() == b"model" {
            pending = call_names(&content);
            return true;
        }
        if pending.is_empty() {
            return true;
        }
        let mut index = 0;
        content.get("parts").each(|_, part| {
            if part.get("functionResponse").exists() {
                if blank_response_name(&part) && index < pending.len() {
                    needed = true;
                    return false;
                }
                index += 1;
            }
            true
        });
        pending.clear();
        !needed
    });
    if !needed {
        return data;
    }

    let mut changed = false;
    let mut items = vec![];
    pending.clear();
    contents.each(|_, content| {
        let mut raw = content.raw.to_vec();
        if content.get("role").bytes().as_ref() == b"model" {
            pending = call_names(&content);
            items.push(raw);
            return true;
        }
        if !pending.is_empty() {
            let mut index = 0;
            let mut parts_changed = false;
            let mut parts = vec![];
            content.get("parts").each(|_, part| {
                let mut part_raw = part.raw.to_vec();
                if part.get("functionResponse").exists() {
                    if blank_response_name(&part) && index < pending.len() {
                        gj::set_str(&mut part_raw, "functionResponse.name", &pending[index]);
                        parts_changed = true;
                    }
                    index += 1;
                }
                parts.push(part_raw);
                true
            });
            if parts_changed {
                gj::set_raw(&mut raw, "parts", gj::join(&parts));
                changed = true;
            }
            pending.clear();
        }
        items.push(raw);
        true
    });
    if !changed {
        return data;
    }
    gj::try_set_raw(&data, "contents", gj::join(&items)).unwrap_or(data)
}

fn backfill_legacy(data: &[u8], contents: &Res<'_>) -> Vec<u8> {
    let mut out = data.to_vec();
    let mut pending: Vec<Vec<u8>> = vec![];
    contents.each(|content_index, content| {
        if content.get("role").bytes().as_ref() == b"model" {
            pending = call_names(&content);
            return true;
        }
        if !pending.is_empty() {
            let mut index = 0;
            content.get("parts").each(|part_index, part| {
                if part.get("functionResponse").exists() {
                    if blank_response_name(&part) && index < pending.len() {
                        let path = format!(
                            "contents.{}.parts.{}.functionResponse.name",
                            content_index.int(),
                            part_index.int()
                        );
                        gj::set_str(&mut out, &path, &pending[index]);
                    }
                    index += 1;
                }
                true
            });
            pending.clear();
        }
        true
    });
    out
}

pub fn go_stream(_: &ResponseCtx<'_>) -> Box<dyn stream::GoStream> {
    Box::new(Passthrough)
}

/// PassthroughGeminiResponseStream: a `data:` payload is trimmed, `[DONE]` is dropped,
/// anything else is forwarded as is.
struct Passthrough;

impl stream::GoStream for Passthrough {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let payload = match line.strip_prefix(b"data:") {
            Some(rest) => trim_space(rest),
            None => line,
        };
        if payload == b"[DONE]" {
            return Ok(vec![]);
        }
        Ok(vec![payload.to_vec()])
    }
}

// ---------------------------------------------------------------------------------------
// Shared Gemini content helpers (translator/common/gemini.go, util/claude_tool_result.go)

/// common.ReorderGeminiUserParts: when text follows a function response, text parts move
/// ahead of the other parts.
pub(crate) fn reorder_user_parts(parts: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let is_response = |p: &[u8]| gj::get(p, "functionResponse").exists() || gj::get(p, "function_response").exists();
    let mut seen_response = false;
    let mut trailing_text = false;
    for p in &parts {
        if is_response(p) {
            seen_response = true;
        } else if seen_response && gj::get(p, "text").exists() {
            trailing_text = true;
            break;
        }
    }
    if !seen_response || !trailing_text {
        return parts;
    }
    let (mut text, other): (Vec<_>, Vec<_>) = parts.into_iter().partition(|p| gj::get(p, "text").exists());
    text.extend(other);
    text
}

/// common.MergeAdjacentGeminiContents: drops contents without parts and merges
/// consecutive user turns (reordering the merged parts).
pub(crate) fn merge_adjacent_contents(contents: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    if contents.len() <= 1 {
        return contents;
    }
    let mut merged: Vec<Vec<u8>> = vec![];
    for content in contents {
        if content.is_empty() {
            continue;
        }
        let role = gj::get(&content, "role").bytes().into_owned();
        let parts = gj::get(&content, "parts");
        if !parts.is_array() || parts.array().is_empty() {
            continue;
        }
        if let Some(last) = merged.last_mut()
            && role == b"user"
            && gj::get(last, "role").bytes().as_ref() == b"user"
        {
            let mut combined: Vec<Vec<u8>> = gj::get(last, "parts").array().iter().map(|p| p.raw.to_vec()).collect();
            combined.extend(parts.array().iter().map(|p| p.raw.to_vec()));
            let combined = reorder_user_parts(combined);
            if let Ok(updated) = gj::try_set_raw(last, "parts", gj::join(&combined)) {
                *last = updated;
                continue;
            }
        }
        merged.push(content);
    }
    merged
}

/// common.ContainsJSONRef: any object key `$ref` holding a string, at any depth.
pub(crate) fn contains_json_ref(value: &Res<'_>) -> bool {
    if !value.is_object() && !value.is_array() {
        return false;
    }
    let object = value.is_object();
    let mut found = false;
    value.each(|key, child| {
        found = (object && key.bytes().as_ref() == b"$ref" && child.kind == cpa_common::json::Kind::String)
            || contains_json_ref(&child);
        !found
    });
    found
}

/// common.SetGeminiFunctionResponseResult: raw JSON, or a string when it holds a
/// `$ref` (Gemini rejects those inside responses).
pub(crate) fn set_function_response_result(part: &mut Vec<u8>, path: &str, result: &Res<'_>) {
    if !result.exists() {
        gj::set_str(part, path, "");
    } else if contains_json_ref(result) {
        let target = if path.ends_with("response") {
            format!("{path}.result")
        } else {
            path.to_owned()
        };
        gj::set_str(part, &target, &result.raw);
    } else {
        gj::set_raw(part, path, &result.raw);
    }
}

/// common.SetGeminiFunctionResponseRaw.
pub(crate) fn set_function_response_raw(part: &mut Vec<u8>, path: &str, raw: &[u8]) {
    let trimmed = trim_space(raw);
    if trimmed.is_empty() {
        gj::set_str(part, path, "");
        return;
    }
    set_function_response_result(part, path, &gj::parse(trimmed));
}

/// util.ClaudeToolResult.
pub(crate) struct ClaudeToolResult {
    pub result: Vec<u8>,
    pub raw: bool,
    /// (MIME type, base64 data) of base64 image blocks.
    pub images: Vec<(Vec<u8>, Vec<u8>)>,
}

fn base64_image(block: &Res<'_>) -> Option<Option<(Vec<u8>, Vec<u8>)>> {
    if block.get("type").bytes().as_ref() != b"image" || block.get("source.type").bytes().as_ref() != b"base64" {
        return None;
    }
    let data = block.get("source.data").bytes().into_owned();
    Some((!data.is_empty()).then(|| (block.get("source.media_type").bytes().into_owned(), data)))
}

/// util.ConvertClaudeToolResultContent: strings stay strings, one non-image block is its
/// raw JSON, several become a raw array, and base64 images are split out.
pub(crate) fn claude_tool_result(content: &Res<'_>) -> ClaudeToolResult {
    let empty = || ClaudeToolResult {
        result: vec![],
        raw: false,
        images: vec![],
    };
    if content.kind == cpa_common::json::Kind::String {
        return ClaudeToolResult {
            result: content.s.to_vec(),
            ..empty()
        };
    }
    if content.is_array() {
        let mut images = vec![];
        let mut count = 0;
        let mut last = vec![];
        let mut filtered = b"[]".to_vec();
        content.each(|_, block| {
            if let Some(image) = base64_image(&block) {
                images.extend(image);
                return true;
            }
            count += 1;
            last = block.raw.to_vec();
            gj::set_raw(&mut filtered, "-1", &block.raw);
            true
        });
        return match count {
            0 => ClaudeToolResult { images, ..empty() },
            1 => ClaudeToolResult {
                result: last,
                raw: true,
                images,
            },
            _ => ClaudeToolResult {
                result: filtered,
                raw: true,
                images,
            },
        };
    }
    if content.is_object() {
        return match base64_image(content) {
            Some(image) => ClaudeToolResult {
                images: image.into_iter().collect(),
                ..empty()
            },
            None => ClaudeToolResult {
                result: content.raw.to_vec(),
                raw: true,
                images: vec![],
            },
        };
    }
    if !content.raw.is_empty() {
        return ClaudeToolResult {
            result: content.raw.to_vec(),
            raw: true,
            images: vec![],
        };
    }
    empty()
}
