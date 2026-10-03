//! Gemini Interactions responses -> OpenAI Responses responses (with the apply_patch
//! custom-tool bridge), and OpenAI Responses responses -> Interactions responses
//! (internal/translator/openai/interactions/responses/interactions_openai_responses_response.go).

use std::collections::{HashMap, HashSet};

use cpa_common::json::{self as gj, Kind, Res};

use crate::apply_patch::{self, CallState};
use crate::common::{format_rfc3339_utc, now_nanos, now_unix, request_model_name, sse_event, trim_space};
use crate::gemini_interactions::first_existing;
use crate::openai_interactions::{
    antigravity_name_to_client, antigravity_name_to_upstream, first_nonblank, is_antigravity, json_string_value,
};
use crate::responses_interactions::{
    content_part, content_part_to_responses, content_texts, function_call_item, function_call_step, summary_parts,
    tool_identity_map,
};
use crate::responses_tools::{Identity, unwrap_custom_tool_input};
use crate::stream::GoStream;
use crate::{Error, ResponseCtx};

fn string(res: &Res<'_>) -> Vec<u8> {
    res.bytes().into_owned()
}

/// interactionsSSEPayload: the payload of one line or frame.
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

/// responseModel.
fn response_model(model: &str, root: &Res<'_>) -> Vec<u8> {
    first_nonblank(&[
        model.as_bytes(),
        &root.get("model").bytes(),
        &root.get("response.model").bytes(),
        &root.get("interaction.model").bytes(),
    ])
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

/// interactionsReasoningEncryptedContent: a trimmed signature Go recognizes, else empty.
fn encrypted_content(raw: &[u8]) -> Vec<u8> {
    let candidate = trim_space(raw);
    if !candidate.is_empty() && cpa_common::signature::is_recognized_reasoning_signature(candidate) {
        candidate.to_vec()
    } else {
        vec![]
    }
}

/// interactionsThoughtSignature.
fn thought_signature(step: &Res<'_>) -> Vec<u8> {
    for path in [
        "encrypted_content",
        "signature",
        "thought_signature",
        "thoughtSignature",
        "extra_content.google.thought_signature",
    ] {
        let signature = encrypted_content(&step.get(path).bytes());
        if !signature.is_empty() {
            return signature;
        }
    }
    let mut signature = vec![];
    let content = step.get("content");
    if content.is_array() {
        content.each(|_, part| {
            let candidate = first_nonblank(&[
                &part.get("signature").bytes(),
                &part.get("thought_signature").bytes(),
                &part.get("thoughtSignature").bytes(),
                &part.get("extra_content.google.thought_signature").bytes(),
            ]);
            signature = encrypted_content(&candidate);
            signature.is_empty()
        });
    }
    signature
}

/// setResponsesUsageFromInteractions: token counts always present (zero when missing).
fn set_responses_usage(out: &mut Vec<u8>, path: &str, usage: &Res<'_>) {
    let value = |paths: &[&str]| {
        let v = first_existing(usage, paths);
        v.exists().then(|| v.int())
    };
    let (mut input, mut output, mut total) = (0, 0, 0);
    if usage.exists() {
        input = value(&["input_tokens", "total_input_tokens"]).unwrap_or(0);
        output = value(&["output_tokens", "total_output_tokens"]).unwrap_or(0);
        total = value(&["total_tokens"]).unwrap_or(input.wrapping_add(output));
    }
    gj::set_int(out, &format!("{path}.input_tokens"), input);
    gj::set_int(out, &format!("{path}.output_tokens"), output);
    gj::set_int(out, &format!("{path}.total_tokens"), total);
    if usage.exists() {
        if let Some(cached) = value(&["cached_tokens", "total_cached_tokens"]) {
            gj::set_int(out, &format!("{path}.input_tokens_details.cached_tokens"), cached);
        }
        if let Some(reasoning) = value(&["reasoning_tokens", "total_thought_tokens"]) {
            gj::set_int(
                out,
                &format!("{path}.output_tokens_details.reasoning_tokens"),
                reasoning,
            );
        }
    }
}

/// The (status, incomplete reason) of an interaction's status and finish reason.
fn incomplete_reason(status: &[u8], reason: &[u8]) -> Option<&'static str> {
    if reason == b"content_filter" {
        Some("content_filter")
    } else if status == b"incomplete" || reason == b"length" || reason == b"max_tokens" {
        Some("max_output_tokens")
    } else {
        None
    }
}

// ---------------------------------------------------------------------------------------
// Interactions -> Responses (non-stream)

/// interactionsStepToResponsesOutput.
fn step_to_output(step: &Res<'_>, antigravity: bool, identities: &HashMap<Vec<u8>, Identity>) -> Option<Vec<u8>> {
    match step.get("type").bytes().as_ref() {
        b"model_output" => {
            let mut item = br#"{"type":"message","role":"assistant","content":[]}"#.to_vec();
            let id = first_nonblank(&[&step.get("id").bytes(), &step.get("step_id").bytes()]);
            if !id.is_empty() {
                gj::set_str(&mut item, "id", &id);
            }
            let content = step.get("content");
            let mut parts = vec![];
            if content.kind == Kind::String {
                let mut part = br#"{"type":"output_text","text":""}"#.to_vec();
                gj::set_str(&mut part, "text", content.bytes());
                parts.push(part);
            } else {
                content.each(|_, part| {
                    parts.extend(content_part_to_responses(&part, "assistant"));
                    true
                });
            }
            gj::set_items(&mut item, "content", &parts);
            Some(item)
        }
        b"thought" => {
            let mut item = br#"{"type":"reasoning","summary":[]}"#.to_vec();
            let signature = thought_signature(step);
            if !signature.is_empty() {
                gj::set_str(&mut item, "encrypted_content", &signature);
            }
            gj::set_items(
                &mut item,
                "summary",
                &summary_parts(&content_texts(&step.get("content"))),
            );
            Some(item)
        }
        b"function_call" => {
            let mut item = function_call_item(step, antigravity, identities);
            gj::set_str(&mut item, "status", "completed");
            Some(item)
        }
        _ => None,
    }
}

/// The client request whose tools define the identity map.
fn identity_source<'a>(ctx: &ResponseCtx<'a>) -> &'a [u8] {
    if ctx.original_request.is_empty() {
        ctx.translated_request
    } else {
        ctx.original_request
    }
}

