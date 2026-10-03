//! OpenAI Chat Completions request -> Codex (OpenAI Responses) request
//! (internal/translator/codex/openai/chat-completions/codex_openai_request.go).

use crate::{
    Registered, apply_patch, codex_chat_response as response,
    common::{go_lower, sanitize, trim_space},
};
use cpa_common::json::{self as gj, AnyValue, Kind, Res};
use std::collections::{HashMap, HashSet};

pub static PAIR: Registered = registered!(
    OpenAI -> Codex,
    request: |ctx, body| Ok(convert(ctx.model, body, ctx.stream)),
    non_stream: response::non_stream,
    go_stream: response::go_stream,
    token_count: None,
);

const NAME_LIMIT: usize = 64;

/// shortenNameIfNeeded: sanitized, then at most 64 bytes, keeping `mcp__` plus the last
/// `__` segment when possible.
pub(crate) fn shorten_name(name: &[u8]) -> Vec<u8> {
    let sanitized = sanitize(name);
    if sanitized.len() <= NAME_LIMIT {
        return sanitized;
    }
    if sanitized.starts_with(b"mcp__")
        && let Some(idx) = sanitized.windows(2).rposition(|w| w == b"__")
        && idx > 0
    {
        let mut candidate = [&b"mcp__"[..], &sanitized[idx + 2..]].concat();
        candidate.truncate(NAME_LIMIT);
        return candidate;
    }
    sanitized[..NAME_LIMIT].to_vec()
}

/// collectRequestToolNames: declared tools, the tool choice, then assistant tool calls.
pub(crate) fn request_tool_names(raw: &[u8]) -> Vec<Vec<u8>> {
    let mut names: Vec<Vec<u8>> = vec![];
    let mut add = |name: Vec<u8>| {
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    };
    let tools = gj::get(raw, "tools");
    if tools.is_array() {
        for tool in tools.array() {
            match tool.get("type").bytes().as_ref() {
                b"function" => add(tool.get("function.name").bytes().into_owned()),
                b"custom" => add(tool.get("name").bytes().into_owned()),
                _ => {}
            }
        }
    }
    let choice = gj::get(raw, "tool_choice");
    if choice.is_object() {
        match choice.get("type").bytes().as_ref() {
            b"function" => {
                let mut name = choice.get("function.name").bytes().into_owned();
                if name.is_empty() {
                    name = choice.get("name").bytes().into_owned();
                }
                add(name);
            }
            b"custom" => add(choice.get("name").bytes().into_owned()),
            _ => {}
        }
    }
    let messages = gj::get(raw, "messages");
    if messages.is_array() {
        for message in messages.array() {
            if message.get("role").bytes().as_ref() != b"assistant" {
                continue;
            }
            let calls = message.get("tool_calls");
            if calls.is_array() {
                for call in calls.array() {
                    let name = call.get("function.name").bytes().into_owned();
                    if name.is_empty() {
                        add(call.get("custom.name").bytes().into_owned());
                    } else {
                        add(name);
                    }
                }
            }
        }
    }
    names
}

/// buildShortNameMap: unique short names, suffixing `_1`, `_2`, ... on collisions.
pub(crate) fn short_name_map(names: &[Vec<u8>]) -> HashMap<Vec<u8>, Vec<u8>> {
    let mut used: HashSet<Vec<u8>> = HashSet::new();
    let mut map = HashMap::new();
    for name in names {
        let candidate = shorten_name(name);
        let unique = if used.contains(&candidate) {
            (1..)
                .map(|i| {
                    let suffix = format!("_{i}");
                    let allowed = NAME_LIMIT.saturating_sub(suffix.len());
                    let mut tmp = candidate[..candidate.len().min(allowed)].to_vec();
                    tmp.extend_from_slice(suffix.as_bytes());
                    tmp
                })
                .find(|tmp| !used.contains(tmp))
                .unwrap()
        } else {
            candidate
        };
        used.insert(unique.clone());
        map.insert(name.clone(), unique);
    }
    map
}

