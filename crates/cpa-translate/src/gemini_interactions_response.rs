//! Gemini responses -> Interactions responses, and Interactions responses -> Gemini
//! responses (internal/translator/gemini/interactions: interactions_gemini_response.go and
//! the stream half of interactions_gemini_common.go).

use std::collections::HashMap;

use cpa_common::json::{self as gj, Kind, Res};

use crate::common::{format_rfc3339_utc, now_nanos, now_unix, sse_event, trim_space};
use crate::gemini::{set_function_response_raw, set_function_response_result};
use crate::gemini_interactions::{content_part_to_gemini, first_existing, inline_to_interactions_content, text_part};
use crate::stream::GoStream;
use crate::{Error, ResponseCtx};

/// firstNonEmptyInteractionString: the first value that is not blank, as is.
fn first_nonblank(values: &[&[u8]]) -> Vec<u8> {
    values
        .iter()
        .find(|v| !trim_space(v).is_empty())
        .map(|v| v.to_vec())
        .unwrap_or_default()
}

/// interactionsThoughtSignature: the first non-blank signature field, trimmed.
pub(crate) fn thought_signature(part: &Res<'_>) -> Vec<u8> {
    [
        "thoughtSignature",
        "thought_signature",
        "extra_content.google.thought_signature",
    ]
    .iter()
    .map(|p| trim_space(&part.get(*p).bytes()).to_vec())
    .find(|s| !s.is_empty())
    .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------
// Gemini -> Interactions

/// geminiThoughtStepJSON.
fn thought_step(signature: &[u8], text: &[u8]) -> Vec<u8> {
    let mut step = br#"{"type":"thought"}"#.to_vec();
    if !signature.is_empty() {
        gj::set_str(&mut step, "signature", signature);
    }
    if !text.is_empty() {
        let mut item = br#"{"text":""}"#.to_vec();
        gj::set_str(&mut item, "text", text);
        gj::set_items(&mut step, "content", &[item]);
    }
    step
}

/// A model_output step holding one content item, then the part's signature as a thought.
fn output_steps(item: Vec<u8>, signature: &[u8]) -> Vec<Vec<u8>> {
    let mut step = br#"{"type":"model_output","content":[]}"#.to_vec();
    gj::set_items(&mut step, "content", &[item]);
    let mut steps = vec![step];
    if !signature.is_empty() {
        steps.push(thought_step(signature, b""));
    }
    steps
}

/// The `id`, else `call_id`, of a Gemini function part as `call_id`.
fn set_call_id(step: &mut Vec<u8>, part: &Res<'_>) {
    let id = first_existing(part, &["id", "call_id"]);
    if id.exists() {
        gj::set_str(step, "call_id", id.bytes());
    }
}

/// geminiPartToInteractionsSteps.
pub(crate) fn part_to_steps(part: &Res<'_>) -> Vec<Vec<u8>> {
    let signature = thought_signature(part);
    let call = part.get("functionCall");
    if call.exists() {
        let mut steps = vec![];
        if !signature.is_empty() {
            steps.push(thought_step(&signature, b""));
        }
        let mut step = br#"{"type":"function_call","name":"","arguments":{}}"#.to_vec();
        gj::set_str(&mut step, "name", call.get("name").bytes());
        set_call_id(&mut step, &call);
        let args = call.get("args");
        if args.exists() {
            gj::set_raw(&mut step, "arguments", &args.raw);
        }
        steps.push(step);
        return steps;
    }
    let result = part.get("functionResponse");
    if result.exists() {
        let mut step = br#"{"type":"function_result","name":"","result":{}}"#.to_vec();
        gj::set_str(&mut step, "name", result.get("name").bytes());
        set_call_id(&mut step, &result);
        let response = result.get("response");
        if response.exists() {
            gj::set_raw(&mut step, "result", &response.raw);
        }
        return vec![step];
    }
    let text = part.get("text");
    if text.exists() {
        if part.get("thought").bool() {
            return vec![thought_step(&signature, &text.bytes())];
        }
        if text.bytes().is_empty() {
            return if signature.is_empty() {
                vec![]
            } else {
                vec![thought_step(&signature, b"")]
            };
        }
        let mut item = br#"{"text":""}"#.to_vec();
        gj::set_str(&mut item, "text", text.bytes());
        return output_steps(item, &signature);
    }
    let inline = part.get("inlineData");
    if inline.exists() {
        let mut mime = inline.get("mimeType").bytes().into_owned();
        if mime.is_empty() {
            mime = inline.get("mime_type").bytes().into_owned();
        }
        let item = inline_to_interactions_content(&mime, &inline.get("data").bytes());
        return output_steps(item, &signature);
    }
    let inline = part.get("inline_data");
    if inline.exists() {
        let item = inline_to_interactions_content(&inline.get("mime_type").bytes(), &inline.get("data").bytes());
        return output_steps(item, &signature);
    }
    if signature.is_empty() {
        vec![]
    } else {
        vec![thought_step(&signature, b"")]
    }
}

/// The Gemini usage object (`usageMetadata`, else `usage_metadata`).
fn gemini_usage<'a>(root: &Res<'a>) -> Res<'a> {
    first_existing(root, &["usageMetadata", "usage_metadata"])
}

