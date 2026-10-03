//! Claude Messages client, Gemini Interactions upstream (internal/translator/interactions/claude):
//! ConvertClaudeRequestToInteractions (and its WithCompat variant),
//! ConvertInteractionsResponseToClaude and its NonStream.

use std::collections::HashMap;

use cpa_common::json::{self as gj, Kind, Res};

use crate::common::{align_claude_tool_results, claude_message_system_reminder_text, go_lower, now_nanos, trim_space};
use crate::gemini_interactions::first_existing;
use crate::openai_interactions::first_nonblank;
use crate::stream::GoStream;
use crate::{Error, Registered, RequestCtx, ResponseCtx};

pub static PAIR: Registered = registered!(
    Claude -> Interactions,
    request: |ctx, body| Ok(convert(ctx.model, body, ctx.stream, false)),
    non_stream: non_stream,
    go_stream: go_stream,
    token_count: None,
);

/// ConvertClaudeRequestToInteractionsWithCompat: empty thinking blocks are kept.
pub fn request_with_compat(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    Ok(convert(ctx.model, body, ctx.stream, true))
}

fn string(res: &Res<'_>) -> Vec<u8> {
    res.bytes().into_owned()
}

// ---------------------------------------------------------------------------------------
// Request

/// convertClaudeRequestToInteractions.
fn convert(model: &str, raw: &[u8], stream: bool, preserve_empty_thinking: bool) -> Vec<u8> {
    let root = gj::parse(raw);
    let mut out = br#"{"model":"","input":[]}"#.to_vec();
    gj::set_str(
        &mut out,
        "model",
        first_nonblank(&[model.as_bytes(), &root.get("model").bytes()]),
    );
    let stream_value = root.get("stream");
    if stream_value.exists() {
        gj::set_bool(&mut out, "stream", stream_value.bool());
    } else if stream {
        gj::set_bool(&mut out, "stream", true);
    }
    let system = claude_text(&root.get("system"));
    if !system.is_empty() {
        gj::set_str(&mut out, "system_instruction", &system);
    }
    for (from, to) in [
        ("max_tokens", "generation_config.max_output_tokens"),
        ("temperature", "generation_config.temperature"),
        ("top_p", "generation_config.top_p"),
        ("stop_sequences", "generation_config.stop_sequences"),
    ] {
        let value = root.get(from);
        if value.exists() {
            gj::set_raw(&mut out, to, &value.raw);
        }
    }
    copy_thinking(&mut out, &root);
    copy_tool_choice(&mut out, &root.get("tool_choice"));
    append_messages(&mut out, &root.get("messages"), preserve_empty_thinking);
    copy_tools(&mut out, &root);
    out
}

/// copyClaudeThinkingToInteractions.
fn copy_thinking(out: &mut Vec<u8>, root: &Res<'_>) {
    let thinking = root.get("thinking");
    if thinking.exists() {
        match go_lower(trim_space(&thinking.get("type").bytes())).as_slice() {
            b"disabled" => {
                gj::set_str(out, "generation_config.thinking_level", "none");
            }
            b"enabled" => {
                let budget = thinking.get("budget_tokens");
                if budget.exists() {
                    gj::set_raw(out, "generation_config.thinking_config.thinking_budget", &budget.raw);
                } else {
                    gj::set_str(out, "generation_config.thinking_level", "high");
                }
            }
            b"adaptive" => {
                gj::set_str(out, "generation_config.thinking_level", "auto");
            }
            _ => {}
        }
    }
    let effort = root.get("output_config.effort");
    if effort.kind == Kind::String {
        gj::set_str(
            out,
            "generation_config.thinking_level",
            go_lower(trim_space(&effort.bytes())),
        );
    }
}

/// copyClaudeToolChoiceToInteractions.
fn copy_tool_choice(out: &mut Vec<u8>, choice: &Res<'_>) {
    let kind = match choice.kind {
        Kind::String => go_lower(trim_space(&choice.bytes())),
        Kind::Json => go_lower(trim_space(&choice.get("type").bytes())),
        _ => return,
    };
    match kind.as_slice() {
        b"auto" => {
            gj::set_str(out, "generation_config.tool_choice", "auto");
        }
        b"any" | b"required" => {
            gj::set_str(out, "generation_config.tool_choice", "required");
        }
        b"tool" if choice.kind == Kind::Json => {
            let name = trim_space(&choice.get("name").bytes()).to_vec();
            if !name.is_empty() {
                let mut tool = br#"{"type":"function","name":""}"#.to_vec();
                gj::set_str(&mut tool, "name", &name);
                gj::set_raw(out, "generation_config.tool_choice", tool);
            }
        }
        _ => {}
    }
}

