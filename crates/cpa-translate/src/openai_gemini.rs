//! Gemini request -> OpenAI Chat Completions request, and Chat Completions responses
//! back to Gemini (internal/translator/openai/gemini).

use std::collections::{BTreeMap, HashMap};

use cpa_common::json::{self as gj, Kind, Res};
use sha2::{Digest, Sha256};

use crate::common::{go_lower, go_runes, is_gemini_thought_part, trim_space};
use crate::stream::GoStream;
use crate::{Error, Registered, ResponseCtx, gemini};

pub static PAIR: Registered = registered!(
    Gemini -> OpenAI,
    request: |ctx, body| Ok(convert(ctx.model, body, ctx.stream)),
    non_stream: non_stream,
    go_stream: go_stream,
    token_count: Some(gemini::token_count),
);

fn first_of<'a>(value: &Res<'a>, paths: [&str; 2]) -> Res<'a> {
    let first = value.get(paths[0]);
    if first.exists() { first } else { value.get(paths[1]) }
}

fn audio_format(mime: &[u8]) -> &'static str {
    match go_lower(trim_space(mime)).as_slice() {
        b"audio/wav" | b"audio/wave" | b"audio/x-wav" => "wav",
        b"audio/flac" => "flac",
        b"audio/opus" | b"audio/ogg" => "opus",
        b"audio/pcm" | b"audio/l16" => "pcm16",
        _ => "mp3",
    }
}

fn file_name(mime: &[u8]) -> &'static str {
    let lower = go_lower(trim_space(mime));
    match lower.as_slice() {
        b"application/pdf" => "document.pdf",
        b"text/plain" => "document.txt",
        b"text/csv" => "document.csv",
        b"application/json" => "document.json",
        b"application/xml" | b"text/xml" => "document.xml",
        _ if lower.starts_with(b"video/") => "video",
        _ => "document",
    }
}