/// firstInteractionsGeminiUsage(...).Int().
fn usage_int(usage: &Res<'_>, camel: &str, snake: &str) -> i64 {
    first_existing(usage, &[camel, snake]).int()
}

/// setInteractionsUsageFromGemini.
fn set_usage(out: &mut Vec<u8>, path: &str, root: &Res<'_>) {
    let usage = gemini_usage(root);
    if !usage.exists() {
        return;
    }
    let set = |out: &mut Vec<u8>, key: &str, value: i64| gj::set_int(out, &format!("{path}.{key}"), value);
    set(
        out,
        "input_tokens",
        usage_int(&usage, "promptTokenCount", "prompt_token_count"),
    );
    set(
        out,
        "output_tokens",
        usage_int(&usage, "candidatesTokenCount", "candidates_token_count"),
    );
    let reasoning = first_existing(&usage, &["thoughtsTokenCount", "thoughts_token_count"]);
    if reasoning.exists() {
        set(out, "reasoning_tokens", reasoning.int());
    }
    set(
        out,
        "total_tokens",
        usage_int(&usage, "totalTokenCount", "total_token_count"),
    );
    let cached = first_existing(&usage, &["cachedContentTokenCount", "cached_content_token_count"]);
    if cached.exists() {
        set(out, "cached_tokens", cached.int());
    }
}

/// setInteractionsStreamUsageFromGemini.
fn set_stream_usage(out: &mut Vec<u8>, path: &str, root: &Res<'_>) {
    let usage = gemini_usage(root);
    if !usage.exists() {
        return;
    }
    let input = usage_int(&usage, "promptTokenCount", "prompt_token_count");
    let output = usage_int(&usage, "candidatesTokenCount", "candidates_token_count");
    let total = usage_int(&usage, "totalTokenCount", "total_token_count");
    let thoughts = usage_int(&usage, "thoughtsTokenCount", "thoughts_token_count");
    let mut cached = usage.get("cachedContentTokenCount").int();
    if cached == 0 {
        cached = usage.get("cached_content_token_count").int();
    }
    let set = |out: &mut Vec<u8>, key: &str, value: i64| gj::set_int(out, &format!("{path}.{key}"), value);
    set(out, "total_tokens", total);
    set(out, "total_input_tokens", input);
    gj::set_raw(
        out,
        &format!("{path}.input_tokens_by_modality"),
        format!(r#"[{{"modality":"text","tokens":{input}}}]"#),
    );
    set(out, "total_cached_tokens", cached);
    set(out, "total_output_tokens", output);
    set(out, "total_tool_use_tokens", 0);
    set(out, "total_thought_tokens", thoughts);
}

/// hasInteractionsGeminiStreamUsage.
fn has_stream_usage(root: &Res<'_>) -> bool {
    let usage = gemini_usage(root);
    usage.exists()
        && [
            "promptTokenCount",
            "candidatesTokenCount",
            "totalTokenCount",
            "thoughtsTokenCount",
            "cachedContentTokenCount",
            "prompt_token_count",
            "candidates_token_count",
            "total_token_count",
            "thoughts_token_count",
            "cached_content_token_count",
        ]
        .iter()
        .any(|p| usage.get(*p).exists())
}

/// StreamState for ConvertGeminiResponseToInteractionsStream.
struct ToInteractions {
    model: Vec<u8>,
    id: Vec<u8>,
    started: bool,
    finished: bool,
    completed: bool,
    done: bool,
    step_open: bool,
    step_type: &'static str,
    step_index: i64,
    next_step: i64,
}

pub(crate) fn gemini_to_interactions_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    Box::new(ToInteractions {
        model: ctx.model.as_bytes().to_vec(),
        id: format!("interaction_{}", now_nanos()).into_bytes(),
        started: false,
        finished: false,
        completed: false,
        done: false,
        step_open: false,
        step_type: "",
        step_index: 0,
        next_step: 0,
    })
}

