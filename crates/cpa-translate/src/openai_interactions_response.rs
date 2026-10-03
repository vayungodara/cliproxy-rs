//! Gemini Interactions responses -> OpenAI Chat Completions responses, and OpenAI Chat
//! Completions responses -> Interactions responses
//! (internal/translator/openai/interactions/chat-completions: openai_interactions_response.go
//! and interactions_openai_response.go).

use std::collections::HashMap;

use cpa_common::json::{self as gj, Kind, Res};

use crate::common::{format_rfc3339_utc, now_nanos, now_unix, sse_event, trim_space};
use crate::gemini_interactions::first_existing;
use crate::openai_interactions::{
    antigravity_name_to_client, antigravity_name_to_upstream, first_nonblank, is_antigravity, json_string_value,
    reasoning_texts, text_step, tool_call_step,
};
use crate::stream::GoStream;
use crate::{Error, ResponseCtx};

fn string(res: &Res<'_>) -> Vec<u8> {
    res.bytes().into_owned()
}

/// openAIChatSSEPayload / openAIChatInteractionsPayload: the payload of one line or
/// frame (`data:` stripped, several data lines joined).
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

/// The first non-blank environment ID of an interaction event or body.
fn environment_id(interaction: &Res<'_>, root: &Res<'_>, extra: &[&str]) -> Vec<u8> {
    let mut values = vec![
        interaction.get("environment_id").bytes(),
        root.get("environment_id").bytes(),
        interaction.get("environment.id").bytes(),
        root.get("environment.id").bytes(),
    ];
    values.extend(extra.iter().map(|p| root.get(*p).bytes()));
    let values: Vec<&[u8]> = values.iter().map(|v| v.as_ref()).collect();
    first_nonblank(&values)
}

/// The Chat finish reason for an interaction's status and finish reason.
fn finish_reason(interaction: &Res<'_>, root: &Res<'_>, default: &'static str) -> &'static str {
    let status = first_nonblank(&[&interaction.get("status").bytes(), &root.get("status").bytes()]);
    let reason = first_nonblank(&[
        &interaction.get("finish_reason").bytes(),
        &root.get("finish_reason").bytes(),
    ]);
    if reason == b"content_filter" {
        "content_filter"
    } else if status == b"incomplete" || reason == b"length" || reason == b"max_tokens" {
        "length"
    } else {
        default
    }
}

// ---------------------------------------------------------------------------------------
// Interactions -> OpenAI Chat

/// interactionsUsageInt.
fn usage_value(usage: &Res<'_>, paths: &[&str]) -> Option<i64> {
    let value = first_existing(usage, paths);
    value.exists().then(|| value.int())
}

/// setOpenAIChatUsageFromInteractions.
fn set_chat_usage(out: &mut Vec<u8>, path: &str, usage: &Res<'_>) {
    if !usage.exists() {
        return;
    }
    for (key, sources) in [
        ("prompt_tokens", &["input_tokens", "total_input_tokens"][..]),
        ("completion_tokens", &["output_tokens", "total_output_tokens"]),
        ("total_tokens", &["total_tokens"]),
        (
            "prompt_tokens_details.cached_tokens",
            &["cached_tokens", "total_cached_tokens"],
        ),
        (
            "completion_tokens_details.reasoning_tokens",
            &["reasoning_tokens", "total_thought_tokens"],
        ),
    ] {
        if let Some(value) = usage_value(usage, sources) {
            gj::set_int(out, &format!("{path}.{key}"), value);
        }
    }
}

/// interactionsToOpenAIChatStreamState. Go also accumulates step types, arguments and
/// texts per step; nothing reads them back, so they are not kept here.
#[derive(Default)]
struct ToChat {
    request_model: Vec<u8>,
    id: Vec<u8>,
    model: Vec<u8>,
    environment: Vec<u8>,
    created: i64,
    started: bool,
    completed: bool,
    saw_tool_call: bool,
    tool_ids: HashMap<i64, Vec<u8>>,
    tool_names: HashMap<i64, Vec<u8>>,
    tool_index_by_step: HashMap<i64, i64>,
    next_tool_index: i64,
}

pub(crate) fn interactions_to_openai_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    Box::new(ToChat {
        request_model: ctx.model.as_bytes().to_vec(),
        model: ctx.model.as_bytes().to_vec(),
        ..ToChat::default()
    })
}

