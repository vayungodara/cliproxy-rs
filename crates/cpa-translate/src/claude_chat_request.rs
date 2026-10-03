//! OpenAI Chat Completions request -> Claude Messages request
//! (internal/translator/claude/openai/chat-completions/claude_openai_request.go).

use crate::{
    Error, Registered, RequestCtx, claude_chat_response,
    common::{self, trim_space},
    thinking,
};
use cpa_common::json::{self as gj, Kind, Res};
use std::collections::{HashMap, HashSet};

pub static PAIR: Registered = registered!(
    OpenAI -> Claude,
    request: request,
    non_stream: claude_chat_response::non_stream,
    go_stream: claude_chat_response::go_stream,
    token_count: None,
);

fn request(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    Ok(convert(ctx.model, body, ctx.stream, false))
}

/// Compatibility endpoints may accept unsigned assistant thinking history
/// (ConvertOpenAIRequestToClaudeWithCompat). Native Claude uses the registered pair.
pub fn request_with_compat(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    Ok(convert(ctx.model, body, ctx.stream, true))
}

fn thinking_config(out: &mut Vec<u8>, root: &Res<'_>, model: &str) {
    apply_effort(out, &root.get("reasoning_effort"), model);
}

/// The reasoning-effort mapping shared by the Chat and Responses converters: adaptive
/// thinking with an output effort when the model has levels, a budget otherwise.
pub(crate) fn apply_effort(out: &mut Vec<u8>, v: &Res<'_>, model: &str) {
    if !v.exists() {
        return;
    }
    let effort = v.str().trim().to_lowercase();
    if effort.is_empty() {
        return;
    }
    let levels = thinking::lookup_model_info(model, "claude")
        .and_then(|m| m.thinking)
        .map(|t| t.levels)
        .unwrap_or_default();
    if !levels.is_empty() {
        let supports_max = thinking::has_level(&levels, "max");
        match effort.as_str() {
            "none" | "auto" => {
                gj::set_str(
                    out,
                    "thinking.type",
                    if effort == "none" { "disabled" } else { "adaptive" },
                );
                gj::delete(out, "thinking.budget_tokens");
                gj::delete(out, "output_config.effort");
            }
            _ => {
                let effort = thinking::map_to_claude_effort(&effort, supports_max).unwrap_or(&effort);
                gj::set_str(out, "thinking.type", "adaptive");
                gj::delete(out, "thinking.budget_tokens");
                gj::set_str(out, "output_config.effort", effort);
            }
        }
    } else if let Some(budget) = thinking::convert_level_to_budget(&effort) {
        match budget {
            0 => {
                gj::set_str(out, "thinking.type", "disabled");
            }
            -1 => {
                gj::set_str(out, "thinking.type", "enabled");
            }
            b if b > 0 => {
                gj::set_str(out, "thinking.type", "enabled");
                gj::set_int(out, "thinking.budget_tokens", b);
            }
            _ => {}
        }
    }
}

pub(crate) fn text_part(text: &[u8]) -> Vec<u8> {
    let mut part = br#"{"type":"text","text":""}"#.to_vec();
    gj::set_str(&mut part, "text", text);
    part
}