fn url_part(kind: &str, url: &[u8]) -> Vec<u8> {
    let mut part = format!(r#"{{"type":"{kind}","{kind}":{{"url":""}}}}"#).into_bytes();
    gj::set_str(&mut part, &format!("{kind}.url"), url);
    part
}

/// openAIContentPartFromGeminiInlineData.
fn inline_data_part(part: &Res<'_>) -> Option<Vec<u8>> {
    let inline = first_of(part, ["inlineData", "inline_data"]);
    if !inline.exists() {
        return None;
    }
    let mut mime = inline.get("mimeType").bytes().into_owned();
    if mime.is_empty() {
        mime = inline.get("mime_type").bytes().into_owned();
    }
    if mime.is_empty() {
        mime = b"application/octet-stream".to_vec();
    }
    let data = inline.get("data").bytes().into_owned();
    if data.is_empty() {
        return None;
    }
    let data_url = [b"data:", &mime[..], b";base64,", &data[..]].concat();
    let lower = go_lower(&mime);
    Some(if lower.starts_with(b"image/") {
        url_part("image_url", &data_url)
    } else if lower.starts_with(b"audio/") {
        let mut out = br#"{"type":"input_audio","input_audio":{"data":"","format":""}}"#.to_vec();
        gj::set_str(&mut out, "input_audio.data", &data);
        gj::set_str(&mut out, "input_audio.format", audio_format(&mime));
        out
    } else if lower.starts_with(b"video/") {
        url_part("video_url", &data_url)
    } else {
        let mut out = br#"{"type":"file","file":{"filename":"","file_data":""}}"#.to_vec();
        gj::set_str(&mut out, "file.filename", file_name(&mime));
        gj::set_str(&mut out, "file.file_data", &data);
        out
    })
}

/// openAIContentPartFromGeminiFileData.
fn file_data_part(part: &Res<'_>) -> Option<Vec<u8>> {
    let file = first_of(part, ["fileData", "file_data"]);
    if !file.exists() {
        return None;
    }
    let mut uri = file.get("fileUri").bytes().into_owned();
    if uri.is_empty() {
        uri = file.get("file_uri").bytes().into_owned();
    }
    if uri.is_empty() {
        return None;
    }
    let mut mime = file.get("mimeType").bytes().into_owned();
    if mime.is_empty() {
        mime = file.get("mime_type").bytes().into_owned();
    }
    let lower = go_lower(&mime);
    if lower.starts_with(b"image/") {
        return Some(url_part("image_url", &uri));
    }
    if lower.starts_with(b"video/") {
        return Some(url_part("video_url", &uri));
    }
    if lower.starts_with(b"application/") || lower.starts_with(b"text/") {
        let mut out = br#"{"type":"file","file":{"filename":"","file_url":""}}"#.to_vec();
        gj::set_str(&mut out, "file.filename", file_name(&mime));
        gj::set_str(&mut out, "file.file_url", &uri);
        return Some(out);
    }
    let mut info = [&b"File: "[..], &uri].concat();
    if !mime.is_empty() {
        info.extend_from_slice(b" (Type: ");
        info.extend_from_slice(&mime);
        info.push(b')');
    }
    let mut out = br#"{"type":"text","text":""}"#.to_vec();
    gj::set_str(&mut out, "text", &info);
    Some(out)
}

/// deterministicToolCallID: `call_` + 12 bytes of sha256 over the call's position and
/// payload.
fn deterministic_id(kind: &str, msg: i64, part: i64, name: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(format!("{kind}|{msg}|{part}|").as_bytes());
    hasher.update(name);
    hasher.update(b"|");
    hasher.update(payload);
    format!("call_{}", crate::common::hex(&hasher.finalize()[..12])).into_bytes()
}

/// explicitGeminiToolID.
fn explicit_id(node: &Res<'_>) -> Vec<u8> {
    ["id", "call_id", "callId"]
        .iter()
        .map(|k| trim_space(&node.get(*k).bytes()).to_vec())
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

/// ConvertGeminiRequestToOpenAI.
fn convert(model: &str, raw: &[u8], stream: bool) -> Vec<u8> {
    let mut out = br#"{"model":"","messages":[]}"#.to_vec();
    let root = gj::parse(raw);
    gj::set_str(&mut out, "model", model);

    let config = root.get("generationConfig");
    if config.exists() {
        let temperature = config.get("temperature");
        if temperature.exists() {
            gj::set_f64(&mut out, "temperature", temperature.float());
        }
        let max_tokens = config.get("maxOutputTokens");
        if max_tokens.exists() {
            gj::set_int(&mut out, "max_tokens", max_tokens.int());
        }
        let top_p = config.get("topP");
        if top_p.exists() {
            gj::set_f64(&mut out, "top_p", top_p.float());
        }
        let top_k = config.get("topK");
        if top_k.exists() {
            gj::set_int(&mut out, "top_k", top_k.int());
        }
        let stops = config.get("stopSequences");
        if stops.is_array() {
            let stops: Vec<Vec<u8>> = stops.array().iter().map(|s| s.bytes().into_owned()).collect();
            if !stops.is_empty() {
                gj::set_strs(&mut out, "stop", &stops);
            }
        }
        let candidates = config.get("candidateCount");
        if candidates.exists() {
            gj::set_int(&mut out, "n", candidates.int());
        }
        let modalities = config.get("responseModalities");
        if modalities.is_array() {
            let list: Vec<&str> = modalities
                .array()
                .iter()
                .filter_map(|m| match go_lower(trim_space(&m.bytes())).as_slice() {
                    b"text" => Some("text"),
                    b"image" => Some("image"),
                    b"audio" => Some("audio"),
                    _ => None,
                })
                .collect();
            if !list.is_empty() {
                gj::set_strs(&mut out, "modalities", &list);
            }
        }
        let thinking = config.get("thinkingConfig");
        if thinking.is_object() {
            let level = first_of(&thinking, ["thinkingLevel", "thinking_level"]);
            if level.exists() {
                let effort = go_lower(trim_space(&level.bytes()));
                if !effort.is_empty() {
                    gj::set_str(&mut out, "reasoning_effort", &effort);
                }
            } else {
                let budget = first_of(&thinking, ["thinkingBudget", "thinking_budget"]);
                if budget.exists()
                    && let Some(level) = cpa_common::thinking::convert_budget_to_level(budget.int())
                {
                    gj::set_str(&mut out, "reasoning_effort", level);
                }
            }
        }
    }
    gj::set_bool(&mut out, "stream", stream);
    let tier = root.get("service_tier");
    if tier.kind == Kind::String {
        gj::set_str(&mut out, "service_tier", &tier.s);
    }

    let mut messages: Vec<Vec<u8>> = vec![];
    let mut ids_by_name: HashMap<Vec<u8>, Vec<Vec<u8>>> = HashMap::new();
    let system = first_of(&root, ["systemInstruction", "system_instruction"]);
    if system.exists() {
        let mut items = vec![];
        let parts = system.get("parts");
        if parts.is_array() {
            parts.each(|_, part| {
                if is_gemini_thought_part(&part) {
                    return true;
                }
                let text = part.get("text");
                if text.exists() {
                    let mut item = br#"{"type":"text","text":""}"#.to_vec();
                    gj::set_str(&mut item, "text", text.bytes());
                    items.push(item);
                }
                items.extend(inline_data_part(&part));
                items.extend(file_data_part(&part));
                true
            });
        }
        if !items.is_empty() {
            let mut message = br#"{"role":"system","content":[]}"#.to_vec();
            gj::set_raw(&mut message, "content", gj::join(&items));
            messages.push(message);
        }
    }

    let contents = root.get("contents");
    if contents.is_array() {
        for (msg_index, content) in contents.array().iter().enumerate() {
            let msg_index = msg_index as i64;
            let mut role = content.get("role").bytes().into_owned();
            if role == b"model" {
                role = b"assistant".to_vec();
            }
            let mut message = br#"{"role":"","content":""}"#.to_vec();
            gj::set_str(&mut message, "role", &role);
            let mut text_only = true;
            let mut text = vec![];
            let mut items: Vec<Vec<u8>> = vec![];
            let mut calls: Vec<Vec<u8>> = vec![];
            let mut dropped_thought = false;
            let parts = content.get("parts");
            if parts.is_array() {
                for (part_index, part) in parts.array().iter().enumerate() {
                    let part_index = part_index as i64;
                    if is_gemini_thought_part(part) {
                        dropped_thought = true;
                        continue;
                    }
                    let part_text = part.get("text");
                    if part_text.exists() {
                        let value = part_text.bytes();
                        text.extend_from_slice(&value);
                        let mut item = br#"{"type":"text","text":""}"#.to_vec();
                        gj::set_str(&mut item, "text", &value);
                        items.push(item);
                    }
                    for media in [inline_data_part(part), file_data_part(part)].into_iter().flatten() {
                        text_only = false;
                        items.push(media);
                    }
                    let call = part.get("functionCall");
                    if call.exists() {
                        let name = call.get("name").bytes().into_owned();
                        let args = call.get("args");
                        let args_raw = if args.exists() { args.raw.to_vec() } else { vec![] };
                        let mut id = explicit_id(&call);
                        if id.is_empty() {
                            id = deterministic_id("call", msg_index, part_index, &name, &args_raw);
                        }
                        ids_by_name.entry(name.clone()).or_default().push(id.clone());
                        let mut tool_call =
                            br#"{"id":"","type":"function","function":{"name":"","arguments":""}}"#.to_vec();
                        gj::set_str(&mut tool_call, "id", &id);
                        gj::set_str(&mut tool_call, "function.name", &name);
                        let arguments: &[u8] = if args_raw.is_empty() { b"{}" } else { &args_raw };
                        gj::set_str(&mut tool_call, "function.arguments", arguments);
                        calls.push(tool_call);
                    }
                    let response = part.get("functionResponse");
                    if response.exists() {
                        let name = response.get("name").bytes().into_owned();
                        let mut tool = br#"{"role":"tool","tool_call_id":"","content":""}"#.to_vec();
                        let mut response_raw = vec![];
                        let body = response.get("response");
                        if body.exists() {
                            let content_field = body.get("content");
                            response_raw = if content_field.exists() {
                                content_field.raw.to_vec()
                            } else {
                                body.raw.to_vec()
                            };
                            gj::set_str(&mut tool, "content", &response_raw);
                        }
                        let explicit = explicit_id(&response);
                        let queue = ids_by_name.entry(name.clone()).or_default();
                        if !explicit.is_empty() {
                            gj::set_str(&mut tool, "tool_call_id", &explicit);
                            if let Some(position) = queue.iter().position(|id| *id == explicit) {
                                queue.remove(position);
                            }
                        } else if !queue.is_empty() {
                            let id = queue.remove(0);
                            gj::set_str(&mut tool, "tool_call_id", &id);
                        } else {
                            let id = deterministic_id("response", msg_index, part_index, &name, &response_raw);
                            gj::set_str(&mut tool, "tool_call_id", &id);
                        }
                        messages.push(tool);
                    }
                }
            }
            if !items.is_empty() {
                if text_only {
                    gj::set_str(&mut message, "content", &text);
                } else {
                    gj::set_raw(&mut message, "content", gj::join(&items));
                }
            }
            if !calls.is_empty() {
                gj::set_raw(&mut message, "tool_calls", gj::join(&calls));
            }
            if dropped_thought && items.is_empty() && calls.is_empty() {
                continue;
            }
            messages.push(message);
        }
    }
    gj::set_items(&mut out, "messages", &messages);

    let tools = root.get("tools");
    if tools.is_array() {
        let mut items = vec![];
        tools.each(|_, tool| {
            let declarations = tool.get("functionDeclarations");
            if declarations.is_array() {
                declarations.each(|_, declaration| {
                    let mut item = br#"{"type":"function","function":{"name":"","description":""}}"#.to_vec();
                    gj::set_str(&mut item, "function.name", declaration.get("name").bytes());
                    gj::set_str(
                        &mut item,
                        "function.description",
                        declaration.get("description").bytes(),
                    );
                    let parameters = first_of(&declaration, ["parameters", "parametersJsonSchema"]);
                    if parameters.exists() {
                        gj::set_raw(&mut item, "function.parameters", &parameters.raw);
                    }
                    items.push(item);
                    true
                });
            }
            true
        });
        if !items.is_empty() {
            gj::set_raw(&mut out, "tools", gj::join(&items));
        }
    }

    let calling = root.get("toolConfig.functionCallingConfig");
    if root.get("toolConfig").exists() && calling.exists() {
        match calling.get("mode").bytes().as_ref() {
            b"NONE" => {
                gj::set_str(&mut out, "tool_choice", "none");
            }
            b"AUTO" => {
                gj::set_str(&mut out, "tool_choice", "auto");
            }
            b"ANY" => {
                let allowed = calling.get("allowedFunctionNames");
                let names = allowed.array();
                if allowed.is_array() && names.len() == 1 {
                    let mut choice = br#"{"type":"function","function":{"name":""}}"#.to_vec();
                    gj::set_str(&mut choice, "function.name", names[0].bytes());
                    gj::set_raw(&mut out, "tool_choice", choice);
                } else {
                    gj::set_str(&mut out, "tool_choice", "required");
                }
            }
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------------------
// Responses (openai_gemini_response.go)

/// mapOpenAIFinishReasonToGemini.
fn finish_reason(reason: &[u8]) -> &'static str {
    match reason {
        b"length" => "MAX_TOKENS",
        b"content_filter" => "SAFETY",
        _ => "STOP",
    }
}

/// extractReasoningTexts: strings, `text` fields and other scalars, flattened.
fn reasoning_texts(node: &Res<'_>, out: &mut Vec<Vec<u8>>) {
    if !node.exists() {
        return;
    }
    if node.is_array() {
        node.each(|_, value| {
            reasoning_texts(&value, out);
            true
        });
        return;
    }
    match node.kind {
        Kind::String => out.push(node.s.to_vec()),
        Kind::Json => {
            let text = node.get("text");
            let raw = trim_space(&node.raw);
            if text.exists() {
                out.push(text.bytes().into_owned());
            } else if !raw.is_empty() && !raw.starts_with(b"{") && !raw.starts_with(b"[") {
                out.push(raw.to_vec());
            }
        }
        _ => {}
    }
}

fn usage_count(usage: &Res<'_>, paths: &[&str]) -> Option<i64> {
    paths.iter().map(|p| usage.get(*p)).find(Res::exists).map(|v| v.int())
}

/// setGeminiUsageMetadataFromOpenAIUsage.
fn set_usage(out: &mut Vec<u8>, usage: &Res<'_>) {
    let prompt = usage_count(usage, &["prompt_tokens", "input_tokens"]);
    let completion = usage_count(usage, &["completion_tokens", "output_tokens"]);
    let total = usage_count(usage, &["total_tokens"]);
    if let Some(prompt) = prompt {
        gj::set_int(out, "usageMetadata.promptTokenCount", prompt);
    }
    if let Some(completion) = completion {
        gj::set_int(out, "usageMetadata.candidatesTokenCount", completion);
    }
    if let Some(total) = total {
        gj::set_int(out, "usageMetadata.totalTokenCount", total);
    } else if prompt.is_some() || completion.is_some() {
        gj::set_int(
            out,
            "usageMetadata.totalTokenCount",
            prompt.unwrap_or(0).wrapping_add(completion.unwrap_or(0)),
        );
    }
    let reasoning = usage_count(
        usage,
        &[
            "completion_tokens_details.reasoning_tokens",
            "output_tokens_details.reasoning_tokens",
        ],
    )
    .unwrap_or(0);
    if reasoning > 0 {
        gj::set_int(out, "usageMetadata.thoughtsTokenCount", reasoning);
    }
    let cached = usage_count(
        usage,
        &[
            "prompt_tokens_details.cached_tokens",
            "input_tokens_details.cached_tokens",
        ],
    )
    .unwrap_or(0);
    if cached > 0 {
        gj::set_int(out, "usageMetadata.cachedContentTokenCount", cached);
    }
}

#[derive(Default)]
struct Accumulator {
    id: Vec<u8>,
    name: Vec<u8>,
    arguments: Vec<u8>,
}

#[derive(Default)]
struct State {
    /// Tool calls by index.
    // ponytail: Go keeps these in a map and emits them in map order (random for two or
    // more calls); this emits them by tool index.
    calls: BTreeMap<i64, Accumulator>,
}

pub fn go_stream(_: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    Box::new(State::default())
}

impl GoStream for State {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        if trim_space(line) == b"[DONE]" {
            return Ok(vec![]);
        }
        let mut raw = line;
        if let Some(rest) = raw.strip_prefix(b"data:") {
            raw = trim_space(rest);
        }
        let root = gj::parse(raw);
        let choices = root.get("choices");
        if !choices.is_array() {
            return Ok(vec![]);
        }
        let model = root.get("model");
        if choices.array().is_empty() {
            let usage = root.get("usage");
            if !usage.exists() {
                return Ok(vec![]);
            }
            let mut out = br#"{"candidates":[],"usageMetadata":{}}"#.to_vec();
            if model.exists() {
                gj::set_str(&mut out, "model", model.bytes());
            }
            set_usage(&mut out, &usage);
            return Ok(vec![out]);
        }
        let mut results = vec![];
        for choice in choices.array() {
            let mut template = br#"{"candidates":[{"content":{"parts":[],"role":"model"},"index":0}]}"#.to_vec();
            if model.exists() {
                gj::set_str(&mut template, "model", model.bytes());
            }
            let delta = choice.get("delta");
            let mut chunks = vec![];
            let mut texts = vec![];
            reasoning_texts(&delta.get("reasoning_content"), &mut texts);
            for text in texts.iter().filter(|t| !t.is_empty()) {
                let mut chunk = template.clone();
                gj::set_bool(&mut chunk, "candidates.0.content.parts.0.thought", true);
                gj::set_str(&mut chunk, "candidates.0.content.parts.0.text", text);
                chunks.push(chunk);
            }
            let content = delta.get("content");
            if content.exists() && !content.bytes().is_empty() {
                let mut chunk = template.clone();
                gj::set_str(&mut chunk, "candidates.0.content.parts.0.text", content.bytes());
                chunks.push(chunk);
            }
            if !chunks.is_empty() {
                results.extend(chunks);
                continue;
            }
            let calls = delta.get("tool_calls");
            if calls.is_array() {
                for call in calls.array() {
                    let kind = call.get("type").bytes();
                    let function = call.get("function");
                    if (!kind.is_empty() && kind.as_ref() != b"function") || !function.exists() {
                        continue;
                    }
                    let id = call.get("id").bytes().into_owned();
                    let name = function.get("name").bytes().into_owned();
                    let accumulator = self
                        .calls
                        .entry(call.get("index").int())
                        .or_insert_with(|| Accumulator {
                            id: id.clone(),
                            name: name.clone(),
                            arguments: vec![],
                        });
                    if !id.is_empty() {
                        accumulator.id = id;
                    }
                    if !name.is_empty() {
                        accumulator.name = name;
                    }
                    accumulator
                        .arguments
                        .extend_from_slice(&function.get("arguments").bytes());
                }
                continue;
            }
            let finish = choice.get("finish_reason");
            if finish.kind == Kind::String && !finish.s.is_empty() {
                gj::set_str(&mut template, "candidates.0.finishReason", finish_reason(&finish.s));
                for (part, accumulator) in std::mem::take(&mut self.calls).into_values().enumerate() {
                    let at = |field: &str| format!("candidates.0.content.parts.{part}.functionCall.{field}");
                    if !accumulator.id.is_empty() {
                        gj::set_str(&mut template, &at("id"), &accumulator.id);
                    }
                    gj::set_str(&mut template, &at("name"), &accumulator.name);
                    gj::set_raw(&mut template, &at("args"), args_object(&accumulator.arguments));
                }
                results.push(template);
                continue;
            }
            let usage = root.get("usage");
            if usage.exists() {
                set_usage(&mut template, &usage);
                results.push(template);
            }
        }
        Ok(results)
    }
}

/// ensurePart: the part at `index`, padding with empty objects.
fn part_at(parts: &mut Vec<Vec<u8>>, index: usize) -> Vec<u8> {
    while parts.len() <= index {
        parts.push(b"{}".to_vec());
    }
    parts[index].clone()
}

/// ConvertOpenAIResponseToGeminiNonStream: parts of every choice overlay one candidate.
pub fn non_stream(_: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let root = gj::parse(body);
    let mut out = br#"{"candidates":[{"content":{"parts":[],"role":"model"},"index":0}]}"#.to_vec();
    let model = root.get("model");
    if model.exists() {
        gj::set_str(&mut out, "model", model.bytes());
    }
    let choices = root.get("choices");
    if choices.is_array() {
        let mut parts: Vec<Vec<u8>> = vec![];
        for choice in choices.array() {
            let choice_index = choice.get("index").int();
            let message = choice.get("message");
            let role = message.get("role");
            if role.exists() && role.bytes().as_ref() == b"assistant" {
                gj::set_str(&mut out, "candidates.0.content.role", "model");
            }
            let mut index = 0;
            let mut texts = vec![];
            reasoning_texts(&message.get("reasoning_content"), &mut texts);
            for text in texts.iter().filter(|t| !t.is_empty()) {
                let mut part = part_at(&mut parts, index);
                gj::set_bool(&mut part, "thought", true);
                gj::set_str(&mut part, "text", text);
                parts[index] = part;
                index += 1;
            }
            let content = message.get("content");
            if content.exists() && !content.bytes().is_empty() {
                let mut part = part_at(&mut parts, index);
                gj::set_str(&mut part, "text", content.bytes());
                parts[index] = part;
                index += 1;
            }
            let calls = message.get("tool_calls");
            if calls.is_array() {
                for call in calls.array() {
                    if call.get("type").bytes().as_ref() != b"function" {
                        continue;
                    }
                    let function = call.get("function");
                    let mut part = part_at(&mut parts, index);
                    let id = call.get("id").bytes();
                    if !id.is_empty() {
                        gj::set_str(&mut part, "functionCall.id", &id);
                    }
                    gj::set_str(&mut part, "functionCall.name", function.get("name").bytes());
                    gj::set_raw(
                        &mut part,
                        "functionCall.args",
                        args_object(&function.get("arguments").bytes()),
                    );
                    parts[index] = part;
                    index += 1;
                }
            }
            let finish = choice.get("finish_reason");
            if finish.kind == Kind::String && !finish.s.is_empty() {
                gj::set_str(&mut out, "candidates.0.finishReason", finish_reason(&finish.s));
            }
            gj::set_int(&mut out, "candidates.0.index", choice_index);
        }
        if !parts.is_empty() {
            gj::set_raw(&mut out, "candidates.0.content.parts", gj::join(&parts));
        }
    }
    let usage = root.get("usage");
    if usage.exists() {
        set_usage(&mut out, &usage);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// Tolerant argument parsing (parseArgsToObjectRaw)

/// parseArgsToObjectRaw: a JSON object for function call arguments, recovering what it
/// can from malformed text.
fn args_object(arguments: &[u8]) -> Vec<u8> {
    let trimmed = trim_space(arguments);
    if trimmed.is_empty() || trimmed == b"{}" {
        return b"{}".to_vec();
    }
    if gj::valid(trimmed) {
        let parsed = gj::parse(trimmed);
        if parsed.is_object() {
            return parsed.raw.to_vec();
        }
    }
    tolerant_object(trimmed)
}

/// escapeSjsonPathKey.
fn escape_key(key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(key.len());
    for &c in key {
        if c == b'\\' || c == b'.' {
            out.push(b'\\');
        }
        out.push(c);
    }
    out
}

fn encode(runes: &[char]) -> Vec<u8> {
    runes.iter().collect::<String>().into_bytes()
}

fn json_space(c: char) -> bool {
    matches!(c, ' ' | '\n' | '\r' | '\t')
}

/// parseJSONStringRunes: the quoted token at `start` and the index after it (`None`:
/// unterminated).
fn string_token(runes: &[char], start: usize) -> (Vec<char>, Option<usize>) {
    let mut escaped = false;
    let mut i = start + 1;
    while i < runes.len() {
        let r = runes[i];
        if r == '\\' && !escaped {
            escaped = true;
            i += 1;
            continue;
        }
        if r == '"' && !escaped {
            return (runes[start..=i].to_vec(), Some(i + 1));
        }
        escaped = false;
        i += 1;
    }
    (runes[start..].to_vec(), None)
}

/// jsonStringTokenToRawString.
fn token_string(token: &[char]) -> Vec<u8> {
    let raw = encode(token);
    let parsed = gj::parse(&raw);
    if parsed.kind == Kind::String {
        return parsed.s.to_vec();
    }
    if raw.len() >= 2 && raw[0] == b'"' && raw[raw.len() - 1] == b'"' {
        return raw[1..raw.len() - 1].to_vec();
    }
    raw
}

/// captureBracketed: the bracketed segment at `start` (`None`: unterminated).
fn bracketed(runes: &[char], start: usize) -> Option<(Vec<u8>, usize)> {
    let open = runes[start];
    let close = if open == '{' { '}' } else { ']' };
    let (mut depth, mut in_string, mut escaped) = (0, false, false);
    let mut j = start;
    while j < runes.len() {
        let r = runes[j];
        if in_string {
            if r == '\\' && !escaped {
                escaped = true;
            } else {
                if r == '"' && !escaped {
                    in_string = false;
                }
                escaped = false;
            }
        } else if r == '"' {
            in_string = true;
        } else if r == open {
            depth += 1;
        } else if r == close {
            depth -= 1;
            if depth == 0 {
                return Some((encode(&runes[start..=j]), j + 1));
            }
        }
        j += 1;
    }
    None
}

/// tryParseNumber + sjson.SetBytes of the parsed int64, uint64 or float64.
fn set_number(out: &mut Vec<u8>, path: &[u8], token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    if let Ok(n) = token.parse::<i64>() {
        gj::set_int(out, path, n);
        return true;
    }
    if !token.starts_with('+')
        && let Ok(n) = token.parse::<u64>()
    {
        gj::set_raw(out, path, n.to_string());
        return true;
    }
    match gj::go_parse_float(token.as_bytes()) {
        Ok(f) => {
            gj::set_f64(out, path, f);
            true
        }
        Err(_) => false,
    }
}

/// tolerantParseJSONObjectRaw: key/value pairs recovered from the text between the
/// first `{` and the last `}`.
fn tolerant_object(s: &[u8]) -> Vec<u8> {
    let start = s.iter().position(|&c| c == b'{');
    let end = s.iter().rposition(|&c| c == b'}');
    let (Some(start), Some(end)) = (start, end) else {
        return b"{}".to_vec();
    };
    if start >= end {
        return b"{}".to_vec();
    }
    let runes: Vec<char> = go_runes(&s[start + 1..end]).collect();
    let n = runes.len();
    let mut i = 0;
    let mut result = b"{}".to_vec();
    while i < n {
        while i < n && (json_space(runes[i]) || runes[i] == ',') {
            i += 1;
        }
        if i >= n {
            break;
        }
        if runes[i] != '"' {
            while i < n && runes[i] != ',' {
                i += 1;
            }
            continue;
        }
        let (key_token, next) = string_token(&runes, i);
        let Some(next) = next else {
            break;
        };
        let key = escape_key(&token_string(&key_token));
        i = next;
        while i < n && json_space(runes[i]) {
            i += 1;
        }
        if i >= n || runes[i] != ':' {
            break;
        }
        i += 1;
        while i < n && json_space(runes[i]) {
            i += 1;
        }
        if i >= n {
            break;
        }
        match runes[i] {
            '"' => {
                let (token, next) = string_token(&runes, i);
                match next {
                    None => {
                        gj::set_str(&mut result, &key, "");
                        i = n;
                    }
                    Some(next) => {
                        gj::set_str(&mut result, &key, token_string(&token));
                        i = next;
                    }
                }
            }
            '{' | '[' => match bracketed(&runes, i) {
                None => i = n,
                Some((segment, next)) => {
                    if gj::valid(&segment) {
                        gj::set_raw(&mut result, &key, &segment);
                    } else {
                        gj::set_str(&mut result, &key, &segment);
                    }
                    i = next;
                }
            },
            _ => {
                let mut j = i;
                while j < n && runes[j] != ',' {
                    j += 1;
                }
                let raw: String = runes[i..j].iter().collect();
                let token = raw.trim_matches(|c: char| c.is_whitespace());
                match token {
                    "true" => {
                        gj::set_bool(&mut result, &key, true);
                    }
                    "false" => {
                        gj::set_bool(&mut result, &key, false);
                    }
                    "null" => {
                        gj::set_raw(&mut result, &key, b"null");
                    }
                    _ => {
                        if !set_number(&mut result, &key, token) {
                            gj::set_str(&mut result, &key, token);
                        }
                    }
                }
                i = j;
            }
        }
        while i < n && json_space(runes[i]) {
            i += 1;
        }
        if i < n && runes[i] == ',' {
            i += 1;
        }
    }
    result
}