impl ToChat {
    /// openAIChatBaseChunk.
    fn base(&mut self) -> Vec<u8> {
        let mut chunk = br#"{"id":"","object":"chat.completion.chunk","created":0,"model":"","choices":[{"index":0,"delta":{},"finish_reason":null}]}"#.to_vec();
        let fallback = format!("chatcmpl_{}", now_nanos()).into_bytes();
        gj::set_str(&mut chunk, "id", first_nonblank(&[&self.id, &fallback]));
        if self.created == 0 {
            self.created = now_unix();
        }
        gj::set_int(&mut chunk, "created", self.created);
        gj::set_str(&mut chunk, "model", &self.model);
        if !self.environment.is_empty() {
            gj::set_str(&mut chunk, "environment_id", &self.environment);
        }
        chunk
    }

    /// ensureOpenAIChatStarted.
    fn start(&mut self, out: &mut Vec<Vec<u8>>) {
        if self.started {
            return;
        }
        let mut chunk = self.base();
        gj::set_str(&mut chunk, "choices.0.delta.role", "assistant");
        self.started = true;
        out.push(chunk);
    }

    fn delta(&mut self, field: &str, value: &[u8]) -> Vec<u8> {
        let mut chunk = self.base();
        gj::set_str(&mut chunk, &format!("choices.0.delta.{field}"), value);
        chunk
    }

    fn tool_index(&self, step: i64) -> i64 {
        self.tool_index_by_step.get(&step).copied().unwrap_or(step)
    }

    /// interactionsStepStartToOpenAIChat.
    fn step_start(&mut self, root: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        self.start(out);
        let index = root.get("index").int();
        let step = root.get("step");
        if step.get("type").bytes().as_ref() != b"function_call" {
            return;
        }
        self.saw_tool_call = true;
        let tool_index = *self.tool_index_by_step.entry(index).or_insert_with(|| {
            let next = self.next_tool_index;
            self.next_tool_index += 1;
            next
        });
        let fallback = format!("call_{tool_index}").into_bytes();
        let id = first_nonblank(&[&step.get("call_id").bytes(), &step.get("id").bytes(), &fallback]);
        self.tool_ids.insert(index, id.clone());
        let mut name = string(&step.get("name"));
        if is_antigravity(&self.request_model) || is_antigravity(&self.model) {
            name = antigravity_name_to_client(&name);
        }
        self.tool_names.insert(index, name.clone());
        let mut chunk = self.base();
        let mut call = br#"{"index":0,"id":"","type":"function","function":{"name":"","arguments":""}}"#.to_vec();
        gj::set_int(&mut call, "index", tool_index);
        gj::set_str(&mut call, "id", first_nonblank(&[&id, &fallback]));
        gj::set_str(&mut call, "function.name", &name);
        gj::set_raw(&mut chunk, "choices.0.delta.tool_calls.-1", call);
        out.push(chunk);
    }

    /// interactionsStepDeltaToOpenAIChat.
    fn step_delta(&mut self, root: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        let index = root.get("index").int();
        let delta = root.get("delta");
        self.start(out);
        match delta.get("type").bytes().as_ref() {
            b"thought_summary" => {
                let text = first_nonblank(&[&delta.get("content.text").bytes(), &delta.get("text").bytes()]);
                if !text.is_empty() {
                    let chunk = self.delta("reasoning_content", &text);
                    out.push(chunk);
                }
            }
            b"arguments_delta" => {
                let mut chunk = self.base();
                let mut call = br#"{"index":0,"function":{"arguments":""}}"#.to_vec();
                gj::set_int(&mut call, "index", self.tool_index(index));
                gj::set_str(&mut call, "function.arguments", delta.get("arguments").bytes());
                gj::set_raw(&mut chunk, "choices.0.delta.tool_calls.-1", call);
                out.push(chunk);
            }
            _ => {
                let text = string(&delta.get("text"));
                if !text.is_empty() {
                    let chunk = self.delta("content", &text);
                    out.push(chunk);
                }
            }
        }
    }