/// The input being built from Claude messages, with system reminders held back while
/// tool results are pending.
#[derive(Default)]
struct Input {
    items: Vec<Vec<u8>>,
    pending_tool_uses: Vec<Vec<u8>>,
    pending_reminders: Vec<Vec<u8>>,
    names: HashMap<Vec<u8>, Vec<u8>>,
    preserve_empty_thinking: bool,
}

impl Input {
    fn release_reminders(&mut self) {
        self.items.append(&mut self.pending_reminders);
    }

    /// appendClaudeMessageToInteractions.
    fn message(&mut self, role: &[u8], content: &Res<'_>, parts: Option<Vec<Res<'_>>>) {
        let step_type = if role == b"assistant" {
            "model_output"
        } else {
            "user_input"
        };
        if content.kind == Kind::String {
            self.release_reminders();
            let mut step = br#"{"type":"","content":[{"type":"text","text":""}]}"#.to_vec();
            gj::set_str(&mut step, "type", step_type);
            gj::set_str(&mut step, "content.0.text", content.bytes());
            self.items.push(step);
            return;
        }
        let Some(parts) = parts else {
            return;
        };
        let mut pending: Vec<Vec<u8>> = vec![];
        let flush = |items: &mut Vec<Vec<u8>>, pending: &mut Vec<Vec<u8>>| {
            if pending.is_empty() {
                return;
            }
            let mut step = br#"{"type":"","content":[]}"#.to_vec();
            gj::set_str(&mut step, "type", step_type);
            gj::set_raw(&mut step, "content", gj::join(pending));
            items.push(step);
            pending.clear();
        };
        for part in &parts {
            let kind = go_lower(trim_space(&part.get("type").bytes()));
            match kind.as_slice() {
                b"text" => {
                    let text = part.get("text").bytes();
                    if !text.is_empty() {
                        if !self.pending_reminders.is_empty() {
                            flush(&mut self.items, &mut pending);
                            self.release_reminders();
                        }
                        let mut item = br#"{"type":"text","text":""}"#.to_vec();
                        gj::set_str(&mut item, "text", &text);
                        pending.push(item);
                    }
                }
                b"thinking" => {
                    flush(&mut self.items, &mut pending);
                    let text = part.get("thinking").bytes();
                    if !text.is_empty() || self.preserve_empty_thinking {
                        let mut step = br#"{"type":"thought","content":[{"type":"text","text":""}]}"#.to_vec();
                        gj::set_str(&mut step, "content.0.text", &text);
                        self.items.push(step);
                    }
                }
                b"image" | b"document" => {
                    if let Some(media) = media_part(part, &kind) {
                        if !self.pending_reminders.is_empty() {
                            flush(&mut self.items, &mut pending);
                            self.release_reminders();
                        }
                        pending.push(media);
                    }
                }
                b"tool_use" => {
                    flush(&mut self.items, &mut pending);
                    let id = string(&part.get("id"));
                    if !id.is_empty() {
                        self.pending_tool_uses.push(id.clone());
                        let name = string(&part.get("name"));
                        if !name.is_empty() {
                            self.names.insert(id, name);
                        }
                    }
                    let mut step = br#"{"type":"function_call","name":"","arguments":{}}"#.to_vec();
                    gj::set_str(&mut step, "name", part.get("name").bytes());
                    let id = part.get("id").bytes();
                    if !id.is_empty() {
                        gj::set_str(&mut step, "id", &id);
                    }
                    let input = part.get("input");
                    if input.is_object() {
                        gj::set_raw(&mut step, "arguments", &input.raw);
                    }
                    self.items.push(step);
                }
                b"tool_result" => {
                    flush(&mut self.items, &mut pending);
                    let step = tool_result(part, &self.names);
                    self.items.push(step);
                }
                _ => {}
            }
        }
        flush(&mut self.items, &mut pending);
    }
}

