//! Gemini Interactions client, Claude upstream (internal/translator/claude/interactions):
//! ConvertInteractionsRequestToClaude, ConvertClaudeResponseToInteractions and its
//! NonStream.

use std::collections::HashMap;

use cpa_common::json::{self as gj, Kind, Res};

use crate::common::{
    format_rfc3339_utc, go_lower, normalize_claude_tool_input_schema, now_nanos, now_unix,
    sanitize_claude_function_name, sanitize_claude_tool_id, sse_event, trim_space,
};
use crate::gemini_interactions::first_existing;
use crate::stream::GoStream;
use crate::{Error, Registered, ResponseCtx};

pub static PAIR: Registered = registered!(
    Interactions -> Claude,
    request: |ctx, body| Ok(convert(ctx.model, body, ctx.stream)),
    non_stream: non_stream,
    go_stream: go_stream,
    token_count: None,
);

fn string(res: &Res<'_>) -> Vec<u8> {
    res.bytes().into_owned()
}

/// firstNonEmptyString (this package): the first non-empty value.
fn first_nonempty(values: &[&[u8]]) -> Vec<u8> {
    values
        .iter()
        .find(|v| !v.is_empty())
        .map(|v| v.to_vec())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------
// Request

/// common.ClaudeMessageAccumulator: consecutive same-role messages merge into one, with
/// an assistant turn's tool_use blocks moved after its other content.
#[derive(Default)]
pub(crate) struct Accumulator {
    messages: Vec<Vec<u8>>,
    role: Vec<u8>,
    content: Vec<Vec<u8>>,
    tool_uses: Vec<Vec<u8>>,
}

impl Accumulator {
    pub(crate) fn append(&mut self, message: &[u8]) {
        let root = gj::parse(message);
        let role = string(&root.get("role"));
        if role != b"user" && role != b"assistant" {
            return;
        }
        let parts = content_parts(&root.get("content"));
        if parts.is_empty() {
            return;
        }
        if !self.role.is_empty() && self.role != role {
            self.flush();
        }
        for part in parts {
            if role == b"assistant" && gj::get(&part, "type").bytes().as_ref() == b"tool_use" {
                self.tool_uses.push(part);
            } else {
                self.content.push(part);
            }
        }
        self.role = role;
    }

    pub(crate) fn flush(&mut self) {
        if self.role.is_empty() {
            return;
        }
        let mut parts = std::mem::take(&mut self.content);
        parts.append(&mut self.tool_uses);
        if !parts.is_empty() {
            let mut message = br#"{"role":"","content":[]}"#.to_vec();
            gj::set_str(&mut message, "role", &self.role);
            gj::set_raw(&mut message, "content", gj::join(&parts));
            self.messages.push(message);
        }
        self.role.clear();
    }

    pub(crate) fn into_messages(mut self) -> Vec<Vec<u8>> {
        self.flush();
        self.messages
    }
}

/// claudeMessageContentParts.
fn content_parts(content: &Res<'_>) -> Vec<Vec<u8>> {
    if !content.exists() || content.kind == Kind::Null {
        return vec![];
    }
    if content.kind == Kind::String {
        let text = content.bytes();
        if text.is_empty() {
            return vec![];
        }
        let mut part = br#"{"type":"text","text":""}"#.to_vec();
        gj::set_str(&mut part, "text", &text);
        return vec![part];
    }
    let mut parts = vec![];
    if content.is_array() {
        content.each(|_, part| {
            if part.is_object() {
                parts.push(part.raw.to_vec());
            }
            true
        });
    }
    parts
}

/// ConvertInteractionsRequestToClaude.
fn convert(model: &str, raw: &[u8], stream: bool) -> Vec<u8> {
    let root = gj::parse(raw);
    let mut out = br#"{"model":"","max_tokens":32000,"messages":[]}"#.to_vec();
    gj::set_str(&mut out, "model", model);
    if stream || root.get("stream").bool() {
        gj::set_bool(&mut out, "stream", true);
    }
    let system = claude_text(&first_existing(&root, &["system_instruction", "systemInstruction"]));
    if !system.is_empty() {
        gj::set_str(&mut out, "system", &system);
    }
    copy_generation_config(&mut out, &root);
    let mut messages = Accumulator::default();
    let input = root.get("input");
    if input.kind == Kind::String {
        let mut step = br#"{"type":"user_input","content":[{"type":"text","text":""}]}"#.to_vec();
        gj::set_str(&mut step, "content.0.text", input.bytes());
        step_to_message(&mut messages, &gj::parse(&step), "user");
    } else if input.is_object() {
        input_item(&mut messages, &input);
    } else {
        input.each(|_, step| {
            input_item(&mut messages, &step);
            true
        });
    }
    gj::set_items(&mut out, "messages", &messages.into_messages());
    copy_tools(&mut out, &root);
    out
}

fn copy_raw(out: &mut Vec<u8>, root: &Res<'_>, from: &str, to: &str) {
    let value = root.get(from);
    if value.exists() {
        gj::set_raw(out, to, &value.raw);
    }
}

/// copyInteractionsGenerationConfigToClaude.
fn copy_generation_config(out: &mut Vec<u8>, root: &Res<'_>) {
    let cfg = first_existing(root, &["generation_config", "generationConfig"]);
    if cfg.exists() {
        for (from, to) in [
            ("max_output_tokens", "max_tokens"),
            ("maxOutputTokens", "max_tokens"),
            ("top_p", "top_p"),
            ("topP", "top_p"),
            ("temperature", "temperature"),
            ("stop_sequences", "stop_sequences"),
            ("stopSequences", "stop_sequences"),
        ] {
            copy_raw(out, &cfg, from, to);
        }
        let level = first_existing(&cfg, &["thinking_level", "thinkingLevel", "reasoning.effort"]);
        if level.exists() {
            set_thinking_from_level(out, &level.bytes());
        }
        copy_tool_choice(out, &cfg.get("tool_choice"));
        copy_tool_choice(out, &cfg.get("toolChoice"));
    }
    let reasoning = root.get("reasoning");
    if reasoning.exists() {
        let level = first_existing(&reasoning, &["effort", "thinking_level"]);
        if level.exists() {
            set_thinking_from_level(out, &level.bytes());
        }
    }
    copy_tool_choice(out, &root.get("tool_choice"));
    copy_tool_choice(out, &root.get("toolChoice"));
}

/// setClaudeThinkingFromLevel.
fn set_thinking_from_level(out: &mut Vec<u8>, level: &[u8]) {
    let normalized = go_lower(trim_space(level));
    if normalized.is_empty() {
        return;
    }
    match normalized.as_slice() {
        b"none" | b"disabled" | b"off" | b"false" => {
            gj::set_str(out, "thinking.type", "disabled");
            gj::delete(out, "thinking.budget_tokens");
            return;
        }
        b"auto" | b"adaptive" => {
            gj::set_str(out, "thinking.type", "adaptive");
            gj::delete(out, "thinking.budget_tokens");
            return;
        }
        _ => {}
    }
    match cpa_common::thinking::convert_level_to_budget(&String::from_utf8_lossy(&normalized)) {
        Some(0) => {
            gj::set_str(out, "thinking.type", "disabled");
        }
        Some(budget) if budget < 0 => {
            gj::set_str(out, "thinking.type", "enabled");
        }
        Some(budget) => {
            gj::set_str(out, "thinking.type", "enabled");
            gj::set_int(out, "thinking.budget_tokens", budget);
        }
        None => {
            gj::set_str(out, "thinking.type", "adaptive");
            gj::set_str(out, "output_config.effort", &normalized);
        }
    }
}

/// copyInteractionsToolChoiceToClaude.
fn copy_tool_choice(out: &mut Vec<u8>, choice: &Res<'_>) {
    let kind = match choice.kind {
        Kind::String => go_lower(trim_space(&choice.bytes())),
        Kind::Json => go_lower(trim_space(&choice.get("type").bytes())),
        _ => return,
    };
    match kind.as_slice() {
        b"auto" => {
            gj::set_raw(out, "tool_choice", br#"{"type":"auto"}"#);
        }
        b"required" | b"any" => {
            gj::set_raw(out, "tool_choice", br#"{"type":"any"}"#);
        }
        b"function" | b"tool" if choice.kind == Kind::Json => {
            let mut name = string(&choice.get("name"));
            if name.is_empty() {
                name = string(&choice.get("function.name"));
            }
            if !name.is_empty() {
                let mut tool = br#"{"type":"tool","name":""}"#.to_vec();
                gj::set_str(&mut tool, "name", sanitize_claude_function_name(&name));
                gj::set_raw(out, "tool_choice", tool);
            }
        }
        _ => {}
    }
}

/// appendInteractionsInputItemToClaude.
fn input_item(messages: &mut Accumulator, step: &Res<'_>) {
    let steps = step.get("steps");
    if steps.is_array() {
        let role = match step.get("role").bytes().as_ref() {
            b"model" | b"assistant" => "assistant",
            _ => "user",
        };
        steps.each(|_, nested| {
            step_to_message(messages, &nested, role);
            true
        });
        return;
    }
    let parts = step.get("parts");
    if parts.exists() {
        let mut wrapped = br#"{"type":"user_input","content":[]}"#.to_vec();
        if matches!(step.get("role").bytes().as_ref(), b"model" | b"assistant") {
            gj::set_str(&mut wrapped, "type", "model_output");
        }
        gj::set_raw(&mut wrapped, "content", &parts.raw);
        step_to_message(messages, &gj::parse(&wrapped), "user");
        return;
    }
    match step.get("type").bytes().as_ref() {
        b"function_call" => {
            let mut tool_use = br#"{"type":"tool_use","id":"","name":"","input":{}}"#.to_vec();
            gj::set_str(&mut tool_use, "id", tool_id(step));
            gj::set_str(
                &mut tool_use,
                "name",
                sanitize_claude_function_name(&step.get("name").bytes()),
            );
            let args = first_existing(step, &["arguments", "args"]);
            if args.is_object() {
                gj::set_raw(&mut tool_use, "input", &args.raw);
            }
            let mut message = br#"{"role":"assistant","content":[]}"#.to_vec();
            gj::set_raw(&mut message, "content", gj::join(&[tool_use]));
            messages.append(&message);
        }
        b"function_result" => {
            let mut result = br#"{"type":"tool_result","tool_use_id":"","content":""}"#.to_vec();
            gj::set_str(&mut result, "tool_use_id", tool_id(step));
            if step.get("is_error").bool() {
                gj::set_bool(&mut result, "is_error", true);
            }
            let value = first_existing(step, &["result", "output"]);
            if value.is_array() {
                let mut items = vec![];
                value.each(|_, part| {
                    items.extend(content_to_claude(&part, "user"));
                    true
                });
                gj::set_raw(&mut result, "content", gj::join(&items));
            } else if value.exists() && !value.raw.is_empty() {
                gj::set_str(&mut result, "content", &value.raw);
            } else {
                gj::set_str(&mut result, "content", "");
            }
            let mut message = br#"{"role":"user","content":[]}"#.to_vec();
            gj::set_raw(&mut message, "content", gj::join(&[result]));
            messages.append(&message);
        }
        b"model_output" | b"thought" => step_to_message(messages, step, "assistant"),
        _ => step_to_message(messages, step, "user"),
    }
}

/// appendInteractionsStepToClaude.
fn step_to_message(messages: &mut Accumulator, step: &Res<'_>, default_role: &str) {
    let role = match step.get("role").bytes().as_ref() {
        b"user" => "user",
        b"assistant" => "assistant",
        _ => default_role,
    };
    let text_part = |text: &[u8]| {
        let mut part = br#"{"type":"text","text":""}"#.to_vec();
        gj::set_str(&mut part, "text", text);
        part
    };
    let mut items = vec![];
    let content = step.get("content");
    let text = step.get("text");
    if content.kind == Kind::String {
        items.push(text_part(&content.bytes()));
    } else if content.is_array() {
        content.each(|_, part| {
            items.extend(content_to_claude(&part, role));
            true
        });
    } else if text.exists() {
        items.push(text_part(&text.bytes()));
    }
    if items.is_empty() {
        return;
    }
    let mut message = br#"{"role":"","content":[]}"#.to_vec();
    gj::set_str(&mut message, "role", role);
    gj::set_raw(&mut message, "content", gj::join(&items));
    messages.append(&message);
}

/// interactionsContentToClaude.
fn content_to_claude(part: &Res<'_>, role: &str) -> Option<Vec<u8>> {
    let text_part = |text: &[u8]| {
        let mut out = br#"{"type":"text","text":""}"#.to_vec();
        gj::set_str(&mut out, "text", text);
        out
    };
    let mut kind = string(&part.get("type"));
    if kind.is_empty() && part.get("text").exists() {
        kind = b"text".to_vec();
    }
    match kind.as_slice() {
        b"text" => Some(text_part(&part.get("text").bytes())),
        b"thinking" | b"reasoning" => {
            if role != "assistant" {
                return None;
            }
            let mut out = br#"{"type":"thinking","thinking":""}"#.to_vec();
            gj::set_str(&mut out, "thinking", claude_text(part));
            Some(out)
        }
        b"image" => media_part(part, "image"),
        b"document" | b"file" => media_part(part, "document"),
        _ => {
            let text = claude_text(part);
            if !text.is_empty() {
                return Some(text_part(&text));
            }
            if !part.get("data").bytes().is_empty() || !part.get("file_data").bytes().is_empty() {
                return Some(text_part(&[&b"["[..], &kind, b" content omitted]"].concat()));
            }
            None
        }
    }
}

/// interactionsClaudeMediaPart.
fn media_part(part: &Res<'_>, kind: &str) -> Option<Vec<u8>> {
    let mut mime = string(&first_existing(
        part,
        &["mime_type", "mimeType", "media_type", "mediaType"],
    ));
    let mut data = string(&first_existing(part, &["data", "file_data", "fileData"]));
    let source = part.get("source");
    if source.exists() {
        if mime.is_empty() {
            mime = string(&source.get("media_type"));
        }
        if data.is_empty() {
            data = string(&source.get("data"));
        }
    }
    if mime.is_empty() || data.is_empty() {
        return None;
    }
    let mut out = br#"{"type":"","source":{"type":"base64","media_type":"","data":""}}"#.to_vec();
    gj::set_str(&mut out, "type", kind);
    gj::set_str(&mut out, "source.media_type", &mime);
    gj::set_str(&mut out, "source.data", &data);
    Some(out)
}

/// copyInteractionsToolsToClaude.
fn copy_tools(out: &mut Vec<u8>, root: &Res<'_>) {
    let tools = root.get("tools");
    if !tools.is_array() {
        return;
    }
    let mut items = vec![];
    tools.each(|_, tool| {
        for path in ["function_declarations", "functionDeclarations"] {
            let decls = tool.get(path);
            if decls.is_array() {
                decls.each(|_, decl| {
                    items.extend(claude_tool(&decl));
                    true
                });
                return true;
            }
        }
        items.extend(claude_tool(&tool));
        true
    });
    if !items.is_empty() {
        gj::set_raw(out, "tools", gj::join(&items));
    }
}

/// interactionsClaudeTool.
fn claude_tool(tool: &Res<'_>) -> Option<Vec<u8>> {
    let mut name = string(&tool.get("name"));
    if name.is_empty() {
        name = string(&tool.get("function.name"));
    }
    if name.is_empty() {
        return None;
    }
    let mut out = br#"{"name":"","input_schema":{"type":"object","properties":{}}}"#.to_vec();
    gj::set_str(&mut out, "name", sanitize_claude_function_name(&name));
    let description = first_existing(tool, &["description", "function.description"]);
    if description.exists() {
        gj::set_str(&mut out, "description", description.bytes());
    }
    let params = first_existing(
        tool,
        &[
            "parameters",
            "parametersJsonSchema",
            "parameters_json_schema",
            "input_schema",
        ],
    );
    if params.is_object() {
        gj::set_raw(
            &mut out,
            "input_schema",
            normalize_claude_tool_input_schema(Some(&params.raw)),
        );
    }
    Some(out)
}

/// interactionsClaudeToolID.
fn tool_id(step: &Res<'_>) -> Vec<u8> {
    for path in ["call_id", "id", "tool_use_id"] {
        let value = step.get(path).bytes();
        if !value.is_empty() {
            return sanitize_claude_tool_id(&value);
        }
    }
    let name = step.get("name").bytes();
    if !name.is_empty() {
        return sanitize_claude_tool_id(&[&b"toolu_"[..], &name].concat());
    }
    b"toolu_interactions".to_vec()
}

/// interactionsClaudeText.
fn claude_text(value: &Res<'_>) -> Vec<u8> {
    if !value.exists() {
        return vec![];
    }
    if value.kind == Kind::String {
        return string(value);
    }
    for path in ["text", "thinking"] {
        let field = value.get(path);
        if field.exists() {
            return string(&field);
        }
    }
    let content = value.get("content");
    if content.exists() {
        return claude_text(&content);
    }
    let mut out = vec![];
    let parts = value.get("parts");
    if parts.is_array() {
        parts.each(|_, part| {
            let text = claude_text(&part);
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

const USAGE_KEYS: [&str; 5] = [
    "input_tokens",
    "output_tokens",
    "cache_read_input_tokens",
    "cache_creation_input_tokens",
    "thinking_tokens",
];

/// claudeToInteractionsStreamState (also the accumulator of the SSE non-stream path).
#[derive(Default)]
struct State {
    request_model: Vec<u8>,
    id: Vec<u8>,
    model: Vec<u8>,
    created: bool,
    status_updated: bool,
    completed: bool,
    done: bool,
    usage: Vec<u8>,
    step_index: i64,
    active_index: i64,
    active_open: bool,
    current: HashMap<i64, &'static str>,
    names: HashMap<i64, Vec<u8>>,
    ids: HashMap<i64, Vec<u8>>,
    args: HashMap<i64, Vec<u8>>,
}

fn go_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    Box::new(State {
        request_model: ctx.model.as_bytes().to_vec(),
        model: ctx.model.as_bytes().to_vec(),
        ..State::default()
    })
}

/// claudeBlockInteractionsStepType.
fn block_step_type(kind: &[u8]) -> &'static str {
    match kind {
        b"thinking" => "thought",
        b"tool_use" => "function_call",
        _ => "model_output",
    }
}

/// claudeDeltaInteractionsStepType.
fn delta_step_type(kind: &[u8]) -> &'static str {
    match kind {
        b"thinking_delta" => "thought",
        b"input_json_delta" => "function_call",
        _ => "model_output",
    }
}

/// A step object for step.start: its type, plus a function call's name and IDs.
fn step_json(kind: &str, name: &[u8], id: &[u8]) -> Vec<u8> {
    let mut step = br#"{"type":""}"#.to_vec();
    gj::set_str(&mut step, "type", kind);
    if kind == "function_call" {
        gj::set_str(&mut step, "name", name);
        if !id.is_empty() {
            gj::set_str(&mut step, "id", id);
            gj::set_str(&mut step, "call_id", id);
        }
        gj::set_raw(&mut step, "arguments", b"{}");
    }
    step
}

/// claudeToolUseToInteractionsStep.
fn tool_use_step(part: &Res<'_>, args: &[u8]) -> Vec<u8> {
    let mut step = br#"{"type":"function_call","name":"","arguments":{}}"#.to_vec();
    gj::set_str(&mut step, "name", part.get("name").bytes());
    let id = part.get("id").bytes();
    if !id.is_empty() {
        gj::set_str(&mut step, "id", &id);
        gj::set_str(&mut step, "call_id", &id);
    }
    if !args.is_empty() && gj::valid(args) {
        gj::set_raw(&mut step, "arguments", args);
    }
    step
}

/// A model_output or thought step with one text item.
fn text_step(kind: &str, text: &[u8]) -> Vec<u8> {
    let mut step = br#"{"type":"","content":[]}"#.to_vec();
    gj::set_str(&mut step, "type", kind);
    let mut item = br#"{"type":"text","text":""}"#.to_vec();
    gj::set_str(&mut item, "text", text);
    gj::set_items(&mut step, "content", &[item]);
    step
}

/// setInteractionsUsageFromClaude.
fn set_usage(out: &mut Vec<u8>, path: &str, usage: &Res<'_>) {
    if !usage.exists() {
        return;
    }
    let input = usage.get("input_tokens");
    let output = usage.get("output_tokens");
    let cache_read = usage.get("cache_read_input_tokens").int();
    let cache_creation = usage.get("cache_creation_input_tokens").int();
    let thinking = usage.get("thinking_tokens").int();
    let set = |out: &mut Vec<u8>, key: &str, value: i64| gj::set_int(out, &format!("{path}.{key}"), value);
    if input.exists() {
        set(out, "input_tokens", input.int());
        set(out, "total_input_tokens", input.int());
    }
    if output.exists() {
        set(out, "output_tokens", output.int());
        set(out, "total_output_tokens", output.int());
    }
    if input.exists() || output.exists() {
        set(out, "total_tokens", input.int().wrapping_add(output.int()));
    }
    if cache_read != 0 || cache_creation != 0 {
        let cached = cache_read.wrapping_add(cache_creation);
        set(out, "cached_tokens", cached);
        set(out, "total_cached_tokens", cached);
    }
    if thinking != 0 {
        set(out, "reasoning_tokens", thinking);
        set(out, "total_thought_tokens", thinking);
    }
}

impl State {
    /// mergeClaudeUsage.
    fn merge_usage(&mut self, usage: &Res<'_>) {
        if !usage.exists() {
            return;
        }
        if self.usage.is_empty() {
            self.usage = b"{}".to_vec();
        }
        for key in USAGE_KEYS {
            let value = usage.get(key);
            if value.exists() {
                gj::set_raw(&mut self.usage, key, &value.raw);
            }
        }
    }

    fn forget(&mut self, index: i64) {
        self.current.remove(&index);
        self.names.remove(&index);
        self.ids.remove(&index);
        self.args.remove(&index);
    }

    /// appendClaudeInteractionsCreated (with the status update).
    fn create(&mut self, model: &[u8], out: &mut Vec<Vec<u8>>) {
        if self.created {
            return;
        }
        let fallback = format!("interaction_{}", now_nanos()).into_bytes();
        self.id = first_nonempty(&[&self.id, &fallback]);
        let mut created = br#"{"interaction":{"id":"","status":"in_progress","object":"interaction","model":""},"event_type":"interaction.created"}"#.to_vec();
        gj::set_str(&mut created, "interaction.id", &self.id);
        gj::set_str(&mut created, "interaction.model", first_nonempty(&[&self.model, model]));
        out.push(sse_event("interaction.created", &created));
        self.created = true;
        if !self.status_updated {
            let mut status =
                br#"{"interaction_id":"","status":"in_progress","event_type":"interaction.status_update"}"#.to_vec();
            gj::set_str(&mut status, "interaction_id", &self.id);
            out.push(sse_event("interaction.status_update", &status));
            self.status_updated = true;
        }
    }

    /// appendClaudeInteractionsStepStart: the step index advances on stop.
    fn start_step(&mut self, step: &[u8], out: &mut Vec<Vec<u8>>) {
        self.active_index = self.step_index;
        self.active_open = true;
        let mut start = br#"{"index":0,"step":{"type":""},"event_type":"step.start"}"#.to_vec();
        gj::set_int(&mut start, "index", self.active_index);
        gj::set_raw(&mut start, "step", step);
        out.push(sse_event("step.start", &start));
    }

    /// appendClaudeInteractionsStepStop.
    fn stop_step(&mut self, out: &mut Vec<Vec<u8>>) {
        if !self.active_open {
            return;
        }
        let mut stop = br#"{"index":0,"event_type":"step.stop"}"#.to_vec();
        gj::set_int(&mut stop, "index", self.active_index);
        out.push(sse_event("step.stop", &stop));
        self.active_open = false;
        self.step_index += 1;
    }

    /// appendClaudeDeltaToInteractions.
    fn delta(&mut self, delta: &Res<'_>, index: i64, out: &mut Vec<Vec<u8>>) {
        let mut payload = br#"{"index":0,"delta":{"text":"","type":"text"},"event_type":"step.delta"}"#.to_vec();
        gj::set_int(&mut payload, "index", self.active_index);
        match delta.get("type").bytes().as_ref() {
            b"text_delta" => {
                gj::set_str(&mut payload, "delta.text", delta.get("text").bytes());
            }
            b"thinking_delta" => {
                gj::set_str(&mut payload, "delta.type", "thought_summary");
                gj::set_str(&mut payload, "delta.content.type", "text");
                gj::set_str(&mut payload, "delta.content.text", delta.get("thinking").bytes());
                gj::delete(&mut payload, "delta.text");
            }
            b"input_json_delta" => {
                let partial = string(&delta.get("partial_json"));
                self.args.entry(index).or_default().extend_from_slice(&partial);
                payload = br#"{"index":0,"delta":{"arguments":"","type":"arguments_delta"},"event_type":"step.delta"}"#
                    .to_vec();
                gj::set_int(&mut payload, "index", self.active_index);
                gj::set_str(&mut payload, "delta.arguments", &partial);
            }
            _ => return,
        }
        out.push(sse_event("step.delta", &payload));
    }

    /// appendClaudeInteractionsCompleted.
    fn complete(&mut self, model: &[u8], root: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        if self.completed {
            return;
        }
        self.create(model, out);
        let now = format_rfc3339_utc(now_unix());
        let mut completed = br#"{"interaction":{"id":"","status":"completed","usage":{},"created":"","updated":"","service_tier":"standard","object":"interaction","model":""},"event_type":"interaction.completed"}"#.to_vec();
        gj::set_str(&mut completed, "interaction.id", &self.id);
        gj::set_str(&mut completed, "interaction.created", &now);
        gj::set_str(&mut completed, "interaction.updated", &now);
        gj::set_str(
            &mut completed,
            "interaction.model",
            first_nonempty(&[&self.model, model]),
        );
        if self.usage.is_empty() {
            set_usage(&mut completed, "interaction.usage", &root.get("usage"));
        } else {
            set_usage(&mut completed, "interaction.usage", &gj::parse(&self.usage));
        }
        out.push(sse_event("interaction.completed", &completed));
        self.completed = true;
    }
}

/// claudeInteractionsSSEPayload: only `data:` lines (and a bare `[DONE]`) carry events.
fn sse_payload(raw: &[u8]) -> &[u8] {
    let trimmed = trim_space(raw);
    if trimmed == b"[DONE]" {
        return trimmed;
    }
    trimmed.strip_prefix(b"data:").map(trim_space).unwrap_or_default()
}

impl GoStream for State {
    /// ConvertClaudeResponseToInteractions.
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let payload = sse_payload(line);
        let mut out = vec![];
        if payload.is_empty() {
            return Ok(out);
        }
        if payload == b"[DONE]" {
            if !self.done {
                out.push(sse_event("done", b"[DONE]"));
                self.done = true;
            }
            return Ok(out);
        }
        let model = self.request_model.clone();
        let root = gj::parse(payload);
        match root.get("type").bytes().as_ref() {
            b"message_start" => {
                let message = root.get("message");
                let fallback = format!("interaction_{}", now_nanos()).into_bytes();
                self.id = first_nonempty(&[&message.get("id").bytes(), &self.id, &fallback]);
                self.model = first_nonempty(&[&message.get("model").bytes(), &self.model, &model]);
                self.merge_usage(&message.get("usage"));
                let current = self.model.clone();
                self.create(&current, &mut out);
            }
            b"content_block_start" => {
                self.create(&model, &mut out);
                self.stop_step(&mut out);
                let index = root.get("index").int();
                let block = root.get("content_block");
                let kind = block_step_type(&block.get("type").bytes());
                self.current.insert(index, kind);
                if kind == "function_call" {
                    let name = string(&block.get("name"));
                    if !name.is_empty() {
                        self.names.insert(index, name);
                    }
                    let id = string(&block.get("id"));
                    if !id.is_empty() {
                        self.ids.insert(index, id);
                    }
                    let input = block.get("input");
                    if input.is_object() && &input.raw[..] != b"{}" {
                        self.args.insert(index, input.raw.to_vec());
                    }
                }
                let step = step_json(kind, &block.get("name").bytes(), &block.get("id").bytes());
                self.start_step(&step, &mut out);
            }
            b"content_block_delta" => {
                let index = root.get("index").int();
                let delta = root.get("delta");
                match self.current.get(&index).copied() {
                    None => {
                        let kind = delta_step_type(&delta.get("type").bytes());
                        self.create(&model, &mut out);
                        self.stop_step(&mut out);
                        self.start_step(format!(r#"{{"type":"{kind}"}}"#).as_bytes(), &mut out);
                        self.current.insert(index, kind);
                    }
                    Some(kind) if !self.active_open || self.active_index != index => {
                        self.create(&model, &mut out);
                        self.stop_step(&mut out);
                        let name = self.names.get(&index).cloned().unwrap_or_default();
                        let id = self.ids.get(&index).cloned().unwrap_or_default();
                        self.start_step(&step_json(kind, &name, &id), &mut out);
                    }
                    Some(_) => {}
                }
                self.delta(&delta, index, &mut out);
            }
            b"content_block_stop" => {
                self.stop_step(&mut out);
                self.forget(root.get("index").int());
            }
            b"message_delta" => {
                self.merge_usage(&root.get("usage"));
                self.stop_step(&mut out);
                self.complete(&model, &root, &mut out);
            }
            b"message_stop" => self.complete(&model, &root, &mut out),
            b"error" => {
                self.create(&model, &mut out);
                self.complete(&model, &root, &mut out);
            }
            _ => {}
        }
        Ok(out)
    }
}

/// ConvertClaudeResponseToInteractionsNonStream: a Claude message, or a buffered stream.
fn non_stream(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let root = gj::parse(body);
    let mut out = br#"{"id":"","object":"interaction","status":"completed","model":"","steps":[]}"#.to_vec();
    let fallback = format!("interaction_{}", now_nanos()).into_bytes();
    let mut steps = vec![];
    if root.exists() && root.get("content").exists() {
        gj::set_str(&mut out, "id", first_nonempty(&[&root.get("id").bytes(), &fallback]));
        gj::set_str(
            &mut out,
            "model",
            first_nonempty(&[&root.get("model").bytes(), ctx.model.as_bytes()]),
        );
        root.get("content").each(|_, part| {
            match part.get("type").bytes().as_ref() {
                b"text" => steps.push(text_step("model_output", &part.get("text").bytes())),
                b"thinking" => steps.push(text_step("thought", &part.get("thinking").bytes())),
                b"tool_use" => steps.push(tool_use_step(&part, trim_space(&part.get("input").raw))),
                _ => {}
            }
            true
        });
        if !steps.is_empty() {
            gj::set_raw(&mut out, "steps", gj::join(&steps));
        }
        set_usage(&mut out, "usage", &root.get("usage"));
        return Ok(out);
    }
    gj::set_str(&mut out, "id", &fallback);
    gj::set_str(&mut out, "model", ctx.model);
    let mut st = State::default();
    for line in body.split(|&c| c == b'\n') {
        let Some(payload) = trim_space(line).strip_prefix(b"data:") else {
            continue;
        };
        let payload = trim_space(payload);
        if payload == b"[DONE]" {
            continue;
        }
        let event = gj::parse(payload);
        let index = event.get("index").int();
        match event.get("type").bytes().as_ref() {
            b"message_start" => {
                let message = event.get("message");
                let id = message.get("id").bytes();
                if !id.is_empty() {
                    gj::set_str(&mut out, "id", &id);
                }
                let model = message.get("model").bytes();
                if !model.is_empty() {
                    gj::set_str(&mut out, "model", &model);
                }
                st.merge_usage(&message.get("usage"));
            }
            b"content_block_start" => {
                let block = event.get("content_block");
                let kind = block.get("type").bytes();
                st.current.insert(index, block_step_type(&kind));
                if kind.as_ref() == b"tool_use" {
                    st.names.insert(index, string(&block.get("name")));
                    st.ids.insert(index, string(&block.get("id")));
                    let input = block.get("input");
                    if input.is_object() && &input.raw[..] != b"{}" {
                        st.args.insert(index, input.raw.to_vec());
                    }
                }
            }
            b"content_block_delta" => {
                let delta = event.get("delta");
                let text = match delta.get("type").bytes().as_ref() {
                    b"text_delta" => Some(delta.get("text")),
                    b"thinking_delta" => Some(delta.get("thinking")),
                    b"input_json_delta" => Some(delta.get("partial_json")),
                    _ => None,
                };
                if let Some(text) = text {
                    st.args.entry(index).or_default().extend_from_slice(&text.bytes());
                }
            }
            b"content_block_stop" => {
                let text = st.args.get(&index).cloned().unwrap_or_default();
                let step = match st.current.get(&index).copied() {
                    Some("thought") => text_step("thought", &text),
                    Some("function_call") => {
                        let mut part = br#"{"type":"tool_use","id":"","name":"","input":{}}"#.to_vec();
                        gj::set_str(&mut part, "id", st.ids.get(&index).cloned().unwrap_or_default());
                        gj::set_str(&mut part, "name", st.names.get(&index).cloned().unwrap_or_default());
                        tool_use_step(&gj::parse(&part), trim_space(&text))
                    }
                    _ => text_step("model_output", &text),
                };
                st.forget(index);
                steps.push(step);
            }
            b"message_delta" => st.merge_usage(&event.get("usage")),
            _ => {}
        }
    }
    if !steps.is_empty() {
        gj::set_raw(&mut out, "steps", gj::join(&steps));
    }
    if !st.usage.is_empty() {
        set_usage(&mut out, "usage", &gj::parse(&st.usage));
    }
    Ok(out)
}