    /// appendOpenAIChatCompleted.
    fn complete(&mut self, root: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        if self.completed {
            return;
        }
        self.start(out);
        let mut chunk = self.base();
        let default = if self.saw_tool_call { "tool_calls" } else { "stop" };
        let reason = finish_reason(&root.get("interaction"), root, default);
        gj::set_str(&mut chunk, "choices.0.finish_reason", reason);
        set_chat_usage(&mut chunk, "usage", &interactions_usage(root));
        self.completed = true;
        out.push(chunk);
    }
}

/// interactionsFailedToOpenAIChat.
fn chat_error(root: &Res<'_>) -> Vec<u8> {
    let mut error = root.get("error");
    if !error.exists() {
        error = root.get("interaction.error");
    }
    let mut message = string(&error.get("message"));
    if message.is_empty() {
        message = b"upstream error occurred".to_vec();
    }
    let code = string(&error.get("code"));
    let mut kind = string(&error.get("type"));
    if kind.is_empty() {
        kind = b"server_error".to_vec();
    }
    let mut out = br#"{"error":{"message":"","type":"","code":""}}"#.to_vec();
    gj::set_str(&mut out, "error.message", &message);
    gj::set_str(&mut out, "error.type", &kind);
    if code.is_empty() {
        gj::delete(&mut out, "error.code");
    } else {
        gj::set_str(&mut out, "error.code", &code);
    }
    out
}

impl GoStream for ToChat {
    /// ConvertInteractionsResponseToOpenAI.
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let payload = sse_payload(line);
        if payload.is_empty() || payload == b"[DONE]" {
            return Ok(vec![]);
        }
        let root = gj::parse(&payload);
        if !root.exists() {
            return Ok(vec![]);
        }
        let mut out = vec![];
        let interaction = root.get("interaction");
        match root.get("event_type").bytes().as_ref() {
            b"interaction.created" => {
                self.id = first_nonblank(&[&interaction.get("id").bytes(), &self.id]);
                self.model = first_nonblank(&[&interaction.get("model").bytes(), &self.model, &self.request_model]);
                let environment = environment_id(&interaction, &root, &[]);
                if !environment.is_empty() {
                    self.environment = environment;
                }
                self.start(&mut out);
            }
            b"step.start" => self.step_start(&root, &mut out),
            b"step.delta" => self.step_delta(&root, &mut out),
            b"interaction.completed" | b"finish" => {
                let environment = environment_id(&interaction, &root, &[]);
                if !environment.is_empty() {
                    self.environment = environment;
                }
                self.complete(&root, &mut out);
            }
            b"response.failed" | b"interaction.failed" => out.push(chat_error(&root)),
            _ => {}
        }
        Ok(out)
    }
}

/// interactionsContentTextsForOpenAIChat, concatenated.
fn content_texts(content: &Res<'_>, out: &mut Vec<u8>) {
    if content.kind == Kind::String {
        out.extend_from_slice(&content.bytes());
        return;
    }
    content.each(|_, part| {
        out.extend(first_nonblank(&[
            &part.get("text").bytes(),
            &part.get("content.text").bytes(),
        ]));
        true
    });
}