/// appendClaudeMessagesToInteractions.
fn append_messages(out: &mut Vec<u8>, messages: &Res<'_>, preserve_empty_thinking: bool) {
    if !messages.is_array() {
        return;
    }
    let mut input = Input {
        preserve_empty_thinking,
        ..Input::default()
    };
    messages.each(|_, message| {
        let role = go_lower(trim_space(&message.get("role").bytes()));
        let content = message.get("content");
        if role == b"system" {
            if let Some(reminder) = claude_message_system_reminder_text(&content) {
                let mut step = br#"{"type":"user_input","content":[{"type":"text","text":""}]}"#.to_vec();
                gj::set_str(&mut step, "content.0.text", &reminder);
                if input.pending_tool_uses.is_empty() {
                    input.items.push(step);
                } else {
                    input.pending_reminders.push(step);
                }
            }
            return true;
        }
        let parts = content.is_array().then(|| {
            let parts = content.array();
            if role == b"user" && !input.pending_tool_uses.is_empty() {
                align_claude_tool_results(parts, &input.pending_tool_uses)
            } else {
                parts
            }
        });
        input.pending_tool_uses.clear();
        input.message(&role, &content, parts);
        input.release_reminders();
        true
    });
    input.release_reminders();
    gj::set_items(out, "input", &input.items);
}

/// claudeMediaPartToInteractions.
fn media_part(part: &Res<'_>, kind: &[u8]) -> Option<Vec<u8>> {
    let source = part.get("source");
    let mut mime = string(&source.get("media_type"));
    let mut data = string(&source.get("data"));
    if data.is_empty() {
        data = string(&part.get("data"));
    }
    if mime.is_empty() {
        mime = string(&part.get("mime_type"));
    }
    if mime.is_empty() || data.is_empty() {
        return None;
    }
    let mut out = br#"{"type":"","mime_type":"","data":""}"#.to_vec();
    gj::set_str(&mut out, "type", kind);
    gj::set_str(&mut out, "mime_type", &mime);
    gj::set_str(&mut out, "data", &data);
    Some(out)
}

/// claudeToolResultToInteractions.
fn tool_result(part: &Res<'_>, names: &HashMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
    let mut step = br#"{"type":"function_result","call_id":"","result":""}"#.to_vec();
    let id = string(&part.get("tool_use_id"));
    if !id.is_empty() {
        gj::set_str(&mut step, "call_id", &id);
    }
    let mut name = string(&part.get("name"));
    if name.is_empty() && !id.is_empty() {
        name = names.get(&id).cloned().unwrap_or_default();
    }
    if !name.is_empty() {
        gj::set_str(&mut step, "name", &name);
    }
    if part.get("is_error").bool() {
        gj::set_bool(&mut step, "is_error", true);
    }
    let result = part.get("content");
    if !result.exists() {
        return step;
    }
    if result.kind == Kind::String {
        gj::set_str(&mut step, "result", result.bytes());
    } else if result.is_array() {
        let mut items: Vec<Vec<u8>> = vec![];
        result.each(|_, item| {
            let raw = trim_space(&item.raw).to_vec();
            let kind = string(&item.get("type"));
            match kind.as_slice() {
                b"text" => {
                    let mut pure = true;
                    if item.is_object() {
                        item.each(|key, _| {
                            pure = matches!(key.bytes().as_ref(), b"type" | b"text" | b"cache_control");
                            pure
                        });
                    }
                    if pure {
                        let mut text = br#"{"type":"text","text":""}"#.to_vec();
                        gj::set_str(&mut text, "text", item.get("text").bytes());
                        items.push(text);
                    } else if !raw.is_empty() {
                        items.push(raw);
                    }
                }
                b"image" | b"document" => match media_part(&item, &kind) {
                    Some(media) => items.push(media),
                    None if !raw.is_empty() => items.push(raw),
                    None => {}
                },
                _ if !raw.is_empty() => items.push(raw),
                _ => {}
            }
            true
        });
        gj::set_raw(&mut step, "result", gj::join(&items));
    } else {
        gj::set_raw(&mut step, "result", &result.raw);
    }
    step
}

