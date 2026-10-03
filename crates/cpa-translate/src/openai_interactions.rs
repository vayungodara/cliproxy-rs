//! OpenAI Chat Completions <-> Gemini Interactions requests
//! (internal/translator/openai/interactions/chat-completions: openai_interactions_request.go
//! and interactions_openai_request.go). The response sides live in
//! [`crate::openai_interactions_response`].

use cpa_common::json::{self as gj, Kind, Res};

use crate::common::{go_lower, normalize_openai_file_data, trim_space};
use crate::openai_interactions_response as response;
use crate::{Registered, gemini_interactions::first_existing};

/// OpenAI client, Interactions upstream.
pub static OPENAI_TO_INTERACTIONS: Registered = registered!(
    OpenAI -> Interactions,
    request: |ctx, body| Ok(openai_to_interactions(ctx.model, body, ctx.stream)),
    non_stream: response::interactions_to_openai_non_stream,
    go_stream: response::interactions_to_openai_stream,
    token_count: None,
);

/// Interactions client, OpenAI upstream.
pub static INTERACTIONS_TO_OPENAI: Registered = registered!(
    Interactions -> OpenAI,
    request: |ctx, body| Ok(interactions_to_openai(ctx.model, body, ctx.stream)),
    non_stream: response::openai_to_interactions_non_stream,
    go_stream: response::openai_to_interactions_stream,
    token_count: None,
);

/// firstNonEmpty: the first value that is not blank, as is.
pub(crate) fn first_nonblank(values: &[&[u8]]) -> Vec<u8> {
    values
        .iter()
        .find(|v| !trim_space(v).is_empty())
        .map(|v| v.to_vec())
        .unwrap_or_default()
}

fn string(res: &Res<'_>) -> Vec<u8> {
    res.bytes().into_owned()
}

/// isAntigravityModel.
pub(crate) fn is_antigravity(model: &[u8]) -> bool {
    go_lower(model).windows(11).any(|w| w == b"antigravity")
}

const EXTERNAL_TOOL_PREFIX: &[u8] = b"external_";

fn antigravity_collides(name: &[u8]) -> bool {
    matches!(name, b"read_file" | b"write_file" | b"execute_code")
}

/// common.AntigravityToolNameToUpstream: client tools that collide with the agent's
/// sandbox tools get the `external_` prefix.
pub(crate) fn antigravity_name_to_upstream(name: &[u8]) -> Vec<u8> {
    if antigravity_collides(name) {
        [EXTERNAL_TOOL_PREFIX, name].concat()
    } else {
        name.to_vec()
    }
}

/// common.AntigravityUpstreamToolNameToClient.
pub(crate) fn antigravity_name_to_client(name: &[u8]) -> Vec<u8> {
    match name.strip_prefix(EXTERNAL_TOOL_PREFIX) {
        Some(base) if antigravity_collides(base) => base.to_vec(),
        _ => name.to_vec(),
    }
}

/// jsonStringValue: a string's value, any other value's raw JSON, or the fallback.
pub(crate) fn json_string_value(value: &Res<'_>, fallback: &[u8]) -> Vec<u8> {
    if !value.exists() {
        fallback.to_vec()
    } else if value.kind == Kind::String {
        string(value)
    } else {
        value.raw.to_vec()
    }
}

/// copyNumber: the raw value when it exists.
fn copy_raw(out: &mut Vec<u8>, path: &str, value: &Res<'_>) {
    if value.exists() {
        gj::set_raw(out, path, &value.raw);
    }
}

// ---------------------------------------------------------------------------------------
// OpenAI Chat request -> Interactions request