/// ConvertInteractionsResponseToOpenAIResponsesNonStream.
pub(crate) fn interactions_to_responses_non_stream(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let root = gj::parse(body);
    let mut out = br#"{"id":"","object":"response","status":"completed","model":"","output":[]}"#.to_vec();
    gj::set_str(
        &mut out,
        "id",
        first_nonblank(&[&root.get("id").bytes(), &root.get("interaction.id").bytes()]),
    );
    let model = response_model(ctx.model, &root);
    gj::set_str(&mut out, "model", &model);
    let mut steps = root.get("steps");
    if !steps.exists() {
        steps = root.get("interaction.steps");
    }
    let antigravity = is_antigravity(&model);
    let source = identity_source(ctx);
    let identities = if source.is_empty() {
        HashMap::new()
    } else {
        tool_identity_map(source, antigravity)
    };
    let patch_enabled = identities.values().any(|i| i.apply_patch);
    let status = first_nonblank(&[&root.get("status").bytes(), &root.get("interaction.status").bytes()]);
    let source_error = first_existing(&root, &["error", "interaction.error"]);
    let mut failed =
        patch_enabled && (status == b"failed" || (source_error.exists() && source_error.kind != Kind::Null));
    let mut outputs = vec![];
    steps.each(|_, step| {
        if failed {
            return false;
        }
        let is_call = step.get("type").bytes().as_ref() == b"function_call";
        let name = step.get("name").bytes();
        if patch_enabled && is_call && name.is_empty() {
            failed = true;
            return false;
        }
        if is_call && identities.get(name.as_ref()).is_some_and(|i| i.apply_patch) {
            let arguments = json_string_value(&step.get("arguments"), b"{}");
            if !gj::valid(body) || CallState::default().finish_arguments(&arguments).is_err() {
                failed = true;
                return false;
            }
        }
        outputs.extend(step_to_output(&step, antigravity, &identities));
        true
    });
    if failed {
        return Err(Error(apply_patch::UPSTREAM_ERROR_MESSAGE.into()));
    }
    if !outputs.is_empty() {
        gj::set_raw(&mut out, "output", gj::join(&outputs));
    }
    let reason = first_nonblank(&[
        &root.get("finish_reason").bytes(),
        &root.get("interaction.finish_reason").bytes(),
    ]);
    if let Some(reason) = incomplete_reason(&status, &reason) {
        gj::set_str(&mut out, "status", "incomplete");
        gj::set_str(&mut out, "incomplete_details.reason", reason);
    }
    let environment = first_nonblank(&[
        &root.get("environment_id").bytes(),
        &root.get("interaction.environment_id").bytes(),
        &root.get("environment.id").bytes(),
        &root.get("interaction.environment.id").bytes(),
    ]);
    if !environment.is_empty() {
        gj::set_str(&mut out, "environment_id", &environment);
    }
    set_responses_usage(&mut out, "usage", &interactions_usage(&root));
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// Interactions -> Responses (stream)

/// interactionsFunctionCallState. Errors only matter by presence: every apply_patch
/// failure ends the stream with the same `response.failed` event.
#[derive(Default)]
struct Call {
    id: Vec<u8>,
    call_id: Vec<u8>,
    item_id_seen: bool,
    call_id_seen: bool,
    initial_arguments: Vec<u8>,
    raw_name: Vec<u8>,
    added: bool,
    patch: Option<CallState>,
    pending_error: bool,
    snapshot_arguments: Vec<u8>,
    snapshot_input: String,
    has_snapshot: bool,
    name: Vec<u8>,
    namespace: Vec<u8>,
    custom: bool,
    arguments: Vec<u8>,
    fragments: Vec<Vec<u8>>,
    source_stopped: bool,
    stop_pending: bool,
    identity_finalized: bool,
    arguments_done: bool,
    item_done: bool,
}

/// interactionsToResponsesStreamState.
#[derive(Default)]
struct ToResponses {
    model: String,
    original: Vec<u8>,
    translated: Vec<u8>,
    tool_input_failed: bool,
    id: Vec<u8>,
    environment: Vec<u8>,
    calls: HashMap<i64, Call>,
    item_ids: HashMap<i64, Vec<u8>>,
    item_types: HashMap<i64, Vec<u8>>,
    encrypted: HashMap<i64, Vec<u8>>,
    summaries: HashMap<i64, Vec<Vec<u8>>>,
    texts: HashMap<i64, Vec<u8>>,
    seq: i64,
    done: bool,
    terminal: bool,
    source_failed: bool,
    identities: HashMap<Vec<u8>, Identity>,
    pending_envelope_error: bool,
    pending_identity_errors: HashSet<i64>,
    item_identity_indexes: HashMap<Vec<u8>, i64>,
    call_identity_indexes: HashMap<Vec<u8>, i64>,
    antigravity: bool,
}

pub(crate) fn interactions_to_responses_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    let antigravity = is_antigravity(ctx.model.as_bytes());
    let source = identity_source(ctx);
    Box::new(ToResponses {
        model: ctx.model.to_owned(),
        original: ctx.original_request.to_vec(),
        translated: ctx.translated_request.to_vec(),
        identities: if source.is_empty() {
            HashMap::new()
        } else {
            tool_identity_map(source, antigravity)
        },
        antigravity,
        ..ToResponses::default()
    })
}