const TEXT_DELTA: &[u8] = br#"{"index":0,"delta":{"text":"","type":"text"},"event_type":"step.delta"}"#;
const THOUGHT_DELTA: &[u8] =
    br#"{"index":0,"delta":{"content":{"text":"","type":"text"},"type":"thought_summary"},"event_type":"step.delta"}"#;
const ARGUMENTS_DELTA: &[u8] =
    br#"{"index":0,"delta":{"arguments":"","type":"arguments_delta"},"event_type":"step.delta"}"#;
const RESULT_DELTA: &[u8] =
    br#"{"index":0,"delta":{"type":"function_result","name":"","result":{}},"event_type":"step.delta"}"#;
const SIGNATURE_DELTA: &[u8] =
    br#"{"index":0,"delta":{"signature":"","type":"thought_signature"},"event_type":"step.delta"}"#;

impl ToInteractions {
    /// appendInteractionsCompleted (usage only when the chunk is given).
    fn complete(&mut self, root: Option<&Res<'_>>, out: &mut Vec<Vec<u8>>) {
        let now = format_rfc3339_utc(now_unix());
        let mut completed = br#"{"interaction":{"id":"","status":"completed","usage":{},"created":"","updated":"","service_tier":"standard","object":"interaction","model":""},"event_type":"interaction.completed"}"#.to_vec();
        gj::set_str(&mut completed, "interaction.id", &self.id);
        gj::set_str(&mut completed, "interaction.created", &now);
        gj::set_str(&mut completed, "interaction.updated", &now);
        gj::set_str(&mut completed, "interaction.model", &self.model);
        if let Some(root) = root.filter(|r| r.exists()) {
            set_stream_usage(&mut completed, "interaction.usage", root);
        }
        out.push(sse_event("interaction.completed", &completed));
        self.completed = true;
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
    }