/// ConvertOpenAIRequestToInteractions.
pub(crate) fn openai_to_interactions(model: &str, raw: &[u8], stream: bool) -> Vec<u8> {
    let root = gj::parse(raw);
    let mut out = br#"{"model":"","input":[]}"#.to_vec();
    let model = first_nonblank(&[model.as_bytes(), &root.get("model").bytes()]);
    gj::set_str(&mut out, "model", &model);
    let stream_value = root.get("stream");
    if stream_value.exists() {
        gj::set_bool(&mut out, "stream", stream_value.bool());
    } else if stream {
        gj::set_bool(&mut out, "stream", true);
    }
    let previous = first_nonblank(&[
        &root.get("previous_response_id").bytes(),
        &root.get("previous_interaction_id").bytes(),
    ]);
    if !previous.is_empty() {
        gj::set_str(&mut out, "previous_interaction_id", &previous);
    }
    let environment = first_nonblank(&[&root.get("environment_id").bytes(), &root.get("environment.id").bytes()]);
    if !environment.is_empty() {
        gj::set_str(&mut out, "environment_id", &environment);
    }
    copy_raw(&mut out, "agent_config", &root.get("agent_config"));
    let antigravity = is_antigravity(&model);
    append_messages(&mut out, &root.get("messages"), antigravity);
    copy_generation_config(&mut out, &root, antigravity);
    let tools = root.get("tools");
    if tools.is_array() {
        let mut items = vec![];
        tools.each(|_, tool| {
            items.extend(tool_to_interactions(&tool, antigravity));
            true
        });
        if !items.is_empty() {
            gj::set_raw(&mut out, "tools", gj::join(&items));
        }
    }
    out
}

/// appendOpenAIMessagesToInteractions.
fn append_messages(out: &mut Vec<u8>, messages: &Res<'_>, antigravity: bool) {
    if !messages.is_array() {
        return;
    }
    let mut items = vec![];
    let mut system = vec![];
    let mut names: std::collections::HashMap<Vec<u8>, Vec<u8>> = std::collections::HashMap::new();
    messages.each(|_, message| {
        let role = go_lower(trim_space(&message.get("role").bytes()));
        match role.as_slice() {
            b"system" | b"developer" => {
                let text = chat_content_text(&message.get("content"));
                if !text.is_empty() {
                    if !system.is_empty() {
                        system.push(b'\n');
                    }
                    system.extend_from_slice(&text);
                }
            }
            b"assistant" => {
                let reasoning = message.get("reasoning_content");
                if reasoning.exists() {
                    for text in reasoning_texts(&reasoning) {
                        items.push(text_step("thought", &text));
                    }
                }
                items.extend(content_step("model_output", &message.get("content")));
                let calls = message.get("tool_calls");
                if calls.is_array() {
                    calls.each(|_, call| {
                        let id = string(&call.get("id"));
                        let name = string(&call.get("function.name"));
                        if !id.is_empty() && !name.is_empty() {
                            names.insert(id, name);
                        }
                        items.extend(tool_call_step(&call, antigravity));
                        true
                    });
                }
            }
            b"tool" | b"function" => items.push(tool_result(&message, antigravity, &names)),
            _ => items.extend(content_step("user_input", &message.get("content"))),
        }
        true
    });
    if !system.is_empty() {
        gj::set_str(out, "system_instruction", &system);
    }
    gj::set_items(out, "input", &items);
}

/// interactionsTextStep.
pub(crate) fn text_step(kind: &str, text: &[u8]) -> Vec<u8> {
    let mut step = br#"{"type":"","content":[{"type":"text","text":""}]}"#.to_vec();
    gj::set_str(&mut step, "type", kind);
    gj::set_str(&mut step, "content.0.text", text);
    step
}

/// openAIReasoningTexts.
pub(crate) fn reasoning_texts(reasoning: &Res<'_>) -> Vec<Vec<u8>> {
    if reasoning.kind == Kind::String {
        let text = string(reasoning);
        return if text.is_empty() { vec![] } else { vec![text] };
    }
    let mut texts = vec![];
    if reasoning.is_array() {
        reasoning.each(|_, item| {
            let text = first_nonblank(&[&item.get("text").bytes(), &item.get("content").bytes()]);
            if !text.is_empty() {
                texts.push(text);
            }
            true
        });
    }
    texts
}

/// setRawJSONValue.
fn set_raw_json_value(out: &mut Vec<u8>, path: &str, value: &Res<'_>, fallback: &[u8]) {
    if !value.exists() {
        gj::set_raw(out, path, fallback);
        return;
    }
    let text = value.bytes();
    let raw = trim_space(&text);
    if value.kind == Kind::String && gj::valid(raw) {
        gj::set_raw(out, path, raw);
    } else if value.kind == Kind::String {
        gj::set_str(out, path, &text);
    } else {
        gj::set_raw(out, path, &value.raw);
    }
}