fn index_root(index: i64) -> Res<'static> {
    gj::parse(format!(r#"{{"index":{index}}}"#).as_bytes()).into_owned()
}

impl ToResponses {
    fn next_seq(&mut self) -> i64 {
        self.seq += 1;
        self.seq
    }

    fn is_patch(&self, name: &[u8]) -> bool {
        self.identities.get(name).is_some_and(|i| i.apply_patch)
    }

    /// interactionsHasPatchBridge.
    fn has_patch_bridge(&self) -> bool {
        self.identities.values().any(|i| i.apply_patch)
    }

    fn item_type(&self, index: i64) -> &[u8] {
        self.item_types.get(&index).map_or(&[][..], |t| t)
    }

    fn item_id(&self, index: i64) -> Vec<u8> {
        self.item_ids.get(&index).cloned().unwrap_or_default()
    }

    /// interactionsPatchFailure.
    fn failure(&mut self) -> Vec<Vec<u8>> {
        if self.terminal {
            return vec![];
        }
        self.tool_input_failed = true;
        self.terminal = true;
        let seq = self.next_seq();
        vec![sse_event("response.failed", &apply_patch::failure(&self.id, seq))]
    }

    /// interactionsPatchDelta.
    fn patch_delta(&mut self, index: i64, delta: &str) -> Vec<Vec<u8>> {
        if delta.is_empty() {
            return vec![];
        }
        let seq = self.next_seq();
        let payload = self.calls[&index]
            .patch
            .as_ref()
            .expect("patch call")
            .input_delta(delta, seq);
        vec![sse_event("response.custom_tool_call_input.delta", &payload)]
    }

    /// interactionsResolveStepIndex: the output index of a step event, recording identity
    /// conflicts; `Err` when an apply_patch call is involved in one.
    fn resolve_index(&mut self, explicit: &Res<'_>, step: &Res<'_>, fallback: i64) -> Result<i64, ()> {
        let step_index = step.get("index");
        let indexed = explicit.exists() || step_index.exists();
        let mut index = if explicit.exists() {
            explicit.int()
        } else if step_index.exists() {
            step_index.int()
        } else {
            fallback
        };
        let item_id = string(&step.get("id"));
        let call_id = string(&step.get("call_id"));
        let mut matched: HashSet<i64> = HashSet::new();
        if !item_id.is_empty()
            && let Some(&i) = self.item_identity_indexes.get(&item_id)
        {
            matched.insert(i);
        }
        if !call_id.is_empty()
            && let Some(&i) = self.call_identity_indexes.get(&call_id)
        {
            matched.insert(i);
        }
        if !indexed && let Some(&min) = matched.iter().min() {
            index = min;
        }
        for (&i, call) in &self.calls {
            let by_item = !item_id.is_empty() && (call.item_id_seen || call.added) && item_id == call.id;
            let by_call = !call_id.is_empty() && (call.call_id_seen || call.added) && call_id == call.call_id;
            if by_item || by_call {
                if !indexed && (matched.is_empty() || i < index) {
                    index = i;
                }
                matched.insert(i);
            }
        }
        if !indexed && matched.is_empty() {
            let found = self
                .item_ids
                .iter()
                .filter(|(_, id)| !item_id.is_empty() && **id == item_id)
                .map(|(&i, _)| i)
                .min();
            match found {
                Some(i) => index = i,
                None if !item_id.is_empty() || !call_id.is_empty() => {
                    while self.calls.contains_key(&index) || !self.item_type(index).is_empty() {
                        index += 1;
                    }
                }
                None => {}
            }
        }
        let mut related: HashSet<i64> = HashSet::from([index]);
        if explicit.exists() {
            related.insert(explicit.int());
        }
        if step_index.exists() {
            related.insert(step_index.int());
        }
        let mut conflict =
            matched.len() > 1 || (explicit.exists() && step_index.exists() && explicit.int() != step_index.int());
        for &i in &matched {
            related.insert(i);
            if (explicit.exists() && explicit.int() != i) || (step_index.exists() && step_index.int() != i) {
                conflict = true;
            }
        }
        let mut patch_related = self.is_patch(&step.get("name").bytes());
        for i in &related {
            if let Some(call) = self.calls.get(i) {
                patch_related = patch_related || call.patch.is_some() || self.is_patch(&call.raw_name);
                if (!item_id.is_empty() && call.item_id_seen && item_id != call.id)
                    || (!call_id.is_empty() && call.call_id_seen && call_id != call.call_id)
                {
                    conflict = true;
                }
            }
        }
        if conflict {
            for i in &related {
                self.pending_identity_errors.insert(*i);
                if let Some(call) = self.calls.get_mut(i) {
                    call.pending_error = true;
                }
            }
        }
        if !item_id.is_empty() {
            self.item_identity_indexes.entry(item_id).or_insert(index);
        }
        if !call_id.is_empty() {
            self.call_identity_indexes.entry(call_id).or_insert(index);
        }
        if patch_related && related.iter().any(|i| self.pending_identity_errors.contains(i)) {
            return Err(());
        }
        Ok(index)
    }

    /// responsesCreatedEvent.
    fn created(&mut self, root: &Res<'_>) -> Vec<u8> {
        let mut payload = br#"{"type":"response.created","response":{"id":"","object":"response","status":"in_progress","model":"","output":[]}}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut payload, "sequence_number", seq);
        let id = first_nonblank(&[&root.get("interaction.id").bytes(), &root.get("id").bytes()]);
        if !id.is_empty() {
            self.id = id.clone();
        }
        gj::set_str(&mut payload, "response.id", &id);
        gj::set_str(&mut payload, "response.model", &self.model);
        let environment = first_nonblank(&[
            &root.get("interaction.environment_id").bytes(),
            &root.get("environment_id").bytes(),
            &root.get("environment.id").bytes(),
            &root.get("interaction.environment.id").bytes(),
        ]);
        if !environment.is_empty() {
            self.environment = environment.clone();
            gj::set_str(&mut payload, "response.environment_id", &environment);
        }
        let mut request_model = request_model_name(&self.original, &self.translated);
        if request_model.is_empty() {
            request_model = self.model.as_bytes().to_vec();
        }
        if !request_model.is_empty() {
            gj::set_str(&mut payload, "response.model", &request_model);
        }
        sse_event("response.created", &payload)
    }

    /// interactionsStepStartToResponses.
    fn step_start(&mut self, root: &Res<'_>) -> Vec<Vec<u8>> {
        let step = root.get("step");
        let Ok(index) = self.resolve_index(&root.get("index"), &step, 0) else {
            return self.failure();
        };
        let kind = string(&step.get("type"));
        let bridge = self.has_patch_bridge();
        if let Some(call) = self.calls.get(&index)
            && (call.patch.is_some() || self.is_patch(&call.raw_name) || (call.raw_name.is_empty() && bridge))
        {
            return self.update_call(index, &step, true);
        }
        let fallback = format!("item_{index}").into_bytes();
        let item_id = first_nonblank(&[&step.get("id").bytes(), &step.get("call_id").bytes(), &fallback]);
        if kind == b"function_call" {
            return self.update_call(index, &step, true);
        }
        self.item_ids.insert(index, item_id.clone());
        self.item_types.insert(index, kind.clone());
        match kind.as_slice() {
            b"model_output" => {
                let mut added = br#"{"type":"response.output_item.added","output_index":0,"item":{"id":"","type":"message","status":"in_progress","role":"assistant","content":[]}}"#.to_vec();
                let seq = self.next_seq();
                gj::set_int(&mut added, "sequence_number", seq);
                gj::set_int(&mut added, "output_index", index);
                gj::set_str(&mut added, "item.id", &item_id);
                let mut part = br#"{"type":"response.content_part.added","output_index":0,"content_index":0,"item_id":"","part":{"type":"output_text","text":""}}"#.to_vec();
                let seq = self.next_seq();
                gj::set_int(&mut part, "sequence_number", seq);
                gj::set_int(&mut part, "output_index", index);
                gj::set_str(&mut part, "item_id", &item_id);
                vec![
                    sse_event("response.output_item.added", &added),
                    sse_event("response.content_part.added", &part),
                ]
            }
            b"thought" => {
                let mut added = br#"{"type":"response.output_item.added","output_index":0,"item":{"id":"","type":"reasoning","status":"in_progress","encrypted_content":"","summary":[]}}"#.to_vec();
                let seq = self.next_seq();
                gj::set_int(&mut added, "sequence_number", seq);
                gj::set_int(&mut added, "output_index", index);
                gj::set_str(&mut added, "item.id", &item_id);
                let signature = encrypted_content(&self.encrypted.get(&index).cloned().unwrap_or_default());
                if !signature.is_empty() {
                    gj::set_str(&mut added, "item.encrypted_content", &signature);
                }
                vec![sse_event("response.output_item.added", &added)]
            }
            _ => vec![],
        }
    }

    fn arguments_delta_event(&mut self, index: i64, item_id: &[u8], arguments: &[u8]) -> Vec<u8> {
        let mut payload =
            br#"{"type":"response.function_call_arguments.delta","output_index":0,"item_id":"","delta":""}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut payload, "sequence_number", seq);
        gj::set_int(&mut payload, "output_index", index);
        gj::set_str(&mut payload, "item_id", item_id);
        gj::set_str_no_html(&mut payload, "delta", arguments);
        sse_event("response.function_call_arguments.delta", &payload)
    }

    /// interactionsStepDeltaToResponses.
    fn step_delta(&mut self, root: &Res<'_>) -> Vec<Vec<u8>> {
        let Ok(index) = self.resolve_index(&root.get("index"), &root.get("step"), 0) else {
            return self.failure();
        };
        let delta = root.get("delta");
        let arguments = string(&delta.get("arguments"));
        let delta_type = string(&delta.get("type"));
        let bridge = self.has_patch_bridge();
        let step_name_patch = self.is_patch(&root.get("step.name").bytes());
        if let Some(call) = self.calls.get(&index)
            && call.source_stopped
            && delta_type == b"arguments_delta"
            && !arguments.is_empty()
        {
            if call.patch.is_some() || self.is_patch(&call.raw_name) || step_name_patch {
                return self.failure();
            }
            if call.raw_name.is_empty() && bridge && !call.pending_error {
                self.calls.get_mut(&index).expect("present").pending_error = true;
            }
        }
        if root.get("step").is_object() {
            let mut updates = self.update_call(index, &root.get("step"), false);
            if self.terminal {
                return updates;
            }
            let mut raw = root.raw.to_vec();
            gj::delete(&mut raw, "step");
            gj::set_int(&mut raw, "index", index);
            updates.extend(self.step_delta(&gj::parse(&raw)));
            return updates;
        }
        match delta_type.as_slice() {
            b"thought_summary" => {
                let text = first_nonblank(&[&delta.get("content.text").bytes(), &delta.get("text").bytes()]);
                if !text.is_empty() {
                    self.summaries.entry(index).or_default().push(text.clone());
                }
                let mut payload =
                    br#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":""}"#.to_vec();
                let seq = self.next_seq();
                gj::set_int(&mut payload, "sequence_number", seq);
                gj::set_int(&mut payload, "output_index", index);
                gj::set_str(&mut payload, "delta", &text);
                vec![sse_event("response.reasoning_summary_text.delta", &payload)]
            }
            b"thought_signature" => {
                let signature = encrypted_content(&delta.get("signature").bytes());
                if !signature.is_empty() {
                    self.encrypted.insert(index, signature);
                }
                vec![]
            }
            b"arguments_delta" => {
                let invalid = delta.get("invalid_json_str").exists();
                let call = self.calls.entry(index).or_default();
                if invalid {
                    call.pending_error = true;
                    let patch =
                        call.patch.is_some() || self.identities.get(&call.raw_name).is_some_and(|i| i.apply_patch);
                    if patch {
                        return self.failure();
                    }
                }
                if let Some(patch) = &mut call.patch {
                    if call.item_done && !arguments.is_empty() {
                        return self.failure();
                    }
                    call.arguments.extend_from_slice(&arguments);
                    let pushed = patch.push_arguments(&arguments);
                    return match pushed {
                        Ok(delta) => self.patch_delta(index, &delta),
                        Err(_) => self.failure(),
                    };
                }
                if call.item_done {
                    return vec![];
                }
                call.arguments.extend_from_slice(&arguments);
                let collect =
                    call.raw_name.is_empty() || self.identities.get(&call.raw_name).is_some_and(|i| i.apply_patch);
                if !call.source_stopped && collect {
                    call.fragments.push(arguments.clone());
                }
                if call.raw_name.is_empty() || call.custom {
                    return vec![];
                }
                let item_id = self.item_id(index);
                vec![self.arguments_delta_event(index, &item_id, &arguments)]
            }
            _ => {
                let mut payload = br#"{"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"","delta":""}"#.to_vec();
                let seq = self.next_seq();
                gj::set_int(&mut payload, "sequence_number", seq);
                gj::set_int(&mut payload, "output_index", index);
                gj::set_str(&mut payload, "item_id", self.item_id(index));
                let text = string(&delta.get("text"));
                if !text.is_empty() {
                    self.texts.entry(index).or_default().extend_from_slice(&text);
                }
                gj::set_str(&mut payload, "delta", &text);
                vec![sse_event("response.output_text.delta", &payload)]
            }
        }
    }

    /// responsesReasoningItem.
    fn reasoning_item(&self, index: i64) -> Vec<u8> {
        let mut item = br#"{"id":"","type":"reasoning","encrypted_content":"","summary":[]}"#.to_vec();
        gj::set_str(&mut item, "id", self.item_id(index));
        let signature = encrypted_content(&self.encrypted.get(&index).cloned().unwrap_or_default());
        if !signature.is_empty() {
            gj::set_str(&mut item, "encrypted_content", &signature);
        }
        if let Some(summaries) = self.summaries.get(&index) {
            gj::set_items(&mut item, "summary", &summary_parts(summaries));
        }
        item
    }

    /// responsesCompletedOutputItem.
    fn completed_item(&self, index: i64, kind: &[u8]) -> Option<Vec<u8>> {
        match kind {
            b"model_output" => {
                let mut item =
                    br#"{"id":"","type":"message","status":"completed","role":"assistant","content":[]}"#.to_vec();
                gj::set_str(&mut item, "id", self.item_id(index));
                if let Some(text) = self.texts.get(&index).filter(|t| !t.is_empty()) {
                    let mut part = br#"{"type":"output_text","text":""}"#.to_vec();
                    gj::set_str(&mut part, "text", text);
                    gj::set_items(&mut item, "content", &[part]);
                }
                Some(item)
            }
            b"thought" => Some(self.reasoning_item(index)),
            b"function_call" => {
                let item_id = self.item_id(index);
                let call = self.calls.get(&index);
                if let Some(call) = call.filter(|c| c.custom) {
                    let mut item = br#"{"id":"","type":"custom_tool_call","call_id":"","name":"","input":"","status":"completed"}"#.to_vec();
                    gj::set_str(&mut item, "id", &item_id);
                    gj::set_str(&mut item, "call_id", &call.call_id);
                    if !call.namespace.is_empty() {
                        gj::set_str(&mut item, "namespace", &call.namespace);
                    }
                    gj::set_str(&mut item, "name", &call.name);
                    let input = match &call.patch {
                        Some(patch) => patch.decoder.input().as_bytes().to_vec(),
                        None => unwrap_custom_tool_input(&call_arguments(call)),
                    };
                    gj::set_str(&mut item, "input", &input);
                    return Some(item);
                }
                let mut item =
                    br#"{"id":"","type":"function_call","call_id":"","name":"","arguments":"{}","status":"completed"}"#
                        .to_vec();
                gj::set_str(&mut item, "id", &item_id);
                gj::set_str(&mut item, "call_id", &item_id);
                if let Some(call) = call {
                    gj::set_str(&mut item, "call_id", &call.call_id);
                    if !call.namespace.is_empty() {
                        gj::set_str(&mut item, "namespace", &call.namespace);
                    }
                    gj::set_str(&mut item, "name", &call.name);
                    gj::set_str_no_html(&mut item, "arguments", call_arguments(call));
                }
                Some(item)
            }
            _ => None,
        }
    }

    /// interactionsStepStopToResponses.
    fn step_stop(&mut self, root: &Res<'_>) -> Vec<Vec<u8>> {
        let Ok(index) = self.resolve_index(&root.get("index"), &root.get("step"), 0) else {
            return self.failure();
        };
        if let Some(call) = self.calls.get_mut(&index) {
            call.source_stopped = true;
            if call.raw_name.is_empty() {
                call.stop_pending = true;
            }
        }
        let mut updates = vec![];
        if self.item_type(index) == b"function_call" && root.get("step").is_object() {
            updates = self.update_call(index, &root.get("step"), false);
            if self.terminal {
                return updates;
            }
        }
        let item_id = self.item_id(index);
        match self.item_type(index).to_vec().as_slice() {
            b"model_output" => {
                let text = self.texts.get(&index).cloned().unwrap_or_default();
                let mut text_done = br#"{"type":"response.output_text.done","output_index":0,"content_index":0,"item_id":"","text":"","logprobs":[]}"#.to_vec();
                let seq = self.next_seq();
                gj::set_int(&mut text_done, "sequence_number", seq);
                gj::set_int(&mut text_done, "output_index", index);
                gj::set_str(&mut text_done, "item_id", &item_id);
                gj::set_str(&mut text_done, "text", &text);
                let mut part = br#"{"type":"response.content_part.done","output_index":0,"content_index":0,"item_id":"","part":{"type":"output_text","text":""}}"#.to_vec();
                let seq = self.next_seq();
                gj::set_int(&mut part, "sequence_number", seq);
                gj::set_int(&mut part, "output_index", index);
                gj::set_str(&mut part, "item_id", &item_id);
                gj::set_str(&mut part, "part.text", &text);
                let mut done = br#"{"type":"response.output_item.done","output_index":0,"item":{"id":"","type":"message","status":"completed","role":"assistant","content":[]}}"#.to_vec();
                let seq = self.next_seq();
                gj::set_int(&mut done, "sequence_number", seq);
                gj::set_int(&mut done, "output_index", index);
                gj::set_str(&mut done, "item.id", &item_id);
                let mut output_text = br#"{"type":"output_text","text":""}"#.to_vec();
                gj::set_str(&mut output_text, "text", &text);
                gj::set_raw(&mut done, "item.content.-1", output_text);
                vec![
                    sse_event("response.output_text.done", &text_done),
                    sse_event("response.content_part.done", &part),
                    sse_event("response.output_item.done", &done),
                ]
            }
            b"function_call" => {
                let call = self.calls.entry(index).or_insert_with(|| Call {
                    id: item_id.clone(),
                    source_stopped: true,
                    ..Call::default()
                });
                if call.raw_name.is_empty() {
                    call.stop_pending = true;
                    return updates;
                }
                let patch_name = self.identities.get(&call.raw_name).is_some_and(|i| i.apply_patch);
                if patch_name && call.patch.is_none() {
                    call.stop_pending = true;
                    return updates;
                }
                if call.item_done {
                    return updates;
                }
                let mut events = updates;
                if let Some(patch) = &mut call.patch {
                    let args = if call.has_snapshot {
                        call.snapshot_arguments.clone()
                    } else {
                        call.arguments.clone()
                    };
                    if call.has_snapshot && gj::valid(&call.arguments) {
                        match CallState::default().finish_arguments(&call.arguments) {
                            Ok((_, input)) if input == call.snapshot_input => {}
                            _ => return self.failure(),
                        }
                    }
                    let finished = patch.finish_arguments(&args);
                    let Ok((tail, input)) = finished else {
                        return self.failure();
                    };
                    events.extend(self.patch_delta(index, &tail));
                    let seq = self.next_seq();
                    let input_done = self.calls[&index]
                        .patch
                        .as_ref()
                        .expect("patch")
                        .input_done(&input, seq);
                    events.push(sse_event("response.custom_tool_call_input.done", &input_done));
                    let item = self
                        .completed_item(index, b"function_call")
                        .expect("function call item");
                    let mut done = br#"{"type":"response.output_item.done"}"#.to_vec();
                    let seq = self.next_seq();
                    gj::set_int(&mut done, "sequence_number", seq);
                    gj::set_int(&mut done, "output_index", index);
                    gj::set_raw(&mut done, "item", item);
                    let call = self.calls.get_mut(&index).expect("present");
                    call.item_done = true;
                    call.arguments_done = true;
                    events.push(sse_event("response.output_item.done", &done));
                    return events;
                }
                if call.custom {
                    let input = unwrap_custom_tool_input(&call.arguments);
                    let (call_id, namespace, name) = (call.call_id.clone(), call.namespace.clone(), call.name.clone());
                    if !call.arguments_done {
                        call.arguments_done = true;
                        let mut payload = br#"{"type":"response.custom_tool_call_input.done","output_index":0,"item_id":"","input":""}"#.to_vec();
                        let seq = self.next_seq();
                        gj::set_int(&mut payload, "sequence_number", seq);
                        gj::set_int(&mut payload, "output_index", index);
                        gj::set_str(&mut payload, "item_id", &item_id);
                        gj::set_str(&mut payload, "input", &input);
                        events.push(sse_event("response.custom_tool_call_input.done", &payload));
                    }
                    let mut done = br#"{"type":"response.output_item.done","output_index":0,"item":{"id":"","type":"custom_tool_call","call_id":"","name":"","input":"","status":"completed"}}"#.to_vec();
                    let seq = self.next_seq();
                    gj::set_int(&mut done, "sequence_number", seq);
                    gj::set_int(&mut done, "output_index", index);
                    gj::set_str(&mut done, "item.id", &item_id);
                    gj::set_str(&mut done, "item.call_id", &call_id);
                    if !namespace.is_empty() {
                        gj::set_str(&mut done, "item.namespace", &namespace);
                    }
                    gj::set_str(&mut done, "item.name", &name);
                    gj::set_str(&mut done, "item.input", &input);
                    self.calls.get_mut(&index).expect("present").item_done = true;
                    events.push(sse_event("response.output_item.done", &done));
                    return events;
                }
                let arguments = call_arguments(call);
                let (call_id, namespace, name) = (call.call_id.clone(), call.namespace.clone(), call.name.clone());
                if !call.arguments_done {
                    call.arguments_done = true;
                    let mut payload = br#"{"type":"response.function_call_arguments.done","output_index":0,"item_id":"","arguments":""}"#.to_vec();
                    let seq = self.next_seq();
                    gj::set_int(&mut payload, "sequence_number", seq);
                    gj::set_int(&mut payload, "output_index", index);
                    gj::set_str(&mut payload, "item_id", &item_id);
                    gj::set_str_no_html(&mut payload, "arguments", &arguments);
                    events.push(sse_event("response.function_call_arguments.done", &payload));
                }
                let mut done = br#"{"type":"response.output_item.done","output_index":0,"item":{"id":"","type":"function_call","call_id":"","name":"","arguments":"","status":"completed"}}"#.to_vec();
                let seq = self.next_seq();
                gj::set_int(&mut done, "sequence_number", seq);
                gj::set_int(&mut done, "output_index", index);
                gj::set_str(&mut done, "item.id", &item_id);
                gj::set_str(&mut done, "item.call_id", &call_id);
                if !namespace.is_empty() {
                    gj::set_str(&mut done, "item.namespace", &namespace);
                }
                gj::set_str(&mut done, "item.name", &name);
                gj::set_str_no_html(&mut done, "item.arguments", &arguments);
                self.calls.get_mut(&index).expect("present").item_done = true;
                events.push(sse_event("response.output_item.done", &done));
                events
            }
            _ => {
                let mut done = br#"{"type":"response.output_item.done","output_index":0,"item":{}}"#.to_vec();
                let seq = self.next_seq();
                gj::set_int(&mut done, "sequence_number", seq);
                gj::set_int(&mut done, "output_index", index);
                gj::set_raw(&mut done, "item", self.reasoning_item(index));
                vec![sse_event("response.output_item.done", &done)]
            }
        }
    }

    /// interactionsUpdateFunctionCall: merges a function_call step's identity and
    /// arguments, announcing the output item once its identity is known.
    fn update_call(&mut self, index: i64, step: &Res<'_>, initial: bool) -> Vec<Vec<u8>> {
        let step_name = string(&step.get("name"));
        let step_name_patch = self.is_patch(&step_name);
        let identities = &self.identities;
        let call = self.calls.entry(index).or_default();
        let kind = step.get("type");
        if kind.exists() && kind.bytes().as_ref() != b"function_call" {
            call.pending_error = true;
        }
        let raw_name_patch = identities.get(&call.raw_name).is_some_and(|i| i.apply_patch);
        for (value, item) in [(string(&step.get("id")), true), (string(&step.get("call_id")), false)] {
            if value.is_empty() {
                continue;
            }
            let (target, seen) = if item {
                (&mut call.id, &mut call.item_id_seen)
            } else {
                (&mut call.call_id, &mut call.call_id_seen)
            };
            if (*seen || (call.added && !raw_name_patch)) && *target != value {
                call.pending_error = true;
                continue;
            }
            *target = value;
            *seen = true;
        }
        if !step_name.is_empty() {
            if !call.raw_name.is_empty() && call.raw_name != step_name {
                call.pending_error = true;
            } else {
                call.raw_name = step_name.clone();
            }
        }
        if call.pending_error && step_name_patch {
            return self.failure();
        }
        let args = step.get("arguments");
        if args.exists()
            && !(initial
                && !call.has_snapshot
                && call.arguments.is_empty()
                && !call.item_done
                && trim_space(&json_string_value(&args, b"")) == b"{}")
        {
            let arguments = json_string_value(&args, b"{}");
            if !call.added && call.initial_arguments.is_empty() && call.arguments.is_empty() {
                call.initial_arguments = arguments.clone();
            }
            match CallState::default().finish_arguments(&arguments) {
                Err(_) => call.pending_error = true,
                Ok((_, input)) => {
                    if call.has_snapshot && input != call.snapshot_input {
                        call.pending_error = true;
                    }
                    if let Some(patch) = &call.patch
                        && call.item_done
                        && input != patch.decoder.input()
                    {
                        call.pending_error = true;
                    }
                    call.has_snapshot = true;
                    call.snapshot_input = input;
                    call.snapshot_arguments = arguments;
                }
            }
        }
        let call_id_now = call.id.clone();
        let raw_name = call.raw_name.clone();
        self.item_ids.insert(index, call_id_now);
        self.item_types.insert(index, b"function_call".to_vec());
        if raw_name.is_empty() {
            return vec![];
        }
        let identity = self.identities.get(&raw_name).cloned();
        let patch_identity = identity.as_ref().is_some_and(|i| i.apply_patch);
        let antigravity = self.antigravity;
        let envelope_error = self.pending_envelope_error;
        let call = self.calls.get_mut(&index).expect("present");
        match &identity {
            Some(identity) => {
                call.name = identity.name.clone();
                call.namespace = identity.namespace.clone();
                call.custom = identity.custom;
            }
            None => {
                call.name = if antigravity {
                    antigravity_name_to_client(&raw_name)
                } else {
                    raw_name.clone()
                };
            }
        }
        if patch_identity {
            if envelope_error || call.pending_error {
                return self.failure();
            }
            if !(call.item_id_seen && call.call_id_seen) && !call.identity_finalized {
                return vec![];
            }
        } else {
            call.fragments.clear();
            if !call.item_id_seen && !call.added {
                call.id = first_nonblank(&[&call.call_id, format!("item_{index}").as_bytes()]);
            }
            if !call.call_id_seen && !call.added {
                call.call_id = call.id.clone();
            }
        }
        let item_id = call.id.clone();
        self.item_ids.insert(index, item_id);
        let mut events = vec![];
        let call = self.calls.get_mut(&index).expect("present");
        let announced = !call.added;
        if announced {
            if !patch_identity && !call.initial_arguments.is_empty() {
                let fragments = std::mem::take(&mut call.arguments);
                call.arguments = [call.initial_arguments.as_slice(), &fragments].concat();
            }
            let (item_type, input_key) = if call.custom {
                ("custom_tool_call", "item.input")
            } else {
                ("function_call", "item.arguments")
            };
            let (id, call_id, name, namespace) = (
                call.id.clone(),
                call.call_id.clone(),
                call.name.clone(),
                call.namespace.clone(),
            );
            call.added = true;
            let mut added = br#"{"type":"response.output_item.added","item":{"status":"in_progress"}}"#.to_vec();
            let seq = self.next_seq();
            gj::set_int(&mut added, "sequence_number", seq);
            gj::set_int(&mut added, "output_index", index);
            gj::set_str(&mut added, "item.type", item_type);
            gj::set_str(&mut added, input_key, "");
            gj::set_str(&mut added, "item.id", &id);
            gj::set_str(&mut added, "item.call_id", &call_id);
            gj::set_str(&mut added, "item.name", &name);
            if namespace.is_empty() {
                gj::delete(&mut added, "item.namespace");
            } else {
                gj::set_str(&mut added, "item.namespace", &namespace);
            }
            events.push(sse_event("response.output_item.added", &added));
        }
        let call = self.calls.get_mut(&index).expect("present");
        if patch_identity && call.patch.is_none() {
            call.patch = Some(CallState {
                item_id: call.id.clone(),
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                namespace: call.namespace.clone(),
                output_index: index,
                ..CallState::default()
            });
            for fragment in std::mem::take(&mut call.fragments) {
                let pushed = self
                    .calls
                    .get_mut(&index)
                    .and_then(|c| c.patch.as_mut())
                    .expect("patch")
                    .push_arguments(&fragment);
                match pushed {
                    Ok(delta) => events.extend(self.patch_delta(index, &delta)),
                    Err(_) => {
                        events.extend(self.failure());
                        return events;
                    }
                }
            }
        } else if !call.custom && announced && !call.arguments.is_empty() {
            let (id, arguments) = (call.id.clone(), call.arguments.clone());
            events.push(self.arguments_delta_event(index, &id, &arguments));
        }
        let call = self.calls.get_mut(&index).expect("present");
        if call.stop_pending && call.patch.is_some() {
            call.stop_pending = false;
            events.extend(self.step_stop(&index_root(index)));
        }
        events
    }

    /// interactionsFinishPatchCalls.
    fn finish_patch_calls(&mut self) -> Vec<Vec<u8>> {
        if self.has_patch_bridge() && self.calls.values().any(|c| c.raw_name.is_empty()) {
            return self.failure();
        }
        let mut indexes: Vec<i64> = self
            .calls
            .iter()
            .filter(|(_, c)| self.is_patch(&c.raw_name) && !c.item_done)
            .map(|(&i, _)| i)
            .collect();
        indexes.sort_unstable();
        let mut events = vec![];
        for index in indexes {
            let call = self.calls.get_mut(&index).expect("present");
            if call.patch.is_none() {
                if !call.item_id_seen {
                    call.id = first_nonblank(&[&call.call_id, format!("item_{index}").as_bytes()]);
                }
                if !call.call_id_seen {
                    call.call_id = call.id.clone();
                }
                call.identity_finalized = true;
                events.extend(self.update_call(index, &Res::default(), false));
                if self.terminal {
                    break;
                }
            }
            events.extend(self.step_stop(&index_root(index)));
            if self.terminal {
                break;
            }
        }
        events
    }

    /// responsesCompletedEvent.
    fn completed(&mut self, root: &Res<'_>) -> Vec<u8> {
        let interaction = root.get("interaction");
        let status = first_nonblank(&[&interaction.get("status").bytes(), &root.get("status").bytes()]);
        let reason = first_nonblank(&[
            &interaction.get("finish_reason").bytes(),
            &root.get("finish_reason").bytes(),
        ]);
        let incomplete = incomplete_reason(&status, &reason);
        let event_type = if incomplete.is_some() {
            "response.incomplete"
        } else {
            "response.completed"
        };
        let mut payload = br#"{"type":"response.completed","response":{"id":"","object":"response","status":"completed","model":"","output":[],"usage":{}}}"#.to_vec();
        gj::set_str(&mut payload, "type", event_type);
        gj::set_str(
            &mut payload,
            "response.status",
            if incomplete.is_some() {
                "incomplete"
            } else {
                "completed"
            },
        );
        if let Some(reason) = incomplete {
            gj::set_str(&mut payload, "response.incomplete_details.reason", reason);
        }
        let seq = self.next_seq();
        gj::set_int(&mut payload, "sequence_number", seq);
        gj::set_str(
            &mut payload,
            "response.id",
            first_nonblank(&[&interaction.get("id").bytes(), &root.get("id").bytes()]),
        );
        gj::set_str(
            &mut payload,
            "response.model",
            first_nonblank(&[&interaction.get("model").bytes(), self.model.as_bytes()]),
        );
        let mut environment = first_nonblank(&[
            &interaction.get("environment_id").bytes(),
            &root.get("environment_id").bytes(),
            &interaction.get("environment.id").bytes(),
            &root.get("environment.id").bytes(),
        ]);
        if environment.is_empty() {
            environment = self.environment.clone();
        }
        if !environment.is_empty() {
            gj::set_str(&mut payload, "response.environment_id", &environment);
        }
        let max = self.item_types.keys().copied().max().unwrap_or(-1);
        let mut items = vec![];
        for index in 0..=max {
            if let Some(kind) = self.item_types.get(&index) {
                items.extend(self.completed_item(index, kind));
            }
        }
        gj::set_items(&mut payload, "response.output", &items);
        set_responses_usage(&mut payload, "response.usage", &interactions_usage(root));
        sse_event(event_type, &payload)
    }

    /// responsesFailedEvent.
    fn failed(&mut self, root: &Res<'_>) -> Vec<u8> {
        let mut payload = br#"{"type":"response.failed","response":{"id":"","object":"response","status":"failed","model":"","output":[],"error":{"message":"","code":"","type":"server_error"}}}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut payload, "sequence_number", seq);
        let interaction = root.get("interaction");
        let mut id = first_nonblank(&[&interaction.get("id").bytes(), &root.get("id").bytes()]);
        if id.is_empty() {
            id = self.id.clone();
        }
        gj::set_str(&mut payload, "response.id", &id);
        gj::set_str(
            &mut payload,
            "response.model",
            first_nonblank(&[&interaction.get("model").bytes(), self.model.as_bytes()]),
        );
        let mut error = root.get("error");
        if !error.exists() && interaction.exists() {
            error = interaction.get("error");
        }
        let mut message = string(&error.get("message"));
        if message.is_empty() {
            message = b"upstream execution failed".to_vec();
        }
        gj::set_str(&mut payload, "response.error.message", &message);
        let code = string(&error.get("code"));
        if code.is_empty() {
            gj::delete(&mut payload, "response.error.code");
        } else {
            gj::set_str(&mut payload, "response.error.code", &code);
        }
        let mut kind = string(&error.get("type"));
        if kind.is_empty() {
            kind = b"server_error".to_vec();
        }
        gj::set_str(&mut payload, "response.error.type", &kind);
        sse_event("response.failed", &payload)
    }

    /// The `interaction.completed` / `finish` event: settles apply_patch calls named in the
    /// final steps, then completes the response.
    fn interaction_completed(&mut self, root: &Res<'_>) -> Vec<Vec<u8>> {
        let mut events = vec![];
        let steps = first_existing(root, &["interaction.steps", "steps"]);
        let bridge = self.has_patch_bridge();
        steps.each(|key, step| {
            let Ok(index) = self.resolve_index(&step.get("index"), &step, key.int()) else {
                events.extend(self.failure());
                return false;
            };
            let step_name = step.get("name").bytes();
            let step_name_patch = self.is_patch(&step_name);
            let call = self.calls.get(&index);
            let skip_call = call.is_none_or(|c| {
                c.patch.is_none() && !self.is_patch(&c.raw_name) && (!bridge || !c.raw_name.is_empty())
            });
            if skip_call {
                if step.get("type").bytes().as_ref() != b"function_call" {
                    return true;
                }
                let unresolved = bridge && (step_name.is_empty() || call.is_some_and(|c| c.raw_name.is_empty()));
                if !step_name_patch && !unresolved {
                    return true;
                }
            }
            events.extend(self.update_call(index, &step, false));
            !self.terminal
        });
        if self.terminal {
            return events;
        }
        events.extend(self.finish_patch_calls());
        if self.terminal {
            return events;
        }
        self.terminal = true;
        events.push(self.completed(root));
        events
    }
}

/// responsesFunctionCallArguments.
fn call_arguments(call: &Call) -> Vec<u8> {
    if call.arguments.is_empty() {
        b"{}".to_vec()
    } else {
        call.arguments.clone()
    }
}

impl GoStream for ToResponses {
    /// ConvertInteractionsResponseToOpenAIResponses.
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        if self.done || self.tool_input_failed || self.source_failed {
            return Ok(vec![]);
        }
        let payload = sse_payload(line);
        if payload.is_empty() {
            return Ok(vec![]);
        }
        let valid = gj::valid(&payload);
        if payload == b"[DONE]" || (valid && gj::get(&payload, "event_type").bytes().as_ref() == b"done") {
            let mut events = self.finish_patch_calls();
            if self.tool_input_failed {
                return Ok(events);
            }
            self.done = true;
            self.terminal = true;
            events.push(b"data: [DONE]".to_vec());
            return Ok(events);
        }
        if self.terminal {
            return Ok(vec![]);
        }
        let root = gj::parse(&payload);
        if !root.exists() {
            return Ok(vec![]);
        }
        let event_type = string(&root.get("event_type"));
        if !valid {
            let step = root.get("step");
            let Ok(index) = self.resolve_index(&root.get("index"), &step, 0) else {
                return Ok(self.failure());
            };
            if event_type.starts_with(b"step.") {
                let step_name_patch = self.is_patch(&step.get("name").bytes());
                let call = self.calls.entry(index).or_insert_with(|| {
                    let id = string(&step.get("id"));
                    let call_id = string(&step.get("call_id"));
                    Call {
                        item_id_seen: !id.is_empty(),
                        call_id_seen: !call_id.is_empty(),
                        id,
                        call_id,
                        ..Call::default()
                    }
                });
                call.pending_error = true;
                let raw_name_patch = self.identities.get(&call.raw_name).is_some_and(|i| i.apply_patch);
                if raw_name_patch || step_name_patch {
                    return Ok(self.failure());
                }
            } else {
                self.pending_envelope_error = true;
                if self.calls.values().any(|c| c.patch.is_some()) {
                    return Ok(self.failure());
                }
            }
        }
        Ok(match event_type.as_slice() {
            b"interaction.created" => vec![self.created(&root)],
            b"step.start" => self.step_start(&root),
            b"step.delta" => self.step_delta(&root),
            b"step.stop" => self.step_stop(&root),
            b"interaction.completed" | b"finish" => self.interaction_completed(&root),
            b"response.failed" | b"interaction.failed" => {
                if self.has_patch_bridge() {
                    self.failure()
                } else {
                    self.source_failed = true;
                    self.terminal = true;
                    vec![self.failed(&root)]
                }
            }
            _ => vec![],
        })
    }

    fn tool_input_failed(&self) -> bool {
        self.tool_input_failed
    }

    /// FinalizeToolInput: a stream with the apply_patch bridge that ends without its
    /// terminator fails.
    fn finalize_tool_input(&mut self) -> Vec<Vec<u8>> {
        if self.tool_input_failed || self.terminal || !self.has_patch_bridge() {
            return vec![];
        }
        self.tool_input_failed = true;
        self.terminal = true;
        let seq = self.next_seq();
        vec![sse_event("response.failed", &apply_patch::failure(&self.id, seq))]
    }
}