    /// ensureInteractionsStep / appendInteractionsStepStart.
    fn ensure_step(&mut self, kind: &'static str, part: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        if self.step_open && self.step_type == kind {
            return;
        }
        self.stop_step(out);
        let step_id = format!("step_{}", now_nanos());
        self.step_index = self.next_step;
        self.next_step += 1;
        self.step_type = kind;
        self.step_open = true;
        let mut start = br#"{"index":0,"step":{"type":""},"event_type":"step.start"}"#.to_vec();
        gj::set_int(&mut start, "index", self.step_index);
        gj::set_str(&mut start, "step.type", kind);
        if kind == "function_call" {
            let id = first_existing(part, &["id", "call_id"]).bytes().into_owned();
            let id = if id.is_empty() { step_id.into_bytes() } else { id };
            gj::set_str(&mut start, "step.id", &id);
            gj::set_str(&mut start, "step.name", part.get("name").bytes());
            gj::set_raw(&mut start, "step.arguments", b"{}");
        }
        out.push(sse_event("step.start", &start));
    }

    fn delta(&self, template: &[u8], path: &str, value: &[u8], out: &mut Vec<Vec<u8>>) {
        let mut delta = template.to_vec();
        gj::set_int(&mut delta, "index", self.step_index);
        gj::set_str(&mut delta, path, value);
        out.push(sse_event("step.delta", &delta));
    }

    /// appendInteractionsThoughtSignature.
    fn signature(&mut self, part: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        let signature = thought_signature(part);
        if !signature.is_empty() {
            self.ensure_step("thought", &Res::default(), out);
            self.delta(SIGNATURE_DELTA, "delta.signature", &signature, out);
        }
    }

    /// appendGeminiPartToInteractionsStream.
    fn part(&mut self, part: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        let text = part.get("text");
        if text.exists() && !text.bytes().is_empty() {
            if part.get("thought").bool() {
                self.ensure_step("thought", &Res::default(), out);
                self.delta(THOUGHT_DELTA, "delta.content.text", &text.bytes(), out);
            } else {
                self.ensure_step("model_output", &Res::default(), out);
                self.delta(TEXT_DELTA, "delta.text", &text.bytes(), out);
            }
            self.signature(part, out);
            return;
        }
        let call = part.get("functionCall");
        if call.exists() {
            self.signature(part, out);
            self.ensure_step("function_call", &call, out);
            let args = call.get("args");
            let arguments: &[u8] = if args.exists() { &args.raw } else { b"{}" };
            self.delta(ARGUMENTS_DELTA, "delta.arguments", arguments, out);
            self.stop_step(out);
            return;
        }
        let result = part.get("functionResponse");
        if result.exists() {
            self.ensure_step("function_result", &result, out);
            let mut delta = RESULT_DELTA.to_vec();
            gj::set_int(&mut delta, "index", self.step_index);
            gj::set_str(&mut delta, "delta.name", result.get("name").bytes());
            let response = result.get("response");
            if response.exists() {
                gj::set_raw(&mut delta, "delta.result", &response.raw);
            }
            out.push(sse_event("step.delta", &delta));
            self.stop_step(out);
            return;
        }
        self.signature(part, out);
    }
}

impl GoStream for ToInteractions {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let mut out = vec![];
        if trim_space(line) == b"[DONE]" {
            if !self.completed {
                self.stop_step(&mut out);
                self.complete(None, &mut out);
            }
            if !self.done {
                out.push(sse_event("done", b"[DONE]"));
                self.done = true;
            }
            return Ok(out);
        }
        let root = gj::parse(line);
        if !self.started {
            let mut created = br#"{"interaction":{"id":"","status":"in_progress","object":"interaction","model":""},"event_type":"interaction.created"}"#.to_vec();
            gj::set_str(&mut created, "interaction.id", &self.id);
            gj::set_str(&mut created, "interaction.model", &self.model);
            out.push(sse_event("interaction.created", &created));
            let mut status =
                br#"{"interaction_id":"","status":"in_progress","event_type":"interaction.status_update"}"#.to_vec();
            gj::set_str(&mut status, "interaction_id", &self.id);
            out.push(sse_event("interaction.status_update", &status));
            self.started = true;
        }
        root.get("candidates.0.content.parts").each(|_, part| {
            self.part(&part, &mut out);
            true
        });
        if root.get("candidates.0.finishReason").exists() && !self.finished {
            self.stop_step(&mut out);
            self.finished = true;
        }
        if has_stream_usage(&root) && self.finished && !self.completed {
            self.complete(Some(&root), &mut out);
        }
        Ok(out)
    }
}