/// openAIToolCallToInteractionsStep.
pub(crate) fn tool_call_step(call: &Res<'_>, antigravity: bool) -> Option<Vec<u8>> {
    let kind = call.get("type").bytes();
    if !kind.is_empty() && kind.as_ref() != b"function" {
        return None;
    }
    let function = call.get("function");
    if !function.exists() {
        return None;
    }
    let mut step = br#"{"type":"function_call","name":"","arguments":{}}"#.to_vec();
    let id = string(&call.get("id"));
    if !id.is_empty() {
        gj::set_str(&mut step, "id", &id);
    }
    let mut name = string(&function.get("name"));
    if antigravity {
        name = antigravity_name_to_upstream(&name);
    }
    gj::set_str(&mut step, "name", &name);
    set_raw_json_value(&mut step, "arguments", &function.get("arguments"), b"{}");
    Some(step)
}

/// openAIChatContentStep.
fn content_step(kind: &str, content: &Res<'_>) -> Option<Vec<u8>> {
    let mut items = vec![];
    if content.kind == Kind::String {
        let text = string(content);
        if text.is_empty() {
            return None;
        }
        let mut part = br#"{"type":"text","text":""}"#.to_vec();
        gj::set_str(&mut part, "text", &text);
        items.push(part);
    } else if content.is_array() {
        content.each(|_, part| {
            items.extend(chat_part_to_interactions(&part));
            true
        });
    } else if content.is_object() {
        items.extend(chat_part_to_interactions(content));
    }
    if items.is_empty() {
        return None;
    }
    let mut step = br#"{"type":"","content":[]}"#.to_vec();
    gj::set_str(&mut step, "type", kind);
    gj::set_raw(&mut step, "content", gj::join(&items));
    Some(step)
}

/// openAIInputAudioMIMEType.
fn input_audio_mime(format: &[u8]) -> &'static str {
    match go_lower(trim_space(format)).as_slice() {
        b"wav" => "audio/wav",
        b"flac" => "audio/flac",
        b"opus" => "audio/opus",
        b"pcm16" => "audio/pcm",
        _ => "audio/mpeg",
    }
}

/// openAIChatContentPartToInteractions.
fn chat_part_to_interactions(part: &Res<'_>) -> Option<Vec<u8>> {
    let mut kind = go_lower(trim_space(&part.get("type").bytes()));
    if kind.is_empty() && part.get("text").exists() {
        kind = b"text".to_vec();
    }
    match kind.as_slice() {
        b"text" | b"input_text" | b"output_text" => {
            let mut out = br#"{"type":"text","text":""}"#.to_vec();
            gj::set_str(&mut out, "text", part.get("text").bytes());
            Some(out)
        }
        b"image_url" | b"input_image" | b"image" => Some(chat_image_part(part)),
        b"input_audio" | b"audio" => {
            let audio = part.get("input_audio");
            let data = first_nonblank(&[&audio.get("data").bytes(), &part.get("data").bytes()]);
            if data.is_empty() {
                return None;
            }
            let mut out = br#"{"type":"audio","data":""}"#.to_vec();
            gj::set_str(&mut out, "data", &data);
            let format = first_nonblank(&[&audio.get("format").bytes(), &part.get("format").bytes()]);
            if !format.is_empty() {
                gj::set_str(&mut out, "mime_type", input_audio_mime(&format));
            }
            Some(out)
        }
        b"file" | b"input_file" | b"document" => {
            let file = part.get("file");
            let filename = first_nonblank(&[&file.get("filename").bytes(), &part.get("filename").bytes()]);
            let fallback = first_nonblank(&[
                &file.get("mime_type").bytes(),
                &file.get("mimeType").bytes(),
                &part.get("mime_type").bytes(),
                &part.get("mimeType").bytes(),
            ]);
            let data = first_nonblank(&[
                &file.get("file_data").bytes(),
                &part.get("file_data").bytes(),
                &part.get("data").bytes(),
            ]);
            let url = first_nonblank(&[
                &file.get("file_url").bytes(),
                &part.get("file_url").bytes(),
                &part.get("url").bytes(),
            ]);
            let mut out = br#"{"type":"document"}"#.to_vec();
            if !filename.is_empty() {
                gj::set_str(&mut out, "filename", &filename);
            }
            let mut has_content = false;
            if let Some((mime, data)) = normalize_openai_file_data(&filename, &fallback, &data) {
                gj::set_str(&mut out, "mime_type", &mime);
                gj::set_str(&mut out, "data", &data);
                has_content = true;
            }
            if !url.is_empty() {
                gj::set_str(&mut out, "file_url", &url);
                has_content = true;
            }
            has_content.then_some(out)
        }
        _ => None,
    }
}