/// normalizeCodexServiceTier.
pub(crate) fn service_tier(r: &Res<'_>) -> &'static str {
    if r.kind != Kind::String {
        return "";
    }
    match go_lower(trim_space(&r.s)).as_slice() {
        b"fast" | b"priority" => "priority",
        b"ultrafast" => "ultrafast",
        _ => "",
    }
}

struct PendingCall {
    call_id: Vec<u8>,
    source_id: Vec<u8>,
    custom: bool,
    consumed: bool,
}

fn convert(model: &str, raw: &[u8], stream: bool) -> Vec<u8> {
    let root = gj::parse(raw);
    let tools = root.get("tools");
    let tool_list = tools.array();
    let mut out = br#"{"instructions":""}"#.to_vec();
    gj::set_bool(&mut out, "stream", stream);
    let effort = gj::get(raw, "reasoning_effort");
    if effort.exists() {
        AnyValue::from_res(&effort).set(&mut out, "reasoning.effort");
    } else {
        gj::set_str(&mut out, "reasoning.effort", "medium");
    }
    let tier = service_tier(&root.get("service_tier"));
    if !tier.is_empty() {
        gj::set_str(&mut out, "service_tier", tier);
    }
    gj::set_bool(&mut out, "parallel_tool_calls", true);
    gj::set_strs(&mut out, "include", &["reasoning.encrypted_content"]);
    gj::set_str(&mut out, "model", model);

    let mut custom_names: HashSet<Vec<u8>> = HashSet::new();
    if tools.is_array() && !tool_list.is_empty() {
        let mut function_names: HashSet<Vec<u8>> = HashSet::new();
        for tool in &tool_list {
            match tool.get("type").bytes().as_ref() {
                b"function" => {
                    function_names.insert(tool.get("function.name").bytes().into_owned());
                }
                b"custom" => {
                    custom_names.insert(tool.get("name").bytes().into_owned());
                }
                _ => {}
            }
        }
        custom_names.retain(|n| !function_names.contains(n));
    }
    let short_names = short_name_map(&request_tool_names(raw));
    let short = |name: &[u8]| short_names.get(name).cloned().unwrap_or_else(|| shorten_name(name));

    // (custom, name, input)
    let resolve = |call: &Res<'_>| -> Option<(bool, Vec<u8>, Vec<u8>)> {
        match call.get("type").bytes().as_ref() {
            b"custom" => Some((
                true,
                call.get("custom.name").bytes().into_owned(),
                call.get("custom.input").bytes().into_owned(),
            )),
            b"function" => {
                let name = call.get("function.name").bytes().into_owned();
                let custom = custom_names.contains(&name);
                let mut input = call.get("function.arguments").bytes().into_owned();
                if custom
                    && trim_space(&name) == b"apply_patch"
                    && let Some(unwrapped) = apply_patch::unwrap_input(&input)
                {
                    input = unwrapped.into_bytes();
                }
                Some((custom, name, input))
            }
            _ => None,
        }
    };

    gj::set_raw(&mut out, "input", b"[]");
    let mut items: Vec<Vec<u8>> = vec![];
    let mut pending: Vec<PendingCall> = vec![];
    let mut ambiguous: HashSet<Vec<u8>> = HashSet::new();
    let messages = gj::get(raw, "messages");
    if messages.is_array() {
        for (i, m) in messages.array().iter().enumerate() {
            let role = m.get("role").bytes().into_owned();
            if role == b"tool" {
                let tool_call_id = m.get("tool_call_id").bytes().into_owned();
                if !tool_call_id.is_empty() && ambiguous.contains(&tool_call_id) {
                    continue;
                }
                let Some(call) = pending.iter_mut().find(|c| {
                    !c.consumed && (tool_call_id.is_empty() || c.source_id == tool_call_id || c.call_id == tool_call_id)
                }) else {
                    continue;
                };
                call.consumed = true;
                let mut output = b"{}".to_vec();
                let kind = if call.custom {
                    "custom_tool_call_output"
                } else {
                    "function_call_output"
                };
                gj::set_str(&mut output, "type", kind);
                gj::set_str(&mut output, "call_id", &call.call_id);
                set_tool_output(&mut output, &m.get("content"));
                items.push(output);
                continue;
            }
            pending.clear();
            ambiguous.clear();
            let mut msg = b"{}".to_vec();
            gj::set_str(&mut msg, "type", "message");
            gj::set_str(
                &mut msg,
                "role",
                if role == b"system" { &b"developer"[..] } else { &role },
            );
            let assistant = role == b"assistant";
            let text_type = if assistant { "output_text" } else { "input_text" };
            let user = role == b"user";
            let mut parts = vec![];
            let content = m.get("content");
            if content.kind == Kind::String && !content.s.is_empty() {
                let mut part = b"{}".to_vec();
                gj::set_str(&mut part, "type", text_type);
                gj::set_str(&mut part, "text", &content.s);
                parts.push(part);
            } else if content.is_array() {
                for it in content.array() {
                    match it.get("type").bytes().as_ref() {
                        b"text" => {
                            let mut part = b"{}".to_vec();
                            gj::set_str(&mut part, "type", text_type);
                            gj::set_str(&mut part, "text", it.get("text").bytes());
                            parts.push(part);
                        }
                        b"image_url" if user => {
                            let mut part = b"{}".to_vec();
                            gj::set_str(&mut part, "type", "input_image");
                            let url = it.get("image_url.url");
                            if url.exists() {
                                gj::set_str(&mut part, "image_url", url.bytes());
                            }
                            parts.push(part);
                        }
                        b"file" if user => {
                            let data = it.get("file.file_data").bytes();
                            let filename = it.get("file.filename").bytes();
                            if !data.is_empty() {
                                let mut part = b"{}".to_vec();
                                gj::set_str(&mut part, "type", "input_file");
                                gj::set_str(&mut part, "file_data", &data);
                                if !filename.is_empty() {
                                    gj::set_str(&mut part, "filename", &filename);
                                }
                                parts.push(part);
                            }
                        }
                        b"input_audio" if user => {
                            let data = it.get("input_audio.data").bytes();
                            let format = it.get("input_audio.format").bytes();
                            if !data.is_empty() {
                                let mut part = b"{}".to_vec();
                                gj::set_str(&mut part, "type", "input_audio");
                                gj::set_str(&mut part, "data", &data);
                                if !format.is_empty() {
                                    gj::set_str(&mut part, "format", &format);
                                }
                                parts.push(part);
                            }
                        }
                        _ => {}
                    }
                }
            }
            if !assistant || !parts.is_empty() {
                gj::set_raw(&mut msg, "content", gj::join(&parts));
                items.push(msg);
            }
            if !assistant {
                continue;
            }
            let calls = m.get("tool_calls");
            if !calls.is_array() {
                continue;
            }
            let calls = calls.array();
            let mut counts: HashMap<Vec<u8>, usize> = HashMap::new();
            let mut used: HashSet<Vec<u8>> = HashSet::new();
            for call in &calls {
                let id = call.get("id").bytes().into_owned();
                if resolve(call).is_some() && !id.is_empty() {
                    *counts.entry(id.clone()).or_default() += 1;
                    used.insert(id);
                }
            }
            ambiguous.extend(counts.into_iter().filter(|(_, n)| *n > 1).map(|(id, _)| id));
            for (j, call) in calls.iter().enumerate() {
                let Some((custom, name, input)) = resolve(call) else {
                    continue;
                };
                let source_id = call.get("id").bytes().into_owned();
                if !source_id.is_empty() && ambiguous.contains(&source_id) {
                    continue;
                }
                let mut call_id = source_id.clone();
                if call_id.is_empty() {
                    let base = format!("call_missing_{i}_{j}").into_bytes();
                    call_id = base.clone();
                    let mut suffix = 1;
                    while used.contains(&call_id) {
                        call_id = [&base[..], format!("_{suffix}").as_bytes()].concat();
                        suffix += 1;
                    }
                    used.insert(call_id.clone());
                }
                pending.push(PendingCall {
                    call_id: call_id.clone(),
                    source_id,
                    custom,
                    consumed: false,
                });
                let mut item = b"{}".to_vec();
                gj::set_str(
                    &mut item,
                    "type",
                    if custom { "custom_tool_call" } else { "function_call" },
                );
                gj::set_str(&mut item, "call_id", &call_id);
                gj::set_str(&mut item, "name", short(&name));
                gj::set_str(&mut item, if custom { "input" } else { "arguments" }, &input);
                items.push(item);
            }
        }
    }
    gj::set_items(&mut out, "input", &items);

    let format = gj::get(raw, "response_format");
    let text = gj::get(raw, "text");
    if format.exists() {
        if !gj::get(&out, "text").exists() {
            gj::set_raw(&mut out, "text", b"{}");
        }
        match format.get("type").bytes().as_ref() {
            b"text" => {
                gj::set_str(&mut out, "text.format.type", "text");
            }
            b"json_schema" => {
                let schema = format.get("json_schema");
                if schema.exists() {
                    gj::set_str(&mut out, "text.format.type", "json_schema");
                    for (from, to) in [("name", "text.format.name"), ("strict", "text.format.strict")] {
                        let v = schema.get(from);
                        if v.exists() {
                            AnyValue::from_res(&v).set(&mut out, to);
                        }
                    }
                    let s = schema.get("schema");
                    if s.exists() {
                        gj::set_raw(&mut out, "text.format.schema", &s.raw);
                    }
                }
            }
            _ => {}
        }
        let verbosity = text.get("verbosity");
        if text.exists() && verbosity.exists() {
            AnyValue::from_res(&verbosity).set(&mut out, "text.verbosity");
        }
    } else if text.exists() {
        let verbosity = text.get("verbosity");
        if verbosity.exists() {
            if !gj::get(&out, "text").exists() {
                gj::set_raw(&mut out, "text", b"{}");
            }
            AnyValue::from_res(&verbosity).set(&mut out, "text.verbosity");
        }
    }

    if tools.is_array() && !tool_list.is_empty() {
        let mut tool_items = vec![];
        for t in &tool_list {
            let kind = t.get("type").bytes().into_owned();
            if kind == b"custom" {
                let mut item = t.raw.to_vec();
                gj::set_str(&mut item, "name", short(&t.get("name").bytes()));
                tool_items.push(item);
                continue;
            }
            if !kind.is_empty() && kind != b"function" && t.is_object() {
                tool_items.push(t.raw.to_vec());
                continue;
            }
            if kind != b"function" {
                continue;
            }
            let mut item = b"{}".to_vec();
            gj::set_str(&mut item, "type", "function");
            let f = t.get("function");
            if f.exists() {
                let name = f.get("name");
                if name.exists() {
                    gj::set_str(&mut item, "name", short(&name.bytes()));
                }
                let description = f.get("description");
                if description.exists() {
                    AnyValue::from_res(&description).set(&mut item, "description");
                }
                let parameters = f.get("parameters");
                if parameters.exists() {
                    gj::set_raw(&mut item, "parameters", &parameters.raw);
                }
                let strict = f.get("strict");
                if strict.exists() {
                    AnyValue::from_res(&strict).set(&mut item, "strict");
                } else {
                    gj::set_bool(&mut item, "strict", false);
                }
            }
            tool_items.push(item);
        }
        gj::set_raw(&mut out, "tools", gj::join(&tool_items));
    }

    let choice = gj::get(raw, "tool_choice");
    if choice.kind == Kind::String {
        gj::set_str(&mut out, "tool_choice", &choice.s);
    } else if choice.is_object() {
        let mut kind = choice.get("type").bytes().into_owned();
        if kind == b"function" || kind == b"custom" {
            let mut name = choice.get("name").bytes().into_owned();
            if kind == b"function" {
                name = choice.get("function.name").bytes().into_owned();
                if custom_names.contains(&name) {
                    kind = b"custom".to_vec();
                }
            }
            if !name.is_empty() {
                name = short(&name);
            }
            let mut tc = b"{}".to_vec();
            gj::set_str(&mut tc, "type", &kind);
            if !name.is_empty() {
                gj::set_str(&mut tc, "name", &name);
            }
            gj::set_raw(&mut out, "tool_choice", tc);
        } else if !kind.is_empty() {
            gj::set_raw(&mut out, "tool_choice", &choice.raw);
        }
    }
    gj::set_bool(&mut out, "store", false);
    out
}