// ---------------------------------------------------------------------------------------
// Responses -> Interactions

/// setInteractionsUsageFromResponses.
fn set_interactions_usage(out: &mut Vec<u8>, path: &str, usage: &Res<'_>) {
    if !usage.exists() {
        return;
    }
    for (source, keys) in [
        ("input_tokens", &["input_tokens", "total_input_tokens"][..]),
        ("output_tokens", &["output_tokens", "total_output_tokens"]),
        ("total_tokens", &["total_tokens"]),
        (
            "input_tokens_details.cached_tokens",
            &["cached_tokens", "total_cached_tokens"],
        ),
        (
            "output_tokens_details.reasoning_tokens",
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

/// openAIResponsesOutputItemToInteractionsStep.
fn output_item_to_step(item: &Res<'_>, antigravity: bool) -> Option<Vec<u8>> {
    match item.get("type").bytes().as_ref() {
        b"message" => {
            let mut step = br#"{"type":"model_output","content":[]}"#.to_vec();
            item.get("content").each(|_, part| {
                if let Some(converted) = content_part(&part) {
                    gj::set_raw(&mut step, "content.-1", converted);
                }
                true
            });
            Some(step)
        }
        b"function_call" => Some(function_call_step(item, antigravity, false)),
        b"reasoning" => {
            let mut step = br#"{"type":"thought","content":[]}"#.to_vec();
            item.get("summary").each(|_, summary| {
                let text = summary.get("text").bytes();
                if !text.is_empty() {
                    let mut part = br#"{"type":"text","text":""}"#.to_vec();
                    gj::set_str(&mut part, "text", &text);
                    gj::set_raw(&mut step, "content.-1", part);
                }
                true
            });
            Some(step)
        }
        _ => None,
    }
}

/// ConvertOpenAIResponsesResponseToInteractionsNonStream.
pub(crate) fn responses_to_interactions_non_stream(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let root = gj::parse(body);
    let mut out = br#"{"id":"","object":"interaction","status":"completed","model":"","steps":[]}"#.to_vec();
    let status = string(&root.get("status"));
    if !status.is_empty() {
        gj::set_str(&mut out, "status", &status);
    }
    gj::set_str(&mut out, "id", root.get("id").bytes());
    gj::set_str(&mut out, "model", response_model(ctx.model, &root));
    let antigravity = is_antigravity(ctx.model.as_bytes());
    let mut steps = vec![];
    root.get("output").each(|_, item| {
        steps.extend(output_item_to_step(&item, antigravity));
        true
    });
    if !steps.is_empty() {
        gj::set_raw(&mut out, "steps", gj::join(&steps));
    }
    set_interactions_usage(&mut out, "usage", &root.get("usage"));
    Ok(out)
}

/// responsesToInteractionsStreamState. Go also records each call's step index by call
/// ID; nothing reads it back.
#[derive(Default)]
struct ToInteractions {
    model: String,
    id: Vec<u8>,
    created: bool,
    status_updated: bool,
    completed: bool,
    done: bool,
    next_step: i64,
    step_index: i64,
    step_type: &'static str,
    step_open: bool,
    sent_text: HashSet<String>,
    unkeyed_text: bool,
    args_sent: HashSet<String>,
}

pub(crate) fn responses_to_interactions_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    Box::new(ToInteractions {
        model: ctx.model.to_owned(),
        ..ToInteractions::default()
    })
}

const TEXT_DELTA: &[u8] = br#"{"index":0,"delta":{"text":"","type":"text"},"event_type":"step.delta"}"#;
const THOUGHT_DELTA: &[u8] =
    br#"{"index":0,"delta":{"content":{"text":"","type":"text"},"type":"thought_summary"},"event_type":"step.delta"}"#;
const ARGUMENTS_DELTA: &[u8] =
    br#"{"index":0,"delta":{"arguments":"","type":"arguments_delta"},"event_type":"step.delta"}"#;

/// openAIResponsesTextKeys.
fn text_keys(item_id: &[u8], output: Option<i64>, content: Option<i64>) -> Vec<String> {
    let Some(content) = content else {
        return vec![];
    };
    let mut keys = vec![];
    if !item_id.is_empty() {
        keys.push(format!("item:{}:content:{content}", String::from_utf8_lossy(item_id)));
    }
    if let Some(output) = output {
        keys.push(format!("output:{output}:content:{content}"));
    }
    keys.push(format!("content:{content}"));
    keys
}

/// openAIResponsesUnkeyedTextKeys.
fn unkeyed_text_keys(item_id: &[u8], output: Option<i64>) -> Vec<String> {
    let mut keys = vec![];
    if !item_id.is_empty() {
        keys.push(format!("item:{}", String::from_utf8_lossy(item_id)));
    }
    if let Some(output) = output {
        keys.push(format!("output:{output}"));
    }
    keys
}

fn optional_int(value: &Res<'_>) -> Option<i64> {
    value.exists().then(|| value.int())
}

/// textKeysFromResponsesEvent.
fn event_text_keys(root: &Res<'_>) -> Vec<String> {
    let item_id = root.get("item_id").bytes();
    let output = optional_int(&root.get("output_index"));
    match optional_int(&root.get("content_index")) {
        None => unkeyed_text_keys(&item_id, output),
        content => text_keys(&item_id, output, content),
    }
}

/// functionArgsKeysFromResponsesEvent.
fn event_args_keys(root: &Res<'_>) -> Vec<String> {
    let item = root.get("item");
    let mut keys: Vec<String> = vec![];
    for id in [
        root.get("item_id").bytes(),
        root.get("call_id").bytes(),
        item.get("call_id").bytes(),
        item.get("id").bytes(),
    ] {
        if id.is_empty() {
            continue;
        }
        let key = format!("item:{}", String::from_utf8_lossy(&id));
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    if let Some(output) = optional_int(&root.get("output_index")) {
        keys.push(format!("output:{output}"));
    }
    keys
}

impl ToInteractions {
    /// appendInteractionsCreatedDirect (with the status update).
    fn create(&mut self, response: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        if self.created {
            return;
        }
        let fallback = format!("interaction_{}", now_nanos()).into_bytes();
        self.id = first_nonblank(&[&response.get("id").bytes(), &self.id, &fallback]);
        let mut created = br#"{"interaction":{"id":"","status":"in_progress","object":"interaction","model":""},"event_type":"interaction.created"}"#.to_vec();
        gj::set_str(&mut created, "interaction.id", &self.id);
        gj::set_str(&mut created, "interaction.model", response_model(&self.model, response));
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

    /// appendInteractionsStepStartDirect.
    fn start_step(&mut self, kind: &'static str, step: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        self.step_index = self.next_step;
        self.next_step += 1;
        self.step_type = kind;
        self.step_open = true;
        let mut start = br#"{"index":0,"step":{"type":""},"event_type":"step.start"}"#.to_vec();
        gj::set_int(&mut start, "index", self.step_index);
        gj::set_str(&mut start, "step.type", kind);
        if kind == "function_call" {
            let id = first_nonblank(&[&step.get("call_id").bytes(), &step.get("id").bytes()]);
            if !id.is_empty() {
                gj::set_str(&mut start, "step.id", &id);
                gj::set_str(&mut start, "step.call_id", &id);
            }
            gj::set_str(&mut start, "step.name", step.get("name").bytes());
            gj::set_raw(&mut start, "step.arguments", b"{}");
        }
        out.push(sse_event("step.start", &start));
    }

    /// appendInteractionsStepStopDirect.
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

    /// ensureInteractionsStepDirect.
    fn ensure_step(&mut self, kind: &'static str, out: &mut Vec<Vec<u8>>) {
        self.create(&Res::default(), out);
        if self.step_open && self.step_type == kind {
            return;
        }
        self.stop_step(out);
        self.start_step(kind, &Res::default(), out);
    }

    fn delta(&self, template: &[u8], path: &str, value: &[u8], no_html: bool, out: &mut Vec<Vec<u8>>) {
        let mut delta = template.to_vec();
        gj::set_int(&mut delta, "index", self.step_index);
        if no_html {
            gj::set_str_no_html(&mut delta, path, value);
        } else {
            gj::set_str(&mut delta, path, value);
        }
        out.push(sse_event("step.delta", &delta));
    }

    fn text_delta(&self, text: &[u8], thought: bool, out: &mut Vec<Vec<u8>>) {
        if thought {
            self.delta(THOUGHT_DELTA, "delta.content.text", text, false, out);
        } else {
            self.delta(TEXT_DELTA, "delta.text", text, false, out);
        }
    }

    /// A function_call step named by the event's item (antigravity names renamed).
    fn call_step(&self, item: &Res<'_>, id: &[u8]) -> Vec<u8> {
        let mut step = br#"{"type":"function_call","name":"","arguments":{}}"#.to_vec();
        let mut name = string(&item.get("name"));
        if is_antigravity(self.model.as_bytes()) {
            name = antigravity_name_to_upstream(&name);
        }
        gj::set_str(&mut step, "name", &name);
        if !id.is_empty() {
            gj::set_str(&mut step, "id", id);
            gj::set_str(&mut step, "call_id", id);
        }
        step
    }

    /// ensureInteractionsFunctionCallStep.
    fn ensure_call_step(&mut self, root: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        if self.step_open && self.step_type == "function_call" {
            return;
        }
        let mut item = root.get("item");
        if !item.exists() {
            item = root.clone();
        }
        let id = first_nonblank(&[
            &item.get("call_id").bytes(),
            &item.get("id").bytes(),
            &root.get("call_id").bytes(),
            &root.get("item_id").bytes(),
        ]);
        let step = self.call_step(&item, &id);
        self.create(&Res::default(), out);
        self.stop_step(out);
        self.start_step("function_call", &gj::parse(&step), out);
    }

    fn mark_text_sent(&mut self, keys: Vec<String>) {
        if keys.is_empty() {
            self.unkeyed_text = true;
        } else {
            self.sent_text.extend(keys);
        }
    }

    /// appendResponsesMessageFallbackToInteractions: message text not already streamed.
    fn message_fallback(&mut self, item: &Res<'_>, root: &Res<'_>, stop: bool, out: &mut Vec<Vec<u8>>) {
        let item_id = string(&item.get("id"));
        let output = optional_int(&root.get("output_index"));
        item.get("content").each(|content_index, part| {
            let kind = part.get("type").bytes();
            if kind.as_ref() != b"output_text" && kind.as_ref() != b"text" {
                return true;
            }
            let content = optional_int(&content_index);
            let keys = text_keys(&item_id, output, content);
            let unkeyed = unkeyed_text_keys(&item_id, output);
            let sent = (content.is_none() && self.unkeyed_text) || keys.iter().any(|k| self.sent_text.contains(k));
            let sent_unkeyed = if unkeyed.is_empty() {
                self.unkeyed_text
            } else {
                unkeyed.iter().any(|k| self.sent_text.contains(k))
            };
            if sent || sent_unkeyed {
                return true;
            }
            let text = part.get("text").bytes();
            if text.is_empty() {
                return true;
            }
            self.ensure_step("model_output", out);
            self.text_delta(&text, false, out);
            self.mark_text_sent(keys);
            true
        });
        if stop {
            self.stop_step(out);
        }
    }

    /// appendInteractionsCompletedDirect.
    fn complete(&mut self, response: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        if self.completed {
            return;
        }
        let now = format_rfc3339_utc(now_unix());
        let mut completed = br#"{"interaction":{"id":"","status":"completed","usage":{},"created":"","updated":"","service_tier":"standard","object":"interaction","model":""},"event_type":"interaction.completed"}"#.to_vec();
        gj::set_str(&mut completed, "interaction.id", &self.id);
        gj::set_str(&mut completed, "interaction.created", &now);
        gj::set_str(&mut completed, "interaction.updated", &now);
        gj::set_str(
            &mut completed,
            "interaction.model",
            response_model(&self.model, response),
        );
        let status = string(&response.get("status"));
        if !status.is_empty() {
            gj::set_str(&mut completed, "interaction.status", &status);
        }
        set_interactions_usage(&mut completed, "interaction.usage", &response.get("usage"));
        out.push(sse_event("interaction.completed", &completed));
        self.completed = true;
    }

    fn finish(&mut self, out: &mut Vec<Vec<u8>>) {
        if !self.done {
            out.push(sse_event("done", b"[DONE]"));
            self.done = true;
        }
    }
}

impl GoStream for ToInteractions {
    /// ConvertOpenAIResponsesResponseToInteractions.
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let payload = sse_payload(line);
        let mut out = vec![];
        if payload.is_empty() {
            return Ok(out);
        }
        if payload == b"[DONE]" {
            self.finish(&mut out);
            return Ok(out);
        }
        let root = gj::parse(&payload);
        if !root.exists() {
            return Ok(out);
        }
        match root.get("type").bytes().as_ref() {
            b"response.created" => self.create(&root.get("response"), &mut out),
            b"response.output_text.delta" => {
                self.ensure_step("model_output", &mut out);
                self.text_delta(&root.get("delta").bytes(), false, &mut out);
                self.mark_text_sent(event_text_keys(&root));
            }
            b"response.reasoning_summary_text.delta" => {
                self.ensure_step("thought", &mut out);
                self.text_delta(&root.get("delta").bytes(), true, &mut out);
            }
            b"response.output_item.added" => {
                let item = root.get("item");
                match item.get("type").bytes().as_ref() {
                    b"function_call" => {
                        self.create(&Res::default(), &mut out);
                        self.stop_step(&mut out);
                        let id = first_nonblank(&[&item.get("call_id").bytes(), &item.get("id").bytes()]);
                        let step = self.call_step(&item, &id);
                        self.start_step("function_call", &gj::parse(&step), &mut out);
                    }
                    b"message" => self.ensure_step("model_output", &mut out),
                    b"reasoning" => self.ensure_step("thought", &mut out),
                    _ => {}
                }
            }
            b"response.function_call_arguments.delta" => {
                self.ensure_call_step(&root, &mut out);
                self.delta(
                    ARGUMENTS_DELTA,
                    "delta.arguments",
                    &root.get("delta").bytes(),
                    true,
                    &mut out,
                );
                self.args_sent.extend(event_args_keys(&root));
            }
            b"response.output_item.done" => {
                let item = root.get("item");
                match item.get("type").bytes().as_ref() {
                    b"function_call" => {
                        self.ensure_call_step(&root, &mut out);
                        let args = item.get("arguments");
                        let sent = event_args_keys(&root).iter().any(|k| self.args_sent.contains(k));
                        if args.exists() && !args.bytes().is_empty() && !sent {
                            let arguments = json_string_value(&args, b"{}");
                            self.delta(ARGUMENTS_DELTA, "delta.arguments", &arguments, true, &mut out);
                        }
                        self.stop_step(&mut out);
                    }
                    b"reasoning" => {
                        self.ensure_step("thought", &mut out);
                        item.get("summary").each(|_, summary| {
                            let text = summary.get("text").bytes();
                            if !text.is_empty() {
                                self.text_delta(&text, true, &mut out);
                            }
                            true
                        });
                        self.stop_step(&mut out);
                    }
                    b"message" => self.message_fallback(&item, &root, true, &mut out),
                    _ => {}
                }
            }
            b"response.completed" | b"response.incomplete" => {
                let response = root.get("response");
                response.get("output").each(|output_index, item| {
                    if item.get("type").bytes().as_ref() == b"message" {
                        let mut index_root = br#"{"output_index":0}"#.to_vec();
                        gj::set_int(&mut index_root, "output_index", output_index.int());
                        let id = item.get("id").bytes();
                        if !id.is_empty() {
                            gj::set_str(&mut index_root, "item_id", &id);
                        }
                        self.message_fallback(&item, &gj::parse(&index_root), false, &mut out);
                    }
                    true
                });
                self.stop_step(&mut out);
                self.complete(&response, &mut out);
                self.finish(&mut out);
            }
            _ => {}
        }
        Ok(out)
    }
}