/// openAIChatParseDataURL: (MIME type, data) of a base64 `data:` URL.
fn parse_data_url(value: &[u8]) -> Option<(&[u8], &[u8])> {
    let rest = value.strip_prefix(b"data:")?;
    let comma = rest.iter().position(|&c| c == b',')?;
    let (meta, data) = (&rest[..comma], &rest[comma + 1..]);
    let (mime, encoding) = match meta.iter().position(|&c| c == b';') {
        Some(i) => (&meta[..i], &meta[i + 1..]),
        None => (meta, &b""[..]),
    };
    use cpa_common::gostr::GoStr;
    let base64 = String::from_utf8_lossy(encoding).go_eq_fold("base64");
    (base64 && !trim_space(mime).is_empty() && !data.is_empty()).then_some((mime, data))
}

/// openAIChatImagePartToInteractions.
fn chat_image_part(part: &Res<'_>) -> Vec<u8> {
    let mut out = br#"{"type":"image"}"#.to_vec();
    let url = first_nonblank(&[
        &part.get("image_url.url").bytes(),
        &part.get("image_url").bytes(),
        &part.get("url").bytes(),
    ]);
    if let Some((mime, data)) = parse_data_url(&url) {
        gj::set_str(&mut out, "mime_type", mime);
        gj::set_str(&mut out, "data", data);
        return out;
    }
    let data = string(&part.get("data"));
    if !data.is_empty() {
        gj::set_str(&mut out, "data", &data);
        let mime = string(&part.get("mime_type"));
        if !mime.is_empty() {
            gj::set_str(&mut out, "mime_type", &mime);
        }
        return out;
    }
    if !url.is_empty() {
        gj::set_str(&mut out, "image_url", &url);
    }
    out
}

/// openAIToolResultToInteractions.
fn tool_result(message: &Res<'_>, antigravity: bool, names: &std::collections::HashMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
    let mut out = br#"{"type":"function_result","result":""}"#.to_vec();
    let call_id = first_nonblank(&[&message.get("tool_call_id").bytes(), &message.get("id").bytes()]);
    if !call_id.is_empty() {
        gj::set_str(&mut out, "call_id", &call_id);
    }
    let mut name = string(&message.get("name"));
    if name.is_empty() && !call_id.is_empty() {
        name = names.get(&call_id).cloned().unwrap_or_default();
    }
    if !name.is_empty() {
        if antigravity {
            name = antigravity_name_to_upstream(&name);
        }
        gj::set_str(&mut out, "name", &name);
    }
    let content = message.get("content");
    if content.kind == Kind::String {
        gj::set_str(&mut out, "result", content.bytes());
    } else if content.exists() {
        gj::set_raw(&mut out, "result", &content.raw);
    }
    out
}

/// copyOpenAIChatGenerationConfigToInteractions.
fn copy_generation_config(out: &mut Vec<u8>, root: &Res<'_>, antigravity: bool) {
    if antigravity {
        let max = first_existing(root, &["max_completion_tokens", "max_tokens", "max_output_tokens"]);
        if max.exists() && !root.get("agent_config.max_total_tokens").exists() {
            gj::set_int(out, "agent_config.max_total_tokens", max.int());
        }
    } else {
        for (path, sources) in [
            (
                "generation_config.max_output_tokens",
                &["max_completion_tokens", "max_tokens"][..],
            ),
            ("generation_config.temperature", &["temperature"]),
            ("generation_config.top_p", &["top_p"]),
            ("generation_config.presence_penalty", &["presence_penalty"]),
            ("generation_config.frequency_penalty", &["frequency_penalty"]),
            ("generation_config.candidate_count", &["n"]),
            ("generation_config.stop_sequences", &["stop"]),
        ] {
            copy_raw(out, path, &first_existing(root, sources));
        }
    }
    let choice = root.get("tool_choice");
    if choice.exists() {
        let mut raw = choice.raw.to_vec();
        if antigravity && choice.is_object() {
            let function_name = string(&choice.get("function.name"));
            let name = string(&choice.get("name"));
            if !function_name.is_empty() {
                gj::set_str(&mut raw, "function.name", antigravity_name_to_upstream(&function_name));
            } else if !name.is_empty() {
                gj::set_str(&mut raw, "name", antigravity_name_to_upstream(&name));
            }
        }
        gj::set_raw(out, "generation_config.tool_choice", raw);
    }
    let effort = root.get("reasoning_effort");
    if effort.kind == Kind::String {
        gj::set_str(
            out,
            "generation_config.thinking_level",
            go_lower(trim_space(&effort.bytes())),
        );
    }
    copy_raw(out, "response_format", &root.get("response_format"));
    copy_raw(out, "response_modalities", &root.get("modalities"));
    let tier = root.get("service_tier");
    if tier.kind == Kind::String {
        gj::set_str(out, "service_tier", tier.bytes());
    }
}