/// setToolCallOutputContent.
fn set_tool_output(output: &mut Vec<u8>, content: &Res<'_>) {
    if content.kind == Kind::String {
        let structured = gj::parse(&content.s).into_owned();
        if has_image_part(&structured) {
            return set_tool_output(output, &structured);
        }
        gj::set_str(output, "output", &content.s);
    } else if content.is_array() {
        let parts: Vec<Vec<u8>> = content.array().iter().map(output_part).collect();
        gj::set_raw(output, "output", gj::join(&parts));
    } else {
        let fallback = if content.raw.is_empty() {
            content.bytes().into_owned()
        } else {
            content.raw.to_vec()
        };
        gj::set_str(output, "output", fallback);
    }
}

fn input_text(text: &[u8]) -> Vec<u8> {
    let mut part = b"{}".to_vec();
    gj::set_str(&mut part, "type", "input_text");
    gj::set_str(&mut part, "text", text);
    part
}

fn fallback_part(item: &Res<'_>) -> Vec<u8> {
    let text = if item.raw.is_empty() {
        item.bytes().into_owned()
    } else {
        item.raw.to_vec()
    };
    input_text(&text)
}

/// toolOutputContentPart.
fn output_part(item: &Res<'_>) -> Vec<u8> {
    let kind = item.get("type").bytes().into_owned();
    match kind.as_slice() {
        b"text" | b"input_text" | b"output_text" => input_text(&item.get("text").bytes()),
        b"image_url" | b"input_image" => {
            let input = kind == b"input_image";
            let (url, file_id, detail) = if input {
                (item.get("image_url"), item.get("file_id"), item.get("detail"))
            } else {
                (
                    item.get("image_url.url"),
                    item.get("image_url.file_id"),
                    item.get("image_url.detail"),
                )
            };
            let (url, file_id, detail) = (url.bytes(), file_id.bytes(), detail.bytes());
            if url.is_empty() && file_id.is_empty() {
                return fallback_part(item);
            }
            let mut part = b"{}".to_vec();
            gj::set_str(&mut part, "type", "input_image");
            if !url.is_empty() {
                gj::set_str(&mut part, "image_url", &url);
            }
            if !file_id.is_empty() {
                gj::set_str(&mut part, "file_id", &file_id);
            }
            if !detail.is_empty() {
                gj::set_str(&mut part, "detail", &detail);
            }
            part
        }
        b"file" => {
            let (id, data, url) = (
                item.get("file.file_id").bytes(),
                item.get("file.file_data").bytes(),
                item.get("file.file_url").bytes(),
            );
            if id.is_empty() && data.is_empty() && url.is_empty() {
                return fallback_part(item);
            }
            let mut part = b"{}".to_vec();
            gj::set_str(&mut part, "type", "input_file");
            for (key, value) in [("file_id", &id), ("file_data", &data), ("file_url", &url)] {
                if !value.is_empty() {
                    gj::set_str(&mut part, key, value.as_ref());
                }
            }
            let filename = item.get("file.filename").bytes();
            if !filename.is_empty() {
                gj::set_str(&mut part, "filename", &filename);
            }
            part
        }
        _ => fallback_part(item),
    }
}

/// hasToolOutputImagePart.
fn has_image_part(content: &Res<'_>) -> bool {
    content.is_array()
        && content
            .array()
            .iter()
            .any(|item| match item.get("type").bytes().as_ref() {
                b"image_url" => {
                    !item.get("image_url.url").bytes().is_empty() || !item.get("image_url.file_id").bytes().is_empty()
                }
                b"input_image" => !item.get("image_url").bytes().is_empty() || !item.get("file_id").bytes().is_empty(),
                _ => false,
            })
}