fn convert(model: &str, raw: &[u8], stream: bool, preserve_thinking: bool) -> Vec<u8> {
    let user_id = common::derive_claude_user_id(raw);
    let mut out = br#"{"model":"","max_tokens":32000,"messages":[],"metadata":{}}"#.to_vec();
    gj::set_str(&mut out, "metadata.user_id", &user_id);
    let root = gj::parse(raw);
    thinking_config(&mut out, &root, model);
    gj::set_str(&mut out, "model", model);
    let max_tokens = [root.get("max_tokens"), root.get("max_completion_tokens")]
        .into_iter()
        .find(Res::exists);
    if let Some(max_tokens) = max_tokens {
        gj::set_int(&mut out, "max_tokens", max_tokens.int());
    }
    let top_p = root.get("top_p");
    if top_p.exists() {
        gj::set_f64(&mut out, "top_p", top_p.float());
    }
    let stop = root.get("stop");
    if stop.exists() {
        if stop.is_array() {
            let mut sequences = vec![];
            stop.each(|_, v| {
                sequences.push(v.bytes().into_owned());
                true
            });
            if !sequences.is_empty() {
                gj::set_strs(&mut out, "stop_sequences", &sequences);
            }
        } else {
            gj::set_strs(&mut out, "stop_sequences", &[stop.bytes()]);
        }
    }
    gj::set_bool(&mut out, "stream", stream);

    let mut system: Vec<Vec<u8>> = vec![];
    let mut message_blocks: Vec<Vec<u8>> = vec![];
    let messages = root.get("messages");
    if messages.exists() && messages.is_array() {
        let mut last_tool: HashMap<Vec<u8>, Res<'_>> = HashMap::new();
        messages.each(|_, message| {
            if message.get("role").str() == "tool" {
                let id = message.get("tool_call_id").bytes().into_owned();
                if !id.is_empty() {
                    last_tool.insert(id, message);
                }
            }
            true
        });
        let mut emitted: HashSet<Vec<u8>> = HashSet::new();
        let mut acc = common::ClaudeMessages::default();
        messages.each(|_, message| {
            let role = message.get("role").bytes().into_owned();
            let content = message.get("content");
            match role.as_slice() {
                b"system" | b"developer" => {
                    let start = system.len();
                    if content.kind == Kind::String && !content.s.is_empty() {
                        system.push(common::attach_cache_control(text_part(&content.bytes()), &message));
                    } else if content.is_array() {
                        content.each(|_, part| {
                            if part.get("type").str() == "text" {
                                let block = text_part(&part.get("text").bytes());
                                system.push(common::attach_cache_control(block, &part));
                            }
                            true
                        });
                        if message.get("cache_control").exists()
                            && system.len() > start
                            && let Some(last) = system.last_mut()
                            && !gj::get(last, "cache_control").exists()
                        {
                            *last = common::attach_cache_control(std::mem::take(last), &message);
                        }
                    }
                }
                b"user" | b"assistant" => {
                    let mut blocks: Vec<Vec<u8>> = vec![];
                    if preserve_thinking && role == b"assistant" {
                        let reasoning = message.get("reasoning_content");
                        if reasoning.kind == Kind::String && !trim_space(&reasoning.s).is_empty() {
                            let mut part = br#"{"type":"thinking","thinking":"","signature":""}"#.to_vec();
                            gj::set_str(&mut part, "thinking", &reasoning.s);
                            blocks.push(part);
                        }
                    }
                    if content.kind == Kind::String && !content.s.is_empty() {
                        blocks.push(text_part(&content.s));
                    } else if content.is_array() {
                        content.each(|_, part| {
                            if let Some(p) = content_part(&part) {
                                blocks.push(common::attach_cache_control(p, &part));
                            }
                            true
                        });
                    }
                    let calls = message.get("tool_calls");
                    if calls.is_array() && role == b"assistant" {
                        calls.each(|_, call| {
                            if call.get("type").str() == "function" {
                                blocks.push(tool_use(&call));
                            }
                            true
                        });
                    }
                    let mut msg = br#"{"role":"","content":[]}"#.to_vec();
                    gj::set_str(&mut msg, "role", &role);
                    gj::set_raw(&mut msg, "content", gj::join(&blocks));
                    acc.append(&common::attach_message_cache_control(msg, &message));
                }
                b"tool" => {
                    let raw_id = message.get("tool_call_id").bytes().into_owned();
                    let id = common::sanitize_claude_tool_id(&raw_id);
                    if !raw_id.is_empty() && !emitted.insert(raw_id.clone()) {
                        return true;
                    }
                    let target = last_tool.get(&raw_id).cloned().unwrap_or(message);
                    let mut msg =
                        br#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"","content":""}]}"#.to_vec();
                    gj::set_str(&mut msg, "content.0.tool_use_id", &id);
                    match tool_result_content(&target.get("content")) {
                        (value, true) => gj::set_raw(&mut msg, "content.0.content", value),
                        (value, false) => gj::set_str(&mut msg, "content.0.content", value),
                    };
                    acc.append(&common::attach_tool_message_cache_control(msg, &target));
                }
                _ => {}
            }
            true
        });
        message_blocks = acc.messages();
    }
    let instruction = common::claude_structured_output_instruction(&root.get("response_format"));
    if !instruction.is_empty() {
        system.push(text_part(&instruction));
    }
    if message_blocks.is_empty() && !system.is_empty() {
        message_blocks.push(br#"{"role":"user","content":[{"type":"text","text":""}]}"#.to_vec());
    }
    if !system.is_empty() {
        gj::set_raw(&mut out, "system", gj::join(&system));
    }
    gj::set_items(&mut out, "messages", &message_blocks);

    tools(&mut out, &root);
    if root.get("parallel_tool_calls").kind == Kind::False {
        if gj::get(&out, "tool_choice").exists() {
            if gj::get(&out, "tool_choice.type").str() != "none" {
                gj::set_bool(&mut out, "tool_choice.disable_parallel_tool_use", true);
            }
        } else if gj::get(&out, "tools").exists() {
            gj::set_raw(
                &mut out,
                "tool_choice",
                r#"{"type":"auto","disable_parallel_tool_use":true}"#,
            );
        }
    }
    thinking::apply_translated_summary_to_claude(out, raw, "openai", model)
}

fn tool_use(call: &Res<'_>) -> Vec<u8> {
    let mut id = call.get("id").bytes().into_owned();
    if id.is_empty() {
        id = common::generate_claude_tool_call_id();
    }
    let id = common::sanitize_claude_tool_id(&id);
    let function = call.get("function");
    let mut part = br#"{"type":"tool_use","id":"","name":"","input":{}}"#.to_vec();
    gj::set_str(&mut part, "id", &id);
    gj::set_str(
        &mut part,
        "name",
        common::sanitize_claude_function_name(&function.get("name").bytes()),
    );
    let args = function.get("arguments").bytes();
    let parsed = gj::parse(&args);
    let input: &[u8] = if !args.is_empty() && gj::valid(&args) && parsed.is_object() {
        &parsed.raw
    } else {
        b"{}"
    };
    gj::set_raw(&mut part, "input", input);
    part
}

fn tools(out: &mut Vec<u8>, root: &Res<'_>) {
    let mut allowed_names: HashSet<Vec<u8>> = HashSet::new();
    let mut allowed = false;
    let mut mode = "auto".to_owned();
    let choice = root.get("tool_choice");
    if choice.is_object() && choice.get("type").str() == "allowed_tools" {
        allowed = true;
        let mut list = choice.get("allowed_tools.tools").array();
        if list.is_empty() {
            list = choice.get("tools").array();
        }
        for t in list {
            let mut name = trim_space(&t.get("function.name").bytes()).to_vec();
            if name.is_empty() {
                name = trim_space(&t.get("name").bytes()).to_vec();
            }
            if !name.is_empty() {
                allowed_names.insert(common::sanitize_claude_function_name(&name));
                allowed_names.insert(name);
            }
        }
        let mut value = choice.get("allowed_tools.mode").str().trim().to_lowercase();
        if value.is_empty() {
            value = choice.get("mode").str().trim().to_lowercase();
        }
        if !value.is_empty() {
            mode = value;
        }
    }
    let mut converted: Vec<Vec<u8>> = vec![];
    let list = root.get("tools");
    if list.is_array() && !list.array().is_empty() {
        list.each(|_, tool| {
            if tool.get("type").str() != "function" {
                return true;
            }
            let function = tool.get("function");
            let name = function.get("name").bytes().into_owned();
            let sanitized = common::sanitize_claude_function_name(&name);
            if allowed && !allowed_names.contains(&name) && !allowed_names.contains(&sanitized) {
                return true;
            }
            let mut t = br#"{"name":"","description":""}"#.to_vec();
            gj::set_str(&mut t, "name", &sanitized);
            gj::set_str(&mut t, "description", function.get("description").bytes());
            let parameters = [function.get("parameters"), function.get("parametersJsonSchema")]
                .into_iter()
                .find(Res::exists);
            let schema = common::normalize_claude_tool_input_schema(parameters.as_ref().map(|p| &*p.raw));
            gj::set_raw(&mut t, "input_schema", schema);
            t = common::attach_cache_control(t, &tool);
            if !gj::get(&t, "cache_control").exists() {
                t = common::attach_cache_control(t, &function);
            }
            let mut strict = function.get("strict");
            if !strict.exists() {
                strict = tool.get("strict");
            }
            if strict.is_bool() {
                gj::set_bool(&mut t, "strict", strict.kind == Kind::True);
            }
            converted.push(t);
            true
        });
        if converted.is_empty() {
            gj::delete(out, "tools");
        } else {
            gj::set_raw(out, "tools", gj::join(&converted));
        }
    }
    if allowed {
        let choice = if converted.is_empty() {
            r#"{"type":"none"}"#
        } else if mode == "required" {
            r#"{"type":"any"}"#
        } else {
            r#"{"type":"auto"}"#
        };
        gj::set_raw(out, "tool_choice", choice);
        return;
    }
    if !choice.exists() || choice.kind == Kind::Null {
        return;
    }
    let kind = match choice.kind {
        Kind::String => choice.str().into_owned(),
        Kind::Json => choice.get("type").str().into_owned(),
        _ => return,
    };
    match (choice.kind, kind.as_str()) {
        (_, "none") => {
            gj::set_raw(out, "tool_choice", r#"{"type":"none"}"#);
        }
        (_, "auto") => {
            gj::set_raw(out, "tool_choice", r#"{"type":"auto"}"#);
        }
        (_, "required") | (Kind::Json, "any") => {
            gj::set_raw(out, "tool_choice", r#"{"type":"any"}"#);
        }
        (Kind::Json, "function") => {
            let mut name = choice.get("function.name").bytes().into_owned();
            if name.is_empty() {
                name = choice.get("name").bytes().into_owned();
            }
            if name.is_empty() {
                gj::set_raw(out, "tool_choice", r#"{"type":"none"}"#);
            } else {
                let mut c = br#"{"type":"tool","name":""}"#.to_vec();
                gj::set_str(&mut c, "name", common::sanitize_claude_function_name(&name));
                gj::set_raw(out, "tool_choice", c);
            }
        }
        _ => {}
    }
}

/// convertOpenAIContentPartToClaudePartRaw.
fn content_part(part: &Res<'_>) -> Option<Vec<u8>> {
    match &*part.get("type").str() {
        "text" => Some(text_part(&part.get("text").bytes())),
        "image_url" => image_part(&part.get("image_url.url").bytes()),
        "file" => {
            let data = part.get("file.file_data").bytes();
            if !data.starts_with(b"data:") {
                return None;
            }
            let semicolon = data.iter().position(|&c| c == b';')?;
            let comma = data.iter().position(|&c| c == b',')?;
            if comma <= semicolon {
                return None;
            }
            let mut doc = br#"{"type":"document","source":{"type":"base64","media_type":"","data":""}}"#.to_vec();
            gj::set_str(&mut doc, "source.media_type", &data[5..semicolon]);
            gj::set_str(&mut doc, "source.data", &data[comma + 1..]);
            Some(doc)
        }
        _ => None,
    }
}

fn image_part(url: &[u8]) -> Option<Vec<u8>> {
    if url.is_empty() {
        return None;
    }
    if url.starts_with(b"data:") {
        let comma = url.iter().position(|&c| c == b',')?;
        let header = &url[..comma];
        let media = header.split(|&c| c == b';').next().unwrap_or_default();
        let media = media.strip_prefix(b"data:").unwrap_or(media);
        let mut part = br#"{"type":"image","source":{"type":"base64","media_type":"","data":""}}"#.to_vec();
        gj::set_str(
            &mut part,
            "source.media_type",
            if media.is_empty() {
                &b"application/octet-stream"[..]
            } else {
                media
            },
        );
        gj::set_str(&mut part, "source.data", &url[comma + 1..]);
        return Some(part);
    }
    let mut part = br#"{"type":"image","source":{"type":"url","url":""}}"#.to_vec();
    gj::set_str(&mut part, "source.url", url);
    Some(part)
}

/// convertOpenAIToolResultContent: `(value, is_raw_json)`.
fn tool_result_content(content: &Res<'_>) -> (Vec<u8>, bool) {
    if !content.exists() {
        return (vec![], false);
    }
    if content.kind == Kind::String {
        return (content.s.to_vec(), false);
    }
    if content.is_array() {
        let mut parts = vec![];
        content.each(|_, part| {
            if part.kind == Kind::String {
                parts.push(text_part(&part.s));
            } else if let Some(p) = content_part(&part) {
                parts.push(p);
            }
            true
        });
        if !parts.is_empty() || content.array().is_empty() {
            return (gj::join(&parts), true);
        }
        return (content.raw.to_vec(), false);
    }
    if content.is_object()
        && let Some(p) = content_part(content)
    {
        return (gj::join(&[p]), true);
    }
    (content.raw.to_vec(), false)
}