/// openAIChatToolToInteractions.
fn tool_to_interactions(tool: &Res<'_>, antigravity: bool) -> Option<Vec<u8>> {
    let kind = go_lower(trim_space(&tool.get("type").bytes()));
    if !kind.is_empty() && kind != b"function" {
        return None;
    }
    let mut name = first_nonblank(&[&tool.get("function.name").bytes(), &tool.get("name").bytes()]);
    if name.is_empty() {
        return None;
    }
    if antigravity {
        name = antigravity_name_to_upstream(&name);
    }
    let mut out = br#"{"type":"function","name":""}"#.to_vec();
    gj::set_str(&mut out, "name", &name);
    let description = first_existing(tool, &["function.description", "description"]);
    if description.exists() {
        gj::set_str(&mut out, "description", description.bytes());
    }
    copy_raw(
        &mut out,
        "parameters",
        &first_existing(tool, &["function.parameters", "parameters"]),
    );
    Some(out)
}

/// openAIChatContentText.
fn chat_content_text(content: &Res<'_>) -> Vec<u8> {
    if content.kind == Kind::String {
        return string(content);
    }
    if content.is_object() {
        return string(&content.get("text"));
    }
    let mut out = vec![];
    if content.is_array() {
        content.each(|_, part| {
            out.extend_from_slice(&part.get("text").bytes());
            true
        });
    }
    out
}

// ---------------------------------------------------------------------------------------
// Interactions request -> OpenAI Chat request

/// ConvertInteractionsRequestToOpenAI.
pub(crate) fn interactions_to_openai(model: &str, raw: &[u8], stream: bool) -> Vec<u8> {
    let root = gj::parse(raw);
    let mut out = br#"{"model":"","messages":[]}"#.to_vec();
    let model = first_nonblank(&[model.as_bytes(), &root.get("model").bytes()]);
    gj::set_str(&mut out, "model", &model);
    if stream || root.get("stream").bool() {
        gj::set_bool(&mut out, "stream", true);
    }
    let mut messages = vec![];
    let system = interactions_text(&root.get("system_instruction"));
    if !system.is_empty() {
        let mut message = br#"{"role":"system","content":""}"#.to_vec();
        gj::set_str(&mut message, "content", &system);
        messages.push(message);
    }
    let antigravity = is_antigravity(&model);
    let input = root.get("input");
    if input.kind == Kind::String {
        let mut message = br#"{"role":"user","content":""}"#.to_vec();
        gj::set_str(&mut message, "content", input.bytes());
        messages.push(message);
    } else if input.is_array() {
        input.each(|_, step| {
            messages.extend(step_to_message(&step, antigravity));
            true
        });
    } else if input.is_object() {
        messages.extend(step_to_message(&input, antigravity));
    }
    gj::set_items(&mut out, "messages", &messages);
    copy_tools_to_openai(&mut out, &root, antigravity);
    copy_generation_config_to_openai(&mut out, &root);
    copy_top_level(&mut out, &root);
    out
}