/// copyClaudeToolsToInteractions.
fn copy_tools(out: &mut Vec<u8>, root: &Res<'_>) {
    let tools = root.get("tools");
    if !tools.is_array() {
        return;
    }
    let mut items = vec![];
    tools.each(|_, tool| {
        let name = trim_space(&tool.get("name").bytes()).to_vec();
        if name.is_empty() {
            return true;
        }
        let mut item = br#"{"type":"function","name":"","parameters":{}}"#.to_vec();
        gj::set_str(&mut item, "name", &name);
        let description = tool.get("description");
        if description.exists() {
            gj::set_str(&mut item, "description", description.bytes());
        }
        let schema = tool.get("input_schema");
        if schema.is_object() {
            gj::set_raw(&mut item, "parameters", &schema.raw);
        }
        items.push(item);
        true
    });
    if !items.is_empty() {
        gj::set_raw(out, "tools", gj::join(&items));
    }
}

/// claudeText: a string, a `text` field, or the newline-joined texts of an array.
fn claude_text(value: &Res<'_>) -> Vec<u8> {
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
    let mut out = vec![];
    if value.is_array() {
        value.each(|_, item| {
            let text = claude_text(&item);
            if !text.is_empty() {
                if !out.is_empty() {
                    out.push(b'\n');
                }
                out.extend_from_slice(&text);
            }
            true
        });
    }
    out
}

// ---------------------------------------------------------------------------------------
// Responses

/// `event: <event>\ndata: <payload>\n\n\n` (common.AppendSSEEventBytes with three newlines).
fn claude_event(event: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = crate::common::sse_event(event, payload);
    out.push(b'\n');
    out
}

/// interactionsSSEPayload.
fn sse_payload(raw: &[u8]) -> Vec<u8> {
    let trimmed = trim_space(raw);
    if trimmed.is_empty() || trimmed == b"[DONE]" {
        return trimmed.to_vec();
    }
    if let Some(rest) = trimmed.strip_prefix(b"data:") {
        return trim_space(rest).to_vec();
    }
    let lines: Vec<&[u8]> = trimmed
        .split(|&c| c == b'\n')
        .filter_map(|line| trim_space(line).strip_prefix(b"data:").map(trim_space))
        .collect();
    if lines.is_empty() {
        trimmed.to_vec()
    } else {
        lines.join(&b'\n')
    }
}

/// translatorcommon.InteractionsUsage.
fn interactions_usage<'a>(root: &Res<'a>) -> Res<'a> {
    first_existing(
        root,
        &[
            "interaction.usage",
            "usage",
            "metadata.total_usage",
            "metadata.usage",
            "interaction.metadata.total_usage",
            "interaction.metadata.usage",
        ],
    )
}

/// setClaudeUsageFromInteractions.
fn set_claude_usage(out: &mut Vec<u8>, path: &str, usage: &Res<'_>) {
    if !usage.exists() {
        return;
    }
    let value = |paths: &[&str]| {
        let v = first_existing(usage, paths);
        v.exists().then(|| v.int())
    };
    let output = value(&["output_tokens", "total_output_tokens"]);
    let cached = value(&[
        "cache_read_input_tokens",
        "cache_read_tokens",
        "cached_tokens",
        "total_cached_tokens",
    ])
    .filter(|&v| v > 0);
    let cache_write = value(&[
        "cache_creation_input_tokens",
        "cache_creation_tokens",
        "cache_write_tokens",
    ])
    .filter(|&v| v > 0);
    let total_cache = cached.unwrap_or(0).wrapping_add(cache_write.unwrap_or(0));
    let input = if usage.get("input_tokens").exists() {
        Some(usage.get("input_tokens").int())
    } else {
        value(&["total_input_tokens", "prompt_tokens"]).map(|total| {
            if total >= total_cache {
                total.wrapping_sub(total_cache)
            } else {
                0
            }
        })
    };
    if let Some(input) = input {
        gj::set_int(out, &format!("{path}.input_tokens"), input);
    }
    if let Some(output) = output {
        gj::set_int(out, &format!("{path}.output_tokens"), output);
    }
    if let Some(cached) = cached {
        gj::set_int(out, &format!("{path}.cache_read_input_tokens"), cached);
    }
    if let Some(write) = cache_write {
        gj::set_int(out, &format!("{path}.cache_creation_input_tokens"), write);
    }
}

/// interactionsContentTexts.
fn content_texts(content: &Res<'_>) -> Vec<Vec<u8>> {
    if content.kind == Kind::String {
        return vec![string(content)];
    }
    let mut out = vec![];
    content.each(|_, part| {
        let text = first_nonblank(&[&part.get("text").bytes(), &part.get("content.text").bytes()]);
        if !text.is_empty() {
            out.push(text);
        }
        true
    });
    out
}

