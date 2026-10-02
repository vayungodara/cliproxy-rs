//! Gemini -> Gemini (internal/translator/gemini/gemini): a request normalizer, passthrough
//! responses and Gemini's token-count shape.

use crate::{Error, Pair, Registered, ResponseCtx, common::trim_space, stream};
use cpa_common::json::{self as gj, Res};
use cpa_core::format::Format;

pub static PAIR: Registered = Registered {
    pair: Pair {
        request: |_, body| Ok(convert(body)),
        non_stream: |_, body| Ok(body.to_vec()),
        stream: |ctx| stream::framed(Format::Gemini, Format::Gemini, go_stream(ctx)),
        count_tokens: None,
    },
    token_count: Some(token_count),
    go_stream,
};

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