/// appendInteractionsStepToOpenAI.
fn step_to_message(step: &Res<'_>, antigravity: bool) -> Option<Vec<u8>> {
    match step.get("type").bytes().as_ref() {
        b"user_input" => Some(content_message(step, "user")),
        b"model_output" => Some(content_message(step, "assistant")),
        b"thought" => {
            let mut message = br#"{"role":"assistant","content":"","reasoning_content":""}"#.to_vec();
            gj::set_str(
                &mut message,
                "reasoning_content",
                interactions_text(&step.get("content")),
            );
            Some(message)
        }
        b"function_call" => {
            let mut message = br#"{"role":"assistant","content":"","tool_calls":[]}"#.to_vec();
            let mut call = br#"{"id":"","type":"function","function":{"name":"","arguments":"{}"}}"#.to_vec();
            let id = first_nonblank(&[&step.get("call_id").bytes(), &step.get("id").bytes(), b"call_0"]);
            gj::set_str(&mut call, "id", &id);
            let mut name = string(&step.get("name"));
            if antigravity {
                name = antigravity_name_to_client(&name);
            }
            gj::set_str(&mut call, "function.name", &name);
            gj::set_str(
                &mut call,
                "function.arguments",
                json_string_value(&step.get("arguments"), b"{}"),
            );
            gj::set_items(&mut message, "tool_calls", &[call]);
            Some(message)
        }
        b"function_result" => {
            let mut message = br#"{"role":"tool","tool_call_id":"","content":""}"#.to_vec();
            let id = first_nonblank(&[&step.get("call_id").bytes(), &step.get("id").bytes()]);
            gj::set_str(&mut message, "tool_call_id", &id);
            let result = first_existing(step, &["result", "output"]);
            gj::set_str(&mut message, "content", json_string_value(&result, b""));
            Some(message)
        }
        _ if step.kind == Kind::String => {
            let mut message = br#"{"role":"","content":""}"#.to_vec();
            gj::set_str(&mut message, "role", "user");
            gj::set_str(&mut message, "content", step.bytes());
            Some(message)
        }
        _ => None,
    }
}

/// appendInteractionsMessageToOpenAI with appendInteractionsContentToOpenAIMessage.
fn content_message(step: &Res<'_>, role: &str) -> Vec<u8> {
    let mut message = br#"{"role":"","content":""}"#.to_vec();
    gj::set_str(&mut message, "role", role);
    let content = step.get("content");
    if content.kind == Kind::String {
        gj::set_str(&mut message, "content", content.bytes());
        return message;
    }
    let mut items = vec![];
    let mut text_only = true;
    let mut text = vec![];
    let mut add = |part: &Res<'_>| {
        if let Some(converted) = content_part_to_openai(part) {
            if gj::get(&converted, "type").bytes().as_ref() == b"text" {
                text.extend_from_slice(&gj::get(&converted, "text").bytes());
            } else {
                text_only = false;
            }
            items.push(converted);
        }
    };
    if content.is_array() {
        content.each(|_, part| {
            add(&part);
            true
        });
    } else if content.is_object() {
        add(&content);
    }
    if !items.is_empty() {
        if text_only {
            gj::set_str(&mut message, "content", &text);
        } else {
            gj::set_raw(&mut message, "content", gj::join(&items));
        }
    }
    message
}

/// interactionsContentPartToOpenAI.
fn content_part_to_openai(part: &Res<'_>) -> Option<Vec<u8>> {
    let mut kind = string(&part.get("type"));
    if kind.is_empty() && part.get("text").exists() {
        kind = b"text".to_vec();
    }
    let mut out;
    match kind.as_slice() {
        b"text" => {
            out = br#"{"type":"text","text":""}"#.to_vec();
            gj::set_str(&mut out, "text", part.get("text").bytes());
        }
        b"image" => {
            out = br#"{"type":"image_url","image_url":{"url":""}}"#.to_vec();
            gj::set_str(
                &mut out,
                "image_url.url",
                media_data_url(part, b"application/octet-stream"),
            );
        }
        b"audio" => {
            out = br#"{"type":"input_audio","input_audio":{"data":"","format":""}}"#.to_vec();
            gj::set_str(&mut out, "input_audio.data", part.get("data").bytes());
            gj::set_str(
                &mut out,
                "input_audio.format",
                audio_format(&part.get("mime_type").bytes()),
            );
        }
        b"video" => {
            out = br#"{"type":"video_url","video_url":{"url":""}}"#.to_vec();
            gj::set_str(&mut out, "video_url.url", media_data_url(part, b"video/mp4"));
        }
        b"document" | b"file" => {
            out = br#"{"type":"file","file":{"filename":"","file_data":""}}"#.to_vec();
            let filename = first_nonblank(&[
                &part.get("filename").bytes(),
                &file_name_from_mime(&part.get("mime_type").bytes()),
            ]);
            gj::set_str(&mut out, "file.filename", &filename);
            gj::set_str(&mut out, "file.file_data", part.get("data").bytes());
            let url = first_nonblank(&[&part.get("file_url").bytes(), &part.get("url").bytes()]);
            if !url.is_empty() {
                gj::delete(&mut out, "file.file_data");
                gj::set_str(&mut out, "file.file_url", &url);
            }
        }
        _ => return None,
    }
    Some(out)
}