/// ConvertInteractionsResponseToOpenAINonStream.
pub(crate) fn interactions_to_openai_non_stream(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let root = gj::parse(body);
    let nested = root.get("interaction");
    let interaction = if nested.exists() { nested } else { root.clone() };
    let mut out = br#"{"id":"","object":"chat.completion","created":0,"model":"","choices":[{"index":0,"message":{"role":"assistant","content":""},"finish_reason":"stop"}]}"#.to_vec();
    let fallback = format!("chatcmpl_{}", now_nanos()).into_bytes();
    let id = first_nonblank(&[&interaction.get("id").bytes(), &root.get("id").bytes(), &fallback]);
    gj::set_str(&mut out, "id", &id);
    gj::set_int(&mut out, "created", now_unix());
    let model = first_nonblank(&[&interaction.get("model").bytes(), ctx.model.as_bytes()]);
    gj::set_str(&mut out, "model", &model);
    let mut steps = interaction.get("steps");
    if !steps.exists() {
        steps = root.get("steps");
    }
    let (mut text, mut reasoning, mut calls) = (vec![], vec![], vec![]);
    let antigravity = is_antigravity(&model);
    steps.each(|_, step| {
        match step.get("type").bytes().as_ref() {
            b"model_output" => content_texts(&step.get("content"), &mut text),
            b"thought" => content_texts(&step.get("content"), &mut reasoning),
            b"function_call" => {
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
                calls.push(call);
            }
            _ => {}
        }
        true
    });
    if !text.is_empty() {
        gj::set_str(&mut out, "choices.0.message.content", &text);
    }
    if !reasoning.is_empty() {
        gj::set_str(&mut out, "choices.0.message.reasoning_content", &reasoning);
    }
    gj::set_items(&mut out, "choices.0.message.tool_calls", &calls);
    if !calls.is_empty() {
        gj::set_raw(&mut out, "choices.0.message.content", b"null");
        gj::set_str(&mut out, "choices.0.finish_reason", "tool_calls");
    }
    let reason = finish_reason(&interaction, &root, "");
    if !reason.is_empty() {
        gj::set_str(&mut out, "choices.0.finish_reason", reason);
    }
    let environment = environment_id(&interaction, &root, &["interaction.environment_id"]);
    if !environment.is_empty() {
        gj::set_str(&mut out, "environment_id", &environment);
    }
    set_chat_usage(&mut out, "usage", &interactions_usage(&root));
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// OpenAI Chat -> Interactions

/// setInteractionsUsageFromOpenAIChat.
fn set_interactions_usage(out: &mut Vec<u8>, path: &str, usage: &Res<'_>) {
    if !usage.exists() {
        return;
    }
    for (source, keys) in [
        ("prompt_tokens", &["input_tokens", "total_input_tokens"][..]),
        ("completion_tokens", &["output_tokens", "total_output_tokens"]),
        ("total_tokens", &["total_tokens"]),
        (
            "prompt_tokens_details.cached_tokens",
            &["cached_tokens", "total_cached_tokens"],
        ),
        (
            "completion_tokens_details.reasoning_tokens",
            &["reasoning_tokens", "total_thought_tokens"],
        ),
    ] {
        let value = usage.get(source);
        if value.exists() {
            for key in keys {
                gj::set_int(out, &format!("{path}.{key}"), value.int());
            }
        }
    }
}

/// openAIToInteractionsStreamState.
#[derive(Default)]
struct ToInteractions {
    model: Vec<u8>,
    created: bool,
    status_updated: bool,
    completed: bool,
    done: bool,
    step_type: &'static str,
    step_id: Vec<u8>,
    tool_ids: HashMap<i64, Vec<u8>>,
    tool_names: HashMap<i64, Vec<u8>>,
    id: Vec<u8>,
    next_step: i64,
    step_index: i64,
    step_open: bool,
    usage: Option<Vec<u8>>,
}

pub(crate) fn openai_to_interactions_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    Box::new(ToInteractions {
        model: ctx.model.as_bytes().to_vec(),
        ..ToInteractions::default()
    })
}

impl ToInteractions {
    /// appendInteractionsCreated (with the status update).
    fn create(&mut self, root: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        if !self.created {
            let fallback = format!("interaction_{}", now_nanos()).into_bytes();
            self.id = first_nonblank(&[&root.get("id").bytes(), &self.id, &fallback]);
            let mut created = br#"{"interaction":{"id":"","status":"in_progress","object":"interaction","model":""},"event_type":"interaction.created"}"#.to_vec();
            gj::set_str(&mut created, "interaction.id", &self.id);
            gj::set_str(
                &mut created,
                "interaction.model",
                first_nonblank(&[&self.model, &root.get("model").bytes()]),
            );
            out.push(sse_event("interaction.created", &created));
            self.created = true;
        }
        if !self.status_updated {
            let mut status =
                br#"{"interaction_id":"","status":"in_progress","event_type":"interaction.status_update"}"#.to_vec();
            gj::set_str(&mut status, "interaction_id", &self.id);
            out.push(sse_event("interaction.status_update", &status));
            self.status_updated = true;
        }
    }