/// ConvertGeminiResponseToInteractionsNonStream.
pub(crate) fn gemini_to_interactions_non_stream(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let root = gj::parse(body);
    let mut out = br#"{"id":"","object":"interaction","status":"completed","model":"","steps":[]}"#.to_vec();
    let mut id = root.get("responseId").bytes().into_owned();
    if id.is_empty() {
        id = format!("interaction_{}", now_nanos()).into_bytes();
    }
    gj::set_str(&mut out, "id", &id);
    gj::set_str(&mut out, "model", ctx.model);
    let mut steps = vec![];
    root.get("candidates.0.content.parts").each(|_, part| {
        steps.extend(part_to_steps(&part).into_iter().filter(|s| !s.is_empty()));
        true
    });
    gj::set_items(&mut out, "steps", &steps);
    set_usage(&mut out, "usage", &root);
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// Interactions -> Gemini

/// interactionsToGeminiStreamState.
#[derive(Default)]
struct ToGemini {
    request_model: Vec<u8>,
    id: Vec<u8>,
    model: Vec<u8>,
    service_tier: Vec<u8>,
    names: HashMap<i64, Vec<u8>>,
    ids: HashMap<i64, Vec<u8>>,
    signatures: HashMap<i64, Vec<u8>>,
}

pub(crate) fn interactions_to_gemini_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    Box::new(ToGemini {
        request_model: ctx.model.as_bytes().to_vec(),
        model: ctx.model.as_bytes().to_vec(),
        ..ToGemini::default()
    })
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

/// interactionsGeminiSSEPayload: a JSON payload as is, else the `data:` lines joined.
fn sse_payload(raw: &[u8]) -> Vec<u8> {
    let trimmed = trim_space(raw);
    if trimmed.is_empty() || trimmed == b"[DONE]" {
        return vec![];
    }
    if trimmed.starts_with(b"{") {
        return trimmed.to_vec();
    }
    let mut payload = vec![];
    for line in trimmed.split(|&c| c == b'\n') {
        let Some(data) = trim_space(line).strip_prefix(b"data:") else {
            continue;
        };
        let data = trim_space(data);
        if data.is_empty() || data == b"[DONE]" {
            continue;
        }
        if !payload.is_empty() {
            payload.push(b'\n');
        }
        payload.extend_from_slice(data);
    }
    payload
}

/// interactionsUsageInt.
fn usage_value(usage: &Res<'_>, paths: &[&str]) -> Option<i64> {
    let value = first_existing(usage, paths);
    value.exists().then(|| value.int())
}

/// setGeminiUsageMetadataFromInteractionsUsage.
fn set_gemini_usage(out: &mut Vec<u8>, usage: &Res<'_>) {
    if !usage.exists() {
        return;
    }
    let input = usage_value(usage, &["input_tokens", "total_input_tokens"]);
    let output = usage_value(usage, &["output_tokens", "total_output_tokens"]);
    if let Some(input) = input {
        gj::set_int(out, "usageMetadata.promptTokenCount", input);
        gj::set_raw(
            out,
            "usageMetadata.promptTokensDetails",
            format!(r#"[{{"modality":"TEXT","tokenCount":{input}}}]"#),
        );
    }
    if let Some(output) = output {
        gj::set_int(out, "usageMetadata.candidatesTokenCount", output);
    }
    match usage_value(usage, &["total_tokens"]) {
        Some(total) => {
            gj::set_int(out, "usageMetadata.totalTokenCount", total);
        }
        None if input.is_some() || output.is_some() => {
            let total = input.unwrap_or(0).wrapping_add(output.unwrap_or(0));
            gj::set_int(out, "usageMetadata.totalTokenCount", total);
        }
        None => {}
    }
    if let Some(thoughts) = usage_value(usage, &["reasoning_tokens", "total_thought_tokens"]) {
        gj::set_int(out, "usageMetadata.thoughtsTokenCount", thoughts);
    }
    if let Some(cached) = usage_value(usage, &["cached_tokens", "total_cached_tokens"]) {
        gj::set_int(out, "usageMetadata.cachedContentTokenCount", cached);
    }
}

impl ToGemini {
    /// buildInteractionsGeminiChunk.
    fn chunk(&self, parts: Vec<Vec<u8>>, finish: &str, usage: &Res<'_>, include_empty: bool) -> Vec<u8> {
        let mut out = br#"{"candidates":[{"content":{"parts":[],"role":"model"},"index":0}]}"#.to_vec();
        let mut parts = parts;
        if parts.is_empty() && include_empty {
            parts.push(text_part(b"", false));
        }
        parts.retain(|p| !p.is_empty());
        gj::set_items(&mut out, "candidates.0.content.parts", &parts);
        if !finish.is_empty() {
            gj::set_str(&mut out, "candidates.0.finishReason", finish);
        }
        let model = first_nonblank(&[&self.model, &self.request_model]);
        if !model.is_empty() {
            gj::set_str(&mut out, "modelVersion", &model);
        }
        if !self.id.is_empty() {
            gj::set_str(&mut out, "responseId", &self.id);
        }
        if !self.service_tier.is_empty() {
            gj::set_str(&mut out, "usageMetadata.serviceTier", &self.service_tier);
        }
        set_gemini_usage(&mut out, usage);
        out
    }

    /// The interaction's id and model (and service tier) into the state.
    fn adopt(&mut self, interaction: &Res<'_>, tier: bool) {
        self.id = first_nonblank(&[&self.id, &interaction.get("id").bytes()]);
        self.model = first_nonblank(&[&self.model, &interaction.get("model").bytes(), &self.request_model]);
        if tier {
            self.service_tier = first_nonblank(&[&self.service_tier, &interaction.get("service_tier").bytes()]);
        }
    }

    /// interactionsStepDeltaToGeminiChunk.
    fn step_delta(&mut self, root: &Res<'_>) -> Option<Vec<u8>> {
        let index = root.get("index").int();
        let delta = root.get("delta");
        let lookup = |map: &HashMap<i64, Vec<u8>>| map.get(&index).cloned().unwrap_or_default();
        let part = match delta.get("type").bytes().as_ref() {
            b"arguments_delta" => {
                let mut part = br#"{"functionCall":{"name":"","args":{}}}"#.to_vec();
                let name = first_nonblank(&[&lookup(&self.names), &root.get("step.name").bytes()]);
                gj::set_str(&mut part, "functionCall.name", &name);
                let id = lookup(&self.ids);
                if !id.is_empty() {
                    gj::set_str(&mut part, "functionCall.id", &id);
                }
                let signature = lookup(&self.signatures);
                if !signature.is_empty() {
                    gj::set_str(&mut part, "thoughtSignature", &signature);
                }
                let arguments = delta.get("arguments").bytes();
                let arguments = trim_space(&arguments);
                if !arguments.is_empty() && gj::valid(arguments) {
                    gj::set_raw(&mut part, "functionCall.args", arguments);
                }
                part
            }
            b"text" => {
                let text = first_nonblank(&[&delta.get("text").bytes(), &delta.get("content.text").bytes()]);
                if text.is_empty() {
                    return None;
                }
                text_part(&text, false)
            }
            b"thought_summary" => {
                let text = first_nonblank(&[&delta.get("content.text").bytes(), &delta.get("text").bytes()]);
                if text.is_empty() {
                    return None;
                }
                text_part(&text, true)
            }
            b"thought_signature" => {
                let signature = first_nonblank(&[
                    &delta.get("signature").bytes(),
                    &delta.get("thought_signature").bytes(),
                    &delta.get("thoughtSignature").bytes(),
                ]);
                if signature.is_empty() {
                    return None;
                }
                self.signatures.insert(index, signature.clone());
                let mut part = text_part(b"", true);
                gj::set_str(&mut part, "thoughtSignature", &signature);
                part
            }
            _ => return None,
        };
        Some(self.chunk(vec![part], "", &Res::default(), false))
    }
}

/// mapInteractionsErrorToGemini.
fn gemini_error_status(code: &[u8]) -> (i64, &'static str) {
    let code = trim_space(code);
    match crate::common::go_lower(code).as_slice() {
        b"400" | b"invalid_argument" => (400, "INVALID_ARGUMENT"),
        b"401" | b"unauthenticated" => (401, "UNAUTHENTICATED"),
        b"403" | b"permission_denied" => (403, "PERMISSION_DENIED"),
        b"404" | b"not_found" => (404, "NOT_FOUND"),
        b"429" | b"resource_exhausted" | b"rate_limit_exceeded" => (429, "RESOURCE_EXHAUSTED"),
        b"499" | b"canceled" | b"cancelled" => (499, "CANCELLED"),
        b"503" | b"unavailable" => (503, "UNAVAILABLE"),
        b"504" | b"deadline_exceeded" => (504, "DEADLINE_EXCEEDED"),
        b"500" | b"internal" => (500, "INTERNAL"),
        _ => match std::str::from_utf8(code).ok().and_then(|s| s.parse::<i64>().ok()) {
            // strconv.Atoi: optional sign, decimal digits.
            Some(n) if (400..600).contains(&n) => (n, if n >= 500 { "INTERNAL" } else { "INVALID_ARGUMENT" }),
            _ => (500, "INTERNAL"),
        },
    }
}

impl GoStream for ToGemini {
    /// ConvertInteractionsResponseToGemini.
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let payload = sse_payload(line);
        if payload.is_empty() {
            return Ok(vec![]);
        }
        let root = gj::parse(&payload);
        if !root.exists() {
            return Ok(vec![]);
        }
        match root.get("event_type").bytes().as_ref() {
            b"interaction.created" => self.adopt(&root.get("interaction"), false),
            b"step.start" => {
                let index = root.get("index").int();
                let step = root.get("step");
                self.names.insert(index, step.get("name").bytes().into_owned());
                let id = first_nonblank(&[&step.get("call_id").bytes(), &step.get("id").bytes()]);
                self.ids.insert(index, id);
                let signature = first_nonblank(&[
                    &step.get("signature").bytes(),
                    &step.get("thoughtSignature").bytes(),
                    &step.get("thought_signature").bytes(),
                ]);
                self.signatures.insert(index, signature);
            }
            b"step.delta" => return Ok(self.step_delta(&root).into_iter().collect()),
            b"interaction.completed" | b"finish" => {
                self.adopt(&root.get("interaction"), true);
                return Ok(vec![self.chunk(vec![], "STOP", &interactions_usage(&root), true)]);
            }
            b"response.failed" | b"interaction.failed" => {
                let mut error = root.get("error");
                if !error.exists() {
                    error = root.get("interaction.error");
                }
                let mut message = error.get("message").bytes().into_owned();
                if message.is_empty() {
                    message = b"upstream error occurred".to_vec();
                }
                let code = first_nonblank(&[
                    &error.get("code").bytes(),
                    &root.get("code").bytes(),
                    &error.get("status").bytes(),
                ]);
                let (status, text) = gemini_error_status(&code);
                let mut out = br#"{"error":{"code":500,"message":"","status":"INTERNAL"}}"#.to_vec();
                gj::set_int(&mut out, "error.code", status);
                gj::set_str(&mut out, "error.message", &message);
                gj::set_str(&mut out, "error.status", text);
                return Ok(vec![out]);
            }
            _ => {}
        }
        Ok(vec![])
    }
}

/// setInteractionsGeminiRawObject.
fn set_raw_object(out: &mut Vec<u8>, path: &str, value: &Res<'_>) {
    if !value.exists() {
        gj::set_raw(out, path, b"{}");
        return;
    }
    if value.kind == Kind::String {
        let text = value.bytes();
        let raw = trim_space(&text);
        if !raw.is_empty() && gj::valid(raw) {
            gj::set_raw(out, path, raw);
            return;
        }
    }
    if !value.raw.is_empty() {
        gj::set_raw(out, path, &value.raw);
    }
}

/// setInteractionsGeminiFunctionResponse.
fn set_response(out: &mut Vec<u8>, path: &str, value: &Res<'_>) {
    if !value.exists() {
        gj::set_raw(out, path, b"{}");
        return;
    }
    if value.kind == Kind::String {
        let text = value.bytes();
        let raw = trim_space(&text);
        if !raw.is_empty() && gj::valid(raw) {
            set_function_response_raw(out, path, raw);
            return;
        }
    }
    if !value.raw.is_empty() {
        set_function_response_result(out, path, value);
    }
}

/// The step's `call_id`, else `id`, when not blank.
fn step_id(step: &Res<'_>) -> Vec<u8> {
    first_nonblank(&[&step.get("call_id").bytes(), &step.get("id").bytes()])
}

/// interactionsStepToGeminiParts.
fn step_to_parts(step: &Res<'_>) -> Vec<Vec<u8>> {
    match step.get("type").bytes().as_ref() {
        b"function_call" => {
            let mut part = br#"{"functionCall":{"name":"","args":{}}}"#.to_vec();
            gj::set_str(&mut part, "functionCall.name", step.get("name").bytes());
            let id = step_id(step);
            if !id.is_empty() {
                gj::set_str(&mut part, "functionCall.id", &id);
            }
            let signature = first_nonblank(&[
                &step.get("signature").bytes(),
                &step.get("thoughtSignature").bytes(),
                &step.get("thought_signature").bytes(),
            ]);
            if !signature.is_empty() {
                gj::set_str(&mut part, "thoughtSignature", &signature);
            }
            set_raw_object(
                &mut part,
                "functionCall.args",
                &first_existing(step, &["arguments", "args"]),
            );
            vec![part]
        }
        b"function_result" => {
            let mut part = br#"{"functionResponse":{"name":"","response":{}}}"#.to_vec();
            gj::set_str(&mut part, "functionResponse.name", step.get("name").bytes());
            let id = step_id(step);
            if !id.is_empty() {
                gj::set_str(&mut part, "functionResponse.id", &id);
            }
            set_response(
                &mut part,
                "functionResponse.response",
                &first_existing(step, &["result", "response"]),
            );
            vec![part]
        }
        kind => content_to_parts(&step.get("content"), kind == b"thought"),
    }
}

/// interactionsContentToGeminiParts.
fn content_to_parts(content: &Res<'_>, thought: bool) -> Vec<Vec<u8>> {
    if content.kind == Kind::String {
        return vec![text_part(&content.bytes(), thought)];
    }
    let mut parts = vec![];
    if content.is_object() {
        parts.extend(content_part_to_gemini(content, thought));
    } else if content.is_array() {
        content.each(|_, item| {
            parts.extend(content_part_to_gemini(&item, thought));
            true
        });
    }
    parts
}

/// ConvertInteractionsResponseToGeminiNonStream.
pub(crate) fn interactions_to_gemini_non_stream(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let root = gj::parse(body);
    let nested = root.get("interaction");
    let interaction = if nested.exists() { nested } else { root.clone() };
    let fallback_id = format!("response_{}", now_nanos()).into_bytes();
    let state = ToGemini {
        request_model: ctx.model.as_bytes().to_vec(),
        id: first_nonblank(&[&interaction.get("id").bytes(), &root.get("id").bytes(), &fallback_id]),
        model: first_nonblank(&[
            &interaction.get("model").bytes(),
            &root.get("model").bytes(),
            ctx.model.as_bytes(),
        ]),
        service_tier: first_nonblank(&[
            &interaction.get("service_tier").bytes(),
            &root.get("service_tier").bytes(),
        ]),
        ..ToGemini::default()
    };
    let mut steps = interaction.get("steps");
    if !steps.exists() {
        steps = root.get("steps");
    }
    let mut parts = vec![];
    steps.each(|_, step| {
        parts.extend(step_to_parts(&step));
        true
    });
    Ok(state.chunk(parts, "STOP", &interactions_usage(&root), true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_map_like_go() {
        assert_eq!(gemini_error_status(b" 429 "), (429, "RESOURCE_EXHAUSTED"));
        assert_eq!(gemini_error_status(b"Rate_Limit_Exceeded"), (429, "RESOURCE_EXHAUSTED"));
        assert_eq!(gemini_error_status(b"+418"), (418, "INVALID_ARGUMENT"));
        assert_eq!(gemini_error_status(b"502"), (502, "INTERNAL"));
        assert_eq!(gemini_error_status(b"600"), (500, "INTERNAL"));
        assert_eq!(gemini_error_status(b"4_00"), (500, "INTERNAL"));
        assert_eq!(gemini_error_status(b""), (500, "INTERNAL"));
    }

    #[test]
    fn sse_payload_joins_data_lines() {
        assert_eq!(sse_payload(b"  {\"a\":1}\n"), b"{\"a\":1}");
        assert_eq!(
            sse_payload(b"event: x\ndata: a\r\n data:b \ndata: [DONE]\ndata:"),
            b"a\nb"
        );
        assert!(sse_payload(b" [DONE] ").is_empty());
    }
}