/// interactionsMediaDataURL.
fn media_data_url(part: &Res<'_>, fallback_mime: &[u8]) -> Vec<u8> {
    let url = first_nonblank(&[
        &part.get("image_url").bytes(),
        &part.get("file_data").bytes(),
        &part.get("url").bytes(),
    ]);
    if !url.is_empty() {
        return url;
    }
    let data = string(&part.get("data"));
    if data.is_empty() {
        return vec![];
    }
    let mime = first_nonblank(&[&part.get("mime_type").bytes(), fallback_mime]);
    [&b"data:"[..], &mime, b";base64,", &data].concat()
}

/// openAIInputAudioFormatFromMIME.
fn audio_format(mime: &[u8]) -> &'static str {
    match go_lower(trim_space(mime)).as_slice() {
        b"audio/wav" | b"audio/wave" | b"audio/x-wav" => "wav",
        b"audio/flac" => "flac",
        b"audio/opus" | b"audio/ogg" => "opus",
        b"audio/pcm" | b"audio/l16" => "pcm16",
        _ => "mp3",
    }
}

/// openAIFileNameFromMIME.
fn file_name_from_mime(mime: &[u8]) -> Vec<u8> {
    match go_lower(trim_space(mime)).as_slice() {
        b"application/pdf" => b"document.pdf".to_vec(),
        b"text/plain" => b"document.txt".to_vec(),
        b"text/csv" => b"document.csv".to_vec(),
        b"application/json" => b"document.json".to_vec(),
        _ => match mime.iter().position(|&c| c == b'/') {
            Some(slash) if slash + 1 < mime.len() => {
                let suffix: Vec<u8> = mime[slash + 1..]
                    .iter()
                    .map(|&c| if c == b'+' { b'.' } else { c })
                    .collect();
                [&b"document."[..], &suffix].concat()
            }
            _ => b"document.bin".to_vec(),
        },
    }
}

/// openAIToolFromInteractionsTool.
fn tool_to_openai(tool: &Res<'_>, antigravity: bool) -> Option<Vec<u8>> {
    let mut name = first_nonblank(&[&tool.get("name").bytes(), &tool.get("function.name").bytes()]);
    if name.is_empty() {
        return None;
    }
    if antigravity {
        name = antigravity_name_to_client(&name);
    }
    let mut out = br#"{"type":"function","function":{"name":""}}"#.to_vec();
    gj::set_str(&mut out, "function.name", &name);
    let description = first_existing(tool, &["description", "function.description"]);
    if description.exists() {
        gj::set_str(&mut out, "function.description", description.bytes());
    }
    copy_raw(
        &mut out,
        "function.parameters",
        &first_existing(tool, &["parameters", "function.parameters", "parametersJsonSchema"]),
    );
    Some(out)
}

/// copyInteractionsToolsToOpenAI.
fn copy_tools_to_openai(out: &mut Vec<u8>, root: &Res<'_>, antigravity: bool) {
    let tools = root.get("tools");
    if !tools.is_array() {
        return;
    }
    let mut items = vec![];
    tools.each(|_, tool| {
        items.extend(tool_to_openai(&tool, antigravity));
        let decls = first_existing(&tool, &["function_declarations", "functionDeclarations"]);
        if decls.is_array() {
            decls.each(|_, decl| {
                items.extend(tool_to_openai(&decl, antigravity));
                true
            });
        }
        true
    });
    if !items.is_empty() {
        gj::set_raw(out, "tools", gj::join(&items));
    }
}