    /// appendInteractionsStepStart.
    fn start_step(&mut self, kind: &'static str, step: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        self.step_index = self.next_step;
        self.next_step += 1;
        self.step_type = kind;
        self.step_open = true;
        let mut start = br#"{"index":0,"step":{"type":""},"event_type":"step.start"}"#.to_vec();
        gj::set_int(&mut start, "index", self.step_index);
        gj::set_str(&mut start, "step.type", kind);
        if kind == "function_call" {
            self.step_id = first_nonblank(&[&step.get("id").bytes(), &step.get("call_id").bytes(), &self.step_id]);
            if !self.step_id.is_empty() {
                gj::set_str(&mut start, "step.id", &self.step_id);
            }
            gj::set_str(&mut start, "step.name", step.get("name").bytes());
            gj::set_raw(&mut start, "step.arguments", b"{}");
        } else {
            self.step_id.clear();
        }
        out.push(sse_event("step.start", &start));
    }

    /// appendInteractionsStepStop.
    fn stop_step(&mut self, out: &mut Vec<Vec<u8>>) {
        if !self.step_open {
            return;
        }
        let mut stop = br#"{"index":0,"event_type":"step.stop"}"#.to_vec();
        gj::set_int(&mut stop, "index", self.step_index);
        out.push(sse_event("step.stop", &stop));
        self.step_open = false;
        self.step_type = "";
        self.step_id.clear();
    }

    /// ensureInteractionsStep.
    fn ensure_step(&mut self, kind: &'static str, root: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        self.create(root, out);
        if self.step_open && self.step_type == kind {
            return;
        }
        self.stop_step(out);
        self.start_step(kind, root, out);
    }

    fn delta(&self, template: &[u8], path: &str, value: &[u8], out: &mut Vec<Vec<u8>>) {
        let mut delta = template.to_vec();
        gj::set_int(&mut delta, "index", self.step_index);
        gj::set_str(&mut delta, path, value);
        out.push(sse_event("step.delta", &delta));
    }

    /// appendInteractionsCompleted.
    fn complete(&mut self, root: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        if self.completed {
            return;
        }
        if !self.created {
            self.create(root, out);
        }
        let now = format_rfc3339_utc(now_unix());
        let mut completed = br#"{"interaction":{"id":"","status":"completed","usage":{},"created":"","updated":"","service_tier":"standard","object":"interaction","model":""},"event_type":"interaction.completed"}"#.to_vec();
        gj::set_str(&mut completed, "interaction.id", &self.id);
        gj::set_str(&mut completed, "interaction.created", &now);
        gj::set_str(&mut completed, "interaction.updated", &now);
        gj::set_str(
            &mut completed,
            "interaction.model",
            first_nonblank(&[&self.model, &root.get("model").bytes()]),
        );
        let usage = root.get("usage");
        if usage.exists() {
            set_interactions_usage(&mut completed, "interaction.usage", &usage);
        } else if let Some(saved) = &self.usage {
            set_interactions_usage(&mut completed, "interaction.usage", &gj::parse(saved));
        }
        out.push(sse_event("interaction.completed", &completed));
        self.completed = true;
    }

    /// appendOpenAIToolCallDelta.
    fn tool_call(&mut self, root: &Res<'_>, call: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        let index = call.get("index").int();
        let id = string(&call.get("id"));
        if !id.is_empty() {
            self.tool_ids.insert(index, id);
        }
        let function = call.get("function");
        let mut name = string(&function.get("name"));
        if !name.is_empty() {
            if is_antigravity(&self.model) {
                name = antigravity_name_to_upstream(&name);
            }
            self.tool_names.insert(index, name);
        }
        let fallback = format!("call_{index}").into_bytes();
        let step_id = first_nonblank(&[&self.tool_ids.get(&index).cloned().unwrap_or_default(), &fallback]);
        if self.step_type != "function_call" || self.step_id != step_id {
            self.stop_step(out);
            let mut step = br#"{"type":"function_call","id":"","name":"","arguments":{}}"#.to_vec();
            gj::set_str(&mut step, "id", &step_id);
            gj::set_str(
                &mut step,
                "name",
                self.tool_names.get(&index).cloned().unwrap_or_default(),
            );
            self.create(root, out);
            self.start_step("function_call", &gj::parse(&step), out);
        }
        let args = function.get("arguments");
        if args.exists() && !args.bytes().is_empty() {
            self.delta(ARGUMENTS_DELTA, "delta.arguments", &args.bytes(), out);
        }
    }
}