/// interactionsToolID.
fn tool_id(step: &Res<'_>) -> Vec<u8> {
    first_nonblank(&[
        &step.get("call_id").bytes(),
        &step.get("id").bytes(),
        &step.get("tool_use_id").bytes(),
        b"toolu_interactions",
    ])
}

/// interactionsSignature.
fn signature(step: &Res<'_>) -> Vec<u8> {
    first_nonblank(&[
        &step.get("signature").bytes(),
        &step.get("thought_signature").bytes(),
        &step.get("thoughtSignature").bytes(),
        &step.get("extra_content.google.thought_signature").bytes(),
    ])
}

/// Whether an interaction ended on its token limit.
fn hit_limit(interaction: &Res<'_>, root: &Res<'_>) -> bool {
    let status = first_nonblank(&[&interaction.get("status").bytes(), &root.get("status").bytes()]);
    let reason = first_nonblank(&[
        &interaction.get("finish_reason").bytes(),
        &root.get("finish_reason").bytes(),
    ]);
    status == b"incomplete" || reason == b"length" || reason == b"max_tokens"
}

/// ConvertInteractionsResponseToClaudeNonStream.
fn non_stream(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let root = gj::parse(body);
    let nested = root.get("interaction");
    let interaction = if nested.exists() { nested } else { root.clone() };
    let mut out = br#"{"id":"","type":"message","role":"assistant","model":"","content":[],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}"#.to_vec();
    let fallback = format!("msg_{}", now_nanos()).into_bytes();
    gj::set_str(
        &mut out,
        "id",
        first_nonblank(&[&interaction.get("id").bytes(), &root.get("id").bytes(), &fallback]),
    );
    gj::set_str(
        &mut out,
        "model",
        first_nonblank(&[&interaction.get("model").bytes(), ctx.model.as_bytes()]),
    );
    let mut steps = interaction.get("steps");
    if !steps.exists() {
        steps = root.get("steps");
    }
    let mut saw_tool_call = false;
    let mut blocks = vec![];
    steps.each(|_, step| {
        match step.get("type").bytes().as_ref() {
            b"thought" => {
                for text in content_texts(&step.get("content")) {
                    let mut block = br#"{"type":"thinking","thinking":""}"#.to_vec();
                    gj::set_str(&mut block, "thinking", &text);
                    let sig = signature(&step);
                    if !sig.is_empty() {
                        gj::set_str(&mut block, "signature", &sig);
                    }
                    blocks.push(block);
                }
            }
            b"function_call" => {
                saw_tool_call = true;
                let mut block = br#"{"type":"tool_use","id":"","name":"","input":{}}"#.to_vec();
                gj::set_str(&mut block, "id", tool_id(&step));
                gj::set_str(&mut block, "name", step.get("name").bytes());
                let sig = signature(&step);
                if !sig.is_empty() {
                    gj::set_str(&mut block, "signature", &sig);
                }
                let args = first_existing(&step, &["arguments", "args"]);
                if args.is_object() {
                    gj::set_raw(&mut block, "input", &args.raw);
                }
                blocks.push(block);
            }
            _ => {
                for text in content_texts(&step.get("content")) {
                    let mut block = br#"{"type":"text","text":""}"#.to_vec();
                    gj::set_str(&mut block, "text", &text);
                    blocks.push(block);
                }
            }
        }
        true
    });
    gj::set_items(&mut out, "content", &blocks);
    if saw_tool_call {
        gj::set_str(&mut out, "stop_reason", "tool_use");
    }
    if hit_limit(&interaction, &root) {
        gj::set_str(&mut out, "stop_reason", "max_tokens");
    }
    set_claude_usage(&mut out, "usage", &interactions_usage(&root));
    Ok(out)
}

/// interactionsToClaudeStreamState. Go also records each step's type; nothing reads it.
#[derive(Default)]
struct State {
    request_model: Vec<u8>,
    id: Vec<u8>,
    model: Vec<u8>,
    started: bool,
    active: bool,
    active_type: &'static str,
    block_index: i64,
    saw_tool_call: bool,
    completed: bool,
    stopped: bool,
    done: bool,
    names: HashMap<i64, Vec<u8>>,
    ids: HashMap<i64, Vec<u8>>,
    signatures: HashMap<i64, Vec<u8>>,
}