/// copyInteractionsGenerationConfigToOpenAI.
fn copy_generation_config_to_openai(out: &mut Vec<u8>, root: &Res<'_>) {
    let gen_cfg = first_existing(root, &["generation_config", "generationConfig"]);
    let pick = |gen_paths: &[&str], root_paths: &[&str]| {
        let value = first_existing(&gen_cfg, gen_paths);
        if value.exists() {
            value
        } else {
            first_existing(root, root_paths)
        }
    };
    copy_raw(out, "temperature", &pick(&["temperature"], &["temperature"]));
    copy_raw(
        out,
        "max_tokens",
        &pick(
            &["max_output_tokens", "maxOutputTokens"],
            &["max_tokens", "max_completion_tokens"],
        ),
    );
    copy_raw(out, "top_p", &pick(&["top_p", "topP"], &["top_p"]));
    copy_raw(out, "top_k", &pick(&["top_k", "topK"], &[]));
    copy_raw(out, "n", &pick(&["candidate_count", "candidateCount"], &["n"]));
    copy_raw(out, "stop", &pick(&["stop_sequences", "stopSequences"], &["stop"]));
    copy_raw(out, "tool_choice", &pick(&["tool_choice"], &["tool_choice"]));
    let effort = [
        gen_cfg.get("reasoning_effort"),
        gen_cfg.get("thinking_level"),
        gen_cfg.get("thinkingLevel"),
        gen_cfg.get("thinking_config.thinking_level"),
        gen_cfg.get("thinkingConfig.thinkingLevel"),
        root.get("reasoning_effort"),
    ]
    .into_iter()
    .find(|v| v.kind == Kind::String);
    if let Some(effort) = effort {
        let effort = go_lower(trim_space(&effort.bytes()));
        if !effort.is_empty() {
            gj::set_str(out, "reasoning_effort", &effort);
        }
    }
    copy_raw(out, "modalities", &root.get("response_modalities"));
}

/// copyInteractionsOpenAITopLevel.
fn copy_top_level(out: &mut Vec<u8>, root: &Res<'_>) {
    copy_raw(out, "response_format", &root.get("response_format"));
    let tier = root.get("service_tier");
    if tier.kind == Kind::String {
        gj::set_str(out, "service_tier", tier.bytes());
    }
    let previous = first_nonblank(&[
        &root.get("previous_interaction_id").bytes(),
        &root.get("previous_response_id").bytes(),
    ]);
    if !previous.is_empty() {
        gj::set_str(out, "previous_response_id", &previous);
    }
    let environment = first_nonblank(&[&root.get("environment_id").bytes(), &root.get("environment.id").bytes()]);
    if !environment.is_empty() {
        gj::set_str(out, "environment_id", &environment);
    }
    copy_raw(out, "agent_config", &root.get("agent_config"));
    for key in ["parallel_tool_calls", "seed", "user"] {
        copy_raw(out, key, &root.get(key));
    }
}

/// interactionsText: a string, a `text` field, or the joined texts of `content`/`parts`.
fn interactions_text(value: &Res<'_>) -> Vec<u8> {
    if !value.exists() {
        return vec![];
    }
    if value.kind == Kind::String {
        return string(value);
    }
    let text = value.get("text");
    if text.exists() {
        return string(&text);
    }
    for path in ["content", "parts"] {
        let parts = value.get(path);
        if !parts.is_array() {
            continue;
        }
        let mut out = vec![];
        parts.each(|_, part| {
            out.extend(first_nonblank(&[
                &part.get("text").bytes(),
                &part.get("content.text").bytes(),
            ]));
            true
        });
        return out;
    }
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn antigravity_names_round_trip_only_colliding_tools() {
        assert_eq!(antigravity_name_to_upstream(b"read_file"), b"external_read_file");
        assert_eq!(antigravity_name_to_upstream(b"Read_File"), b"Read_File");
        assert_eq!(antigravity_name_to_client(b"external_execute_code"), b"execute_code");
        assert_eq!(antigravity_name_to_client(b"external_other"), b"external_other");
        assert!(is_antigravity("Gemini-AntiGravity-x".as_bytes()));
        assert!(!is_antigravity(b"anti-gravity"));
    }

    #[test]
    fn data_urls_parse_like_go() {
        assert_eq!(
            parse_data_url(b"data: image/png;BASE64,AA"),
            Some((&b" image/png"[..], &b"AA"[..]))
        );
        assert_eq!(
            parse_data_url("data:a/b;ba\u{17f}e64,x".as_bytes()),
            Some((&b"a/b"[..], &b"x"[..]))
        );
        assert_eq!(parse_data_url(b"data:a/b;charset=x;base64,x"), None);
        assert_eq!(parse_data_url(b"data: ;base64,x"), None);
        assert_eq!(parse_data_url(b"data:a/b;base64,"), None);
    }

    #[test]
    fn file_names_follow_the_mime_suffix() {
        assert_eq!(file_name_from_mime(b" Application/PDF "), b"document.pdf");
        assert_eq!(file_name_from_mime(b"image/svg+xml"), b"document.svg.xml");
        assert_eq!(file_name_from_mime(b"Text/X+Y "), b"document.X.Y ");
        assert_eq!(file_name_from_mime(b"text/"), b"document.bin");
        assert_eq!(file_name_from_mime(b""), b"document.bin");
    }
}