const TEXT_DELTA: &[u8] = br#"{"index":0,"delta":{"text":"","type":"text"},"event_type":"step.delta"}"#;
const THOUGHT_DELTA: &[u8] =
    br#"{"index":0,"delta":{"content":{"text":"","type":"text"},"type":"thought_summary"},"event_type":"step.delta"}"#;
const ARGUMENTS_DELTA: &[u8] =
    br#"{"index":0,"delta":{"arguments":"","type":"arguments_delta"},"event_type":"step.delta"}"#;

impl GoStream for ToInteractions {
    /// ConvertOpenAIResponseToInteractions.
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let payload = sse_payload(line);
        if payload.is_empty() {
            return Ok(vec![]);
        }
        let mut out = vec![];
        if payload == b"[DONE]" {
            self.stop_step(&mut out);
            self.complete(&Res::default(), &mut out);
            if !self.done {
                out.push(sse_event("done", b"[DONE]"));
                self.done = true;
            }
            return Ok(out);
        }
        let root = gj::parse(&payload);
        if !root.exists() {
            return Ok(vec![]);
        }
        let usage = root.get("usage");
        if usage.exists() {
            self.usage = Some(usage.raw.to_vec());
        }
        let choices = root.get("choices");
        if !choices.is_array() {
            return Ok(out);
        }
        if choices.array().is_empty() {
            if usage.exists() {
                self.stop_step(&mut out);
                self.complete(&root, &mut out);
            }
            return Ok(out);
        }
        choices.each(|_, choice| {
            let delta = choice.get("delta");
            let reasoning = delta.get("reasoning_content");
            if reasoning.exists() {
                for text in reasoning_texts(&reasoning) {
                    self.ensure_step("thought", &root, &mut out);
                    self.delta(THOUGHT_DELTA, "delta.content.text", &text, &mut out);
                }
            }
            let content = delta.get("content");
            if content.exists() && !content.bytes().is_empty() {
                self.ensure_step("model_output", &root, &mut out);
                self.delta(TEXT_DELTA, "delta.text", &content.bytes(), &mut out);
            }
            let calls = delta.get("tool_calls");
            if calls.is_array() {
                calls.each(|_, call| {
                    self.tool_call(&root, &call, &mut out);
                    true
                });
            }
            if choice.get("finish_reason").exists() {
                self.stop_step(&mut out);
            }
            true
        });
        Ok(out)
    }
}

/// ConvertOpenAIResponseToInteractionsNonStream.
pub(crate) fn openai_to_interactions_non_stream(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let root = gj::parse(body);
    let mut out = br#"{"id":"","status":"completed","object":"interaction","model":"","steps":[]}"#.to_vec();
    let fallback = format!("interaction_{}", now_nanos()).into_bytes();
    gj::set_str(&mut out, "id", first_nonblank(&[&root.get("id").bytes(), &fallback]));
    gj::set_str(
        &mut out,
        "model",
        first_nonblank(&[ctx.model.as_bytes(), &root.get("model").bytes()]),
    );
    let antigravity = is_antigravity(ctx.model.as_bytes());
    let mut steps = vec![];
    root.get("choices").each(|_, choice| {
        let message = choice.get("message");
        let reasoning = message.get("reasoning_content");
        if reasoning.exists() {
            for text in reasoning_texts(&reasoning) {
                steps.push(text_step("thought", &text));
            }
        }
        let content = message.get("content");
        if content.exists() && !content.bytes().is_empty() {
            steps.push(text_step("model_output", &content.bytes()));
        }
        let calls = message.get("tool_calls");
        if calls.is_array() {
            calls.each(|_, call| {
                steps.extend(tool_call_step(&call, antigravity));
                true
            });
        }
        let reason = choice.get("finish_reason");
        if reason.exists() {
            gj::set_str(&mut out, "finish_reason", reason.bytes());
        }
        true
    });
    gj::set_items(&mut out, "steps", &steps);
    set_interactions_usage(&mut out, "usage", &root.get("usage"));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads_strip_data_prefixes_like_go() {
        assert_eq!(sse_payload(b" data: {\"a\":1} "), b"{\"a\":1}");
        assert_eq!(sse_payload(b"event: x\ndata: a\n data:b"), b"a\nb");
        assert_eq!(sse_payload(b"event: x"), b"event: x");
        assert_eq!(sse_payload(b" [DONE] "), b"[DONE]");
    }
}