fn go_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    Box::new(State {
        request_model: ctx.model.as_bytes().to_vec(),
        model: ctx.model.as_bytes().to_vec(),
        ..State::default()
    })
}

impl State {
    /// appendClaudeMessageStart.
    fn message_start(&mut self, out: &mut Vec<Vec<u8>>) {
        if self.started {
            return;
        }
        let mut message = br#"{"type":"message_start","message":{"id":"","type":"message","role":"assistant","content":[],"model":"","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}}"#.to_vec();
        let fallback = format!("msg_{}", now_nanos()).into_bytes();
        gj::set_str(&mut message, "message.id", first_nonblank(&[&self.id, &fallback]));
        gj::set_str(&mut message, "message.model", &self.model);
        self.started = true;
        out.push(claude_event("message_start", &message));
    }

    /// appendClaudeContentBlockStop.
    fn block_stop(&mut self, out: &mut Vec<Vec<u8>>) {
        if !self.active {
            return;
        }
        let mut stop = br#"{"type":"content_block_stop","index":0}"#.to_vec();
        gj::set_int(&mut stop, "index", self.block_index);
        out.push(claude_event("content_block_stop", &stop));
        self.active = false;
        self.active_type = "";
        self.block_index += 1;
    }

    /// appendClaudeContentBlockStart (and ensureClaudeContentBlock).
    fn block_start(&mut self, kind: &'static str, out: &mut Vec<Vec<u8>>) {
        if self.active && self.active_type == kind {
            return;
        }
        self.block_stop(out);
        let mut block = if kind == "thinking" {
            br#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#.to_vec()
        } else {
            br#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#.to_vec()
        };
        gj::set_int(&mut block, "index", self.block_index);
        self.active = true;
        self.active_type = kind;
        out.push(claude_event("content_block_start", &block));
    }

    /// appendClaudeToolBlockStart.
    fn tool_block_start(&mut self, step_index: i64, out: &mut Vec<Vec<u8>>) {
        self.block_stop(out);
        let mut block = br#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"","name":"","input":{}}}"#.to_vec();
        gj::set_int(&mut block, "index", self.block_index);
        let fallback = format!("toolu_{step_index}").into_bytes();
        let id = self.ids.get(&step_index).cloned().unwrap_or_default();
        gj::set_str(&mut block, "content_block.id", first_nonblank(&[&id, &fallback]));
        gj::set_str(
            &mut block,
            "content_block.name",
            self.names.get(&step_index).cloned().unwrap_or_default(),
        );
        let sig = self.signatures.get(&step_index).cloned().unwrap_or_default();
        if !sig.is_empty() {
            gj::set_str(&mut block, "content_block.signature", &sig);
        }
        self.active = true;
        self.active_type = "tool_use";
        out.push(claude_event("content_block_start", &block));
    }

    /// appendClaudeContentDelta.
    fn content_delta(&self, kind: &str, field: &str, value: &[u8], out: &mut Vec<Vec<u8>>) {
        if value.is_empty() && kind != "input_json_delta" {
            return;
        }
        let mut delta = br#"{"type":"content_block_delta","index":0,"delta":{"type":""}}"#.to_vec();
        gj::set_int(&mut delta, "index", self.block_index);
        gj::set_str(&mut delta, "delta.type", kind);
        gj::set_str(&mut delta, &format!("delta.{field}"), value);
        out.push(claude_event("content_block_delta", &delta));
    }

    /// appendClaudeMessageDelta.
    fn message_delta(&mut self, root: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        if self.completed {
            return;
        }
        self.message_start(out);
        self.block_stop(out);
        let mut payload = br#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":0,"output_tokens":0}}"#.to_vec();
        if self.saw_tool_call {
            gj::set_str(&mut payload, "delta.stop_reason", "tool_use");
        }
        if hit_limit(&root.get("interaction"), root) {
            gj::set_str(&mut payload, "delta.stop_reason", "max_tokens");
        }
        set_claude_usage(&mut payload, "usage", &interactions_usage(root));
        out.push(claude_event("message_delta", &payload));
        self.completed = true;
    }

    /// appendClaudeMessageStop.
    fn message_stop(&mut self, out: &mut Vec<Vec<u8>>) {
        if self.done {
            return;
        }
        self.block_stop(out);
        if !self.completed {
            self.message_delta(&Res::default(), out);
        }
        if !self.stopped {
            out.push(claude_event("message_stop", br#"{"type":"message_stop"}"#));
            self.stopped = true;
        }
        self.done = true;
    }

    /// interactionsStepStartToClaude.
    fn step_start(&mut self, root: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        self.message_start(out);
        self.block_stop(out);
        let index = root.get("index").int();
        let step = root.get("step");
        match step.get("type").bytes().as_ref() {
            b"function_call" => {
                self.saw_tool_call = true;
                self.names.insert(index, string(&step.get("name")));
                self.ids.insert(index, tool_id(&step));
                self.signatures.insert(index, signature(&step));
                self.tool_block_start(index, out);
            }
            b"thought" => self.block_start("thinking", out),
            _ => self.block_start("text", out),
        }
    }

    /// interactionsStepDeltaToClaude.
    fn step_delta(&mut self, root: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        let index = root.get("index").int();
        let delta = root.get("delta");
        match delta.get("type").bytes().as_ref() {
            b"thought_summary" => {
                self.message_start(out);
                self.block_start("thinking", out);
                let text = first_nonblank(&[&delta.get("content.text").bytes(), &delta.get("text").bytes()]);
                self.content_delta("thinking_delta", "thinking", &text, out);
            }
            b"thought_signature" => {
                if self.active && self.active_type == "thinking" {
                    self.content_delta("signature_delta", "signature", &delta.get("signature").bytes(), out);
                }
            }
            b"arguments_delta" => {
                self.message_start(out);
                if !self.active || self.active_type != "tool_use" {
                    self.block_stop(out);
                    if self.names.get(&index).is_none_or(|n| n.is_empty()) {
                        self.names.insert(index, string(&root.get("step.name")));
                    }
                    if self.ids.get(&index).is_none_or(|n| n.is_empty()) {
                        self.ids.insert(index, format!("toolu_{index}").into_bytes());
                    }
                    self.tool_block_start(index, out);
                }
                self.content_delta("input_json_delta", "partial_json", &delta.get("arguments").bytes(), out);
            }
            _ => {
                self.message_start(out);
                self.block_start("text", out);
                self.content_delta("text_delta", "text", &delta.get("text").bytes(), out);
            }
        }
    }

    /// appendClaudeError.
    fn error(&mut self, root: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        self.block_stop(out);
        let mut error = root.get("error");
        if !error.exists() {
            error = root.get("interaction.error");
        }
        let mut message = string(&error.get("message"));
        if message.is_empty() {
            message = b"upstream error occurred".to_vec();
        }
        let mut kind = string(&error.get("type"));
        if kind.is_empty() {
            kind = b"api_error".to_vec();
        }
        let mut payload = br#"{"type":"error","error":{"type":"","message":""}}"#.to_vec();
        gj::set_str(&mut payload, "error.type", &kind);
        gj::set_str(&mut payload, "error.message", &message);
        out.push(claude_event("error", &payload));
    }
}

impl GoStream for State {
    /// ConvertInteractionsResponseToClaude.
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let payload = sse_payload(line);
        let mut out = vec![];
        if payload.is_empty() {
            return Ok(out);
        }
        if payload == b"[DONE]" {
            self.message_stop(&mut out);
            return Ok(out);
        }
        let root = gj::parse(&payload);
        if !root.exists() {
            return Ok(out);
        }
        match root.get("event_type").bytes().as_ref() {
            b"interaction.created" => {
                let interaction = root.get("interaction");
                self.id = first_nonblank(&[&interaction.get("id").bytes(), &self.id]);
                self.model = first_nonblank(&[&interaction.get("model").bytes(), &self.model, &self.request_model]);
                self.message_start(&mut out);
            }
            b"step.start" => self.step_start(&root, &mut out),
            b"step.delta" => self.step_delta(&root, &mut out),
            b"step.stop" => self.block_stop(&mut out),
            b"interaction.completed" | b"finish" => self.message_delta(&root, &mut out),
            b"response.failed" | b"interaction.failed" => self.error(&root, &mut out),
            b"done" => self.message_stop(&mut out),
            _ => {}
        }
        Ok(out)
    }
}
