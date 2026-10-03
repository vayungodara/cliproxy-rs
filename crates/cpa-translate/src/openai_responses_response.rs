//! OpenAI Chat Completions responses -> OpenAI Responses responses
//! (internal/translator/openai/openai/responses/openai_openai-responses_response.go).
//!
//! Tool calls are keyed by `choice:tool` index. Calls to a declared custom tool become
//! `custom_tool_call` items; `apply_patch` arguments stream through the patch input
//! decoder, and invalid or conflicting patch calls end the response with
//! `response.failed` (Go's ToolInputError contract).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use cpa_common::json::{self as gj, AnyValue, Res};

use crate::apply_patch::{self, CallState};
use crate::claude_responses_response::{Echo, copy_request_fields, pick_request};
use crate::common::{now_nanos, now_unix, request_model_name, sse_event, trim_space};
use crate::openai_responses::{ToolIndex, unwrap_custom_tool_input};
use crate::stream::GoStream;
use crate::{Error, Registered, ResponseCtx};

pub static PAIR: Registered = registered!(
    OpenAIResponse -> OpenAI,
    request: |ctx, body| Ok(crate::openai_responses::convert(ctx.model, body, ctx.stream)),
    non_stream: non_stream,
    go_stream: go_stream,
    token_count: None,
);

static RESPONSE_IDS: AtomicU64 = AtomicU64::new(0);

/// incompleteByFinishReason.
fn incomplete_details(reason: &[u8]) -> Option<&'static [u8]> {
    match reason {
        b"length" | b"max_tokens" => Some(br#"{"reason":"max_output_tokens"}"#),
        b"content_filter" => Some(br#"{"reason":"content_filter"}"#),
        _ => None,
    }
}

fn status(reason: &[u8]) -> &'static str {
    if incomplete_details(reason).is_some() {
        "incomplete"
    } else {
        "completed"
    }
}

fn event(name: &str, payload: &[u8]) -> Vec<u8> {
    sse_event(name, payload)
}

fn with_prefix(prefix: &[u8], id: &[u8]) -> Vec<u8> {
    [prefix, id].concat()
}

#[derive(Default)]
struct State {
    original: Vec<u8>,
    translated: Vec<u8>,
    model: String,
    request: Vec<u8>,
    index: Option<ToolIndex>,
    tool_error: Option<String>,
    patch_calls: HashMap<String, CallState>,
    seq: i64,
    response_id: Vec<u8>,
    created: i64,
    started: bool,
    completed_emitted: bool,
    reasoning_id: Vec<u8>,
    reasoning_index: i64,
    msg_text: HashMap<i64, Vec<u8>>,
    reasoning_buf: Vec<u8>,
    /// (id, text, output index)
    reasonings: Vec<(Vec<u8>, Vec<u8>, i64)>,
    func_args: HashMap<String, Vec<u8>>,
    func_names: HashMap<String, Vec<u8>>,
    func_call_ids: HashMap<String, Vec<u8>>,
    func_conflicts: HashSet<String>,
    func_output_ix: HashMap<String, i64>,
    func_args_sent: HashMap<String, usize>,
    msg_output_ix: HashMap<i64, i64>,
    next_output_ix: i64,
    msg_item_added: HashSet<i64>,
    msg_content_added: HashSet<i64>,
    msg_item_done: HashSet<i64>,
    func_item_added: HashSet<String>,
    func_item_custom: HashMap<String, bool>,
    func_item_done: HashSet<String>,
    finish_reason: Vec<u8>,
    prompt_tokens: i64,
    cached_tokens: i64,
    completion_tokens: i64,
    total_tokens: i64,
    reasoning_tokens: i64,
    usage_seen: bool,
}

pub fn go_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    let request = pick_request(ctx.original_request, ctx.translated_request).to_vec();
    Box::new(State {
        original: ctx.original_request.to_vec(),
        translated: ctx.translated_request.to_vec(),
        model: ctx.model.to_owned(),
        index: Some(ToolIndex::from_raw(&request)),
        request,
        ..Default::default()
    })
}

impl State {
    fn index(&self) -> &ToolIndex {
        self.index.as_ref().expect("tool index is built with the stream")
    }

    fn next_seq(&mut self) -> i64 {
        self.seq += 1;
        self.seq
    }

    fn alloc_output_index(&mut self) -> i64 {
        let index = self.next_output_ix;
        self.next_output_ix += 1;
        index
    }

    fn fail(&mut self, err: String, out: &mut Vec<Vec<u8>>) {
        if self.tool_error.is_none() {
            self.tool_error = Some(err);
            let seq = self.next_seq();
            out.push(event("response.failed", &apply_patch::failure(&self.response_id, seq)));
        }
    }

    /// emitToolItem: output_item.added once the call has an ID and a name (or `force`).
    fn emit_tool_item(&mut self, key: &str, force: bool, out: &mut Vec<Vec<u8>>) {
        if self.func_item_added.contains(key) {
            return;
        }
        let mut call_id = self.func_call_ids.get(key).cloned().unwrap_or_default();
        let mut name = self
            .index()
            .canonical_name(&self.func_names.get(key).cloned().unwrap_or_default());
        self.func_names.insert(key.to_owned(), name.clone());
        if !force && (call_id.is_empty() || name.is_empty()) {
            return;
        }
        if name.is_empty() {
            let (custom, ok) = self.index().single_custom_name();
            if ok {
                name = custom;
                self.func_names.insert(key.to_owned(), name.clone());
            }
        }
        if self.index().is_apply_patch(&name) && self.func_conflicts.contains(key) {
            self.fail("conflicting apply_patch call identity".into(), out);
            return;
        }
        if call_id.is_empty() {
            call_id = format!(
                "call_{}_{}",
                String::from_utf8_lossy(&self.response_id),
                key.replace(':', "_")
            )
            .into_bytes();
            self.func_call_ids.insert(key.to_owned(), call_id.clone());
        }
        let output_index = self.func_output_ix.get(key).copied().unwrap_or(0);
        let custom = self.index().custom.contains(&name);
        self.func_item_custom.insert(key.to_owned(), custom);
        let (template, id): (&[u8], Vec<u8>) = if custom {
            if self.index().is_apply_patch(&name) {
                let d = &self.index().by_chat[&name];
                let call = CallState {
                    item_id: with_prefix(b"ctc_", &call_id),
                    call_id: call_id.clone(),
                    name: d.local_name.clone(),
                    namespace: d.namespace.clone(),
                    output_index,
                    ..Default::default()
                };
                self.patch_calls.insert(key.to_owned(), call);
            }
            (
                br#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"custom_tool_call","status":"in_progress","input":"","call_id":"","name":""}}"#,
                with_prefix(b"ctc_", &call_id),
            )
        } else {
            (
                br#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"function_call","status":"in_progress","arguments":"","call_id":"","name":""}}"#,
                with_prefix(b"fc_", &call_id),
            )
        };
        let mut payload = template.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut payload, "sequence_number", seq);
        gj::set_int(&mut payload, "output_index", output_index);
        gj::set_str(&mut payload, "item.id", &id);
        gj::set_str(&mut payload, "item.call_id", &call_id);
        self.index().apply_identity(&mut payload, &name, "item");
        out.push(event("response.output_item.added", &payload));
        self.func_item_added.insert(key.to_owned());
    }

    /// emitPendingFunctionArgs.
    fn emit_pending_args(&mut self, key: &str, out: &mut Vec<Vec<u8>>) {
        if !self.func_item_added.contains(key) || self.tool_error.is_some() {
            return;
        }
        let sent = self.func_args_sent.get(key).copied().unwrap_or(0);
        let Some(args) = self.func_args.get(key).filter(|a| a.len() > sent).cloned() else {
            return;
        };
        let delta = &args[sent..];
        if self.func_item_custom.get(key).copied().unwrap_or(false) {
            if self.patch_calls.contains_key(key) {
                let result = self.patch_calls.get_mut(key).unwrap().push_arguments(delta);
                match result {
                    Err(err) => self.fail(err, out),
                    Ok(patch_delta) if !patch_delta.is_empty() => {
                        let seq = self.next_seq();
                        let payload = self.patch_calls[key].input_delta(&patch_delta, seq);
                        out.push(event("response.custom_tool_call_input.delta", &payload));
                    }
                    Ok(_) => {}
                }
                self.func_args_sent.insert(key.to_owned(), args.len());
            }
            return;
        }
        let call_id = self.func_call_ids.get(key).cloned().unwrap_or_default();
        let mut payload = br#"{"type":"response.function_call_arguments.delta","sequence_number":0,"item_id":"","output_index":0,"delta":""}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut payload, "sequence_number", seq);
        gj::set_str(&mut payload, "item_id", with_prefix(b"fc_", &call_id));
        gj::set_int(
            &mut payload,
            "output_index",
            self.func_output_ix.get(key).copied().unwrap_or(0),
        );
        gj::set_str_no_html(&mut payload, "delta", delta);
        out.push(event("response.function_call_arguments.delta", &payload));
        self.func_args_sent.insert(key.to_owned(), args.len());
    }

    fn reasoning_event(&mut self, template: &[u8], text_path: &str, text: &[u8]) -> Vec<u8> {
        let mut payload = template.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut payload, "sequence_number", seq);
        gj::set_str(&mut payload, "item_id", &self.reasoning_id);
        gj::set_int(&mut payload, "output_index", self.reasoning_index);
        if !text_path.is_empty() {
            gj::set_str(&mut payload, text_path, text);
        }
        payload
    }

    fn stop_reasoning(&mut self, out: &mut Vec<Vec<u8>>) {
        let text = std::mem::take(&mut self.reasoning_buf);
        let done = self.reasoning_event(
            br#"{"type":"response.reasoning_summary_text.done","sequence_number":0,"item_id":"","output_index":0,"summary_index":0,"text":""}"#,
            "text",
            &text,
        );
        out.push(event("response.reasoning_summary_text.done", &done));
        let part = self.reasoning_event(
            br#"{"type":"response.reasoning_summary_part.done","sequence_number":0,"item_id":"","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}"#,
            "part.text",
            &text,
        );
        out.push(event("response.reasoning_summary_part.done", &part));
        let mut item = br#"{"type":"response.output_item.done","item":{"id":"","type":"reasoning","encrypted_content":"","summary":[{"type":"summary_text","text":""}]},"output_index":0,"sequence_number":0}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut item, "sequence_number", seq);
        gj::set_str(&mut item, "item.id", &self.reasoning_id);
        gj::set_int(&mut item, "output_index", self.reasoning_index);
        gj::set_str(&mut item, "item.summary.0.text", &text);
        out.push(event("response.output_item.done", &item));
        let id = std::mem::take(&mut self.reasoning_id);
        self.reasonings.push((id, text, self.reasoning_index));
    }

    fn message_id(&self, index: i64) -> Vec<u8> {
        format!("msg_{}_{index}", String::from_utf8_lossy(&self.response_id)).into_bytes()
    }

    fn message_done(&mut self, index: i64, out: &mut Vec<Vec<u8>>) {
        if !self.msg_item_added.contains(&index) || self.msg_item_done.contains(&index) {
            return;
        }
        let output_index = self.msg_output_ix.get(&index).copied().unwrap_or(0);
        let text = self.msg_text.get(&index).cloned().unwrap_or_default();
        let id = self.message_id(index);
        for (name, template, path) in [
            (
                "response.output_text.done",
                &br#"{"type":"response.output_text.done","sequence_number":0,"item_id":"","output_index":0,"content_index":0,"text":"","logprobs":[]}"#[..],
                "text",
            ),
            (
                "response.content_part.done",
                br#"{"type":"response.content_part.done","sequence_number":0,"item_id":"","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}"#,
                "part.text",
            ),
        ] {
            let mut payload = template.to_vec();
            let seq = self.next_seq();
            gj::set_int(&mut payload, "sequence_number", seq);
            gj::set_str(&mut payload, "item_id", &id);
            gj::set_int(&mut payload, "output_index", output_index);
            gj::set_int(&mut payload, "content_index", 0);
            gj::set_str(&mut payload, path, &text);
            out.push(event(name, &payload));
        }
        let mut item = br#"{"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":{"id":"","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":""}],"role":"assistant"}}"#.to_vec();
        let seq = self.next_seq();
        gj::set_int(&mut item, "sequence_number", seq);
        gj::set_int(&mut item, "output_index", output_index);
        gj::set_str(&mut item, "item.id", &id);
        gj::set_str(&mut item, "item.status", status(&self.finish_reason));
        gj::set_str(&mut item, "item.content.0.text", &text);
        out.push(event("response.output_item.done", &item));
        self.msg_item_done.insert(index);
    }

    /// finalizeOpenItems: messages, reasoning, then tool calls in output order.
    fn finalize_open_items(&mut self, out: &mut Vec<Vec<u8>>) {
        if self.tool_error.is_some() {
            return;
        }
        let mut messages: Vec<i64> = self.msg_item_added.iter().copied().collect();
        messages.sort_by_key(|i| self.msg_output_ix.get(i).copied().unwrap_or(0));
        for index in messages {
            self.message_done(index, out);
        }
        if !self.reasoning_id.is_empty() {
            self.stop_reasoning(out);
        }
        let mut keys: Vec<String> = self.func_args.keys().cloned().collect();
        keys.sort_by(|a, b| {
            let (l, r) = (self.func_output_ix[a], self.func_output_ix[b]);
            l.cmp(&r).then_with(|| a.cmp(b))
        });
        let incomplete = incomplete_details(&self.finish_reason).is_some();
        let explicit_finish = self.finish_reason == b"tool_calls" || self.finish_reason == b"stop";
        for key in keys {
            if self.func_item_done.contains(&key) {
                continue;
            }
            let buffer = self.func_args.get(&key).cloned().unwrap_or_default();
            let has_args = !buffer.is_empty();
            let mut name = self
                .index()
                .canonical_name(&self.func_names.get(&key).cloned().unwrap_or_default());
            if name.is_empty() {
                name = self.index().single_custom_name().0;
            }
            if !self.index().is_apply_patch(&name)
                && self.finish_reason.is_empty()
                && (!has_args || !gj::valid(&buffer))
            {
                continue;
            }
            self.emit_tool_item(&key, true, out);
            self.emit_pending_args(&key, out);
            if self.tool_error.is_some() {
                return;
            }
            let call_id = self.func_call_ids.get(&key).cloned().unwrap_or_default();
            if call_id.is_empty() || self.func_item_done.contains(&key) {
                continue;
            }
            let output_index = self.func_output_ix[&key];
            let args: Vec<u8> = if has_args {
                buffer
            } else if incomplete || !explicit_finish {
                vec![]
            } else {
                b"{}".to_vec()
            };
            let tool_status = if incomplete { "incomplete" } else { "completed" };
            let names = self.func_names.get(&key).cloned().unwrap_or_default();
            if self.func_item_custom.get(&key).copied().unwrap_or(false) {
                let input;
                if self.patch_calls.contains_key(&key) {
                    let result = self.patch_calls.get_mut(&key).unwrap().finish_arguments(&args);
                    let (tail, full) = match result {
                        Ok(r) => r,
                        Err(err) => {
                            self.fail(err, out);
                            return;
                        }
                    };
                    if !tail.is_empty() {
                        let seq = self.next_seq();
                        let payload = self.patch_calls[&key].input_delta(&tail, seq);
                        out.push(event("response.custom_tool_call_input.delta", &payload));
                    }
                    let seq = self.next_seq();
                    let payload = self.patch_calls[&key].input_done(&full, seq);
                    out.push(event("response.custom_tool_call_input.done", &payload));
                    input = full.into_bytes();
                } else {
                    input = unwrap_custom_tool_input(&args);
                    let mut payload = br#"{"type":"response.custom_tool_call_input.done","sequence_number":0,"item_id":"","output_index":0,"input":""}"#.to_vec();
                    let seq = self.next_seq();
                    gj::set_int(&mut payload, "sequence_number", seq);
                    gj::set_str(&mut payload, "item_id", with_prefix(b"ctc_", &call_id));
                    gj::set_int(&mut payload, "output_index", output_index);
                    gj::set_str(&mut payload, "input", &input);
                    out.push(event("response.custom_tool_call_input.done", &payload));
                }
                let mut item = br#"{"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":{"id":"","type":"custom_tool_call","status":"completed","input":"","call_id":"","name":""}}"#.to_vec();
                let seq = self.next_seq();
                gj::set_int(&mut item, "sequence_number", seq);
                gj::set_int(&mut item, "output_index", output_index);
                gj::set_str(&mut item, "item.id", with_prefix(b"ctc_", &call_id));
                gj::set_str(&mut item, "item.status", tool_status);
                gj::set_str(&mut item, "item.input", &input);
                gj::set_str(&mut item, "item.call_id", &call_id);
                self.index().apply_identity(&mut item, &names, "item");
                out.push(event("response.output_item.done", &item));
                self.func_item_done.insert(key);
                continue;
            }
            let mut done = br#"{"type":"response.function_call_arguments.done","sequence_number":0,"item_id":"","output_index":0,"arguments":""}"#.to_vec();
            let seq = self.next_seq();
            gj::set_int(&mut done, "sequence_number", seq);
            gj::set_str(&mut done, "item_id", with_prefix(b"fc_", &call_id));
            gj::set_int(&mut done, "output_index", output_index);
            gj::set_str_no_html(&mut done, "arguments", &args);
            out.push(event("response.function_call_arguments.done", &done));
            let mut item = br#"{"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":{"id":"","type":"function_call","status":"completed","arguments":"","call_id":"","name":""}}"#.to_vec();
            let seq = self.next_seq();
            gj::set_int(&mut item, "sequence_number", seq);
            gj::set_int(&mut item, "output_index", output_index);
            gj::set_str(&mut item, "item.id", with_prefix(b"fc_", &call_id));
            gj::set_str(&mut item, "item.status", tool_status);
            gj::set_str_no_html(&mut item, "item.arguments", &args);
            gj::set_str(&mut item, "item.call_id", &call_id);
            self.index().apply_identity(&mut item, &names, "item");
            out.push(event("response.output_item.done", &item));
            self.func_item_done.insert(key);
        }
    }

    /// buildResponsesCompletedEvent.
    fn completed_event(&mut self) -> Vec<u8> {
        let incomplete = incomplete_details(&self.finish_reason);
        let event_type = if incomplete.is_some() {
            "response.incomplete"
        } else {
            "response.completed"
        };
        let mut completed = br#"{"type":"","sequence_number":0,"response":{"id":"","object":"response","created_at":0,"status":"","background":false,"error":null}}"#.to_vec();
        gj::set_str(&mut completed, "type", event_type);
        let seq = self.next_seq();
        gj::set_int(&mut completed, "sequence_number", seq);
        gj::set_str(&mut completed, "response.id", &self.response_id);
        gj::set_int(&mut completed, "response.created_at", self.created);
        gj::set_str(&mut completed, "response.status", status(&self.finish_reason));
        if let Some(details) = incomplete {
            gj::set_raw(&mut completed, "response.incomplete_details", details);
        }
        copy_request_fields(&mut completed, &self.request, "response.", Echo::default());

        let item_status = status(&self.finish_reason);
        let mut items: Vec<(i64, Vec<u8>)> = vec![];
        for (id, text, index) in &self.reasonings {
            let mut item = br#"{"id":"","type":"reasoning","summary":[{"type":"summary_text","text":""}]}"#.to_vec();
            gj::set_str(&mut item, "id", id);
            gj::set_str(&mut item, "summary.0.text", text);
            items.push((*index, item));
        }
        for &index in &self.msg_item_added {
            let mut item = br#"{"id":"","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":""}],"role":"assistant"}"#.to_vec();
            gj::set_str(&mut item, "id", self.message_id(index));
            gj::set_str(&mut item, "status", item_status);
            gj::set_str(
                &mut item,
                "content.0.text",
                self.msg_text.get(&index).cloned().unwrap_or_default(),
            );
            items.push((self.msg_output_ix.get(&index).copied().unwrap_or(0), item));
        }
        for (key, args) in &self.func_args {
            if !self.func_item_done.contains(key) {
                continue;
            }
            let call_id = self.func_call_ids.get(key).cloned().unwrap_or_default();
            let name = self.func_names.get(key).cloned().unwrap_or_default();
            let mut item;
            if self.func_item_custom.get(key).copied().unwrap_or(false) {
                item = br#"{"id":"","type":"custom_tool_call","status":"completed","input":"","call_id":"","name":""}"#
                    .to_vec();
                gj::set_str(&mut item, "id", with_prefix(b"ctc_", &call_id));
                gj::set_str(&mut item, "status", item_status);
                let input = match self.patch_calls.get(key) {
                    Some(call) => call.decoder.input().as_bytes().to_vec(),
                    None => unwrap_custom_tool_input(args),
                };
                gj::set_str(&mut item, "input", &input);
            } else {
                item =
                    br#"{"id":"","type":"function_call","status":"completed","arguments":"","call_id":"","name":""}"#
                        .to_vec();
                gj::set_str(&mut item, "id", with_prefix(b"fc_", &call_id));
                gj::set_str(&mut item, "status", item_status);
                gj::set_str_no_html(&mut item, "arguments", args);
            }
            gj::set_str(&mut item, "call_id", &call_id);
            self.index().apply_identity(&mut item, &name, "");
            items.push((self.func_output_ix[key], item));
        }
        items.sort_by_key(|(index, _)| *index);
        if !items.is_empty() {
            let raws: Vec<Vec<u8>> = items.into_iter().map(|(_, raw)| raw).collect();
            gj::set_raw(&mut completed, "response.output", gj::join(&raws));
        }
        if self.usage_seen {
            gj::set_int(&mut completed, "response.usage.input_tokens", self.prompt_tokens);
            gj::set_int(
                &mut completed,
                "response.usage.input_tokens_details.cached_tokens",
                self.cached_tokens,
            );
            gj::set_int(&mut completed, "response.usage.output_tokens", self.completion_tokens);
            if self.reasoning_tokens > 0 {
                gj::set_int(
                    &mut completed,
                    "response.usage.output_tokens_details.reasoning_tokens",
                    self.reasoning_tokens,
                );
            }
            let total = if self.total_tokens == 0 {
                self.prompt_tokens.wrapping_add(self.completion_tokens)
            } else {
                self.total_tokens
            };
            gj::set_int(&mut completed, "response.usage.total_tokens", total);
        }
        event(event_type, &completed)
    }

    fn usage(&mut self, usage: &Res<'_>) {
        let mut take = |paths: &[&str], slot: &mut i64| {
            if let Some(v) = paths.iter().map(|p| usage.get(*p)).find(Res::exists) {
                *slot = v.int();
                self.usage_seen = true;
            }
        };
        let mut values = (
            self.prompt_tokens,
            self.cached_tokens,
            self.completion_tokens,
            self.reasoning_tokens,
            self.total_tokens,
        );
        take(&["prompt_tokens"], &mut values.0);
        take(&["prompt_tokens_details.cached_tokens"], &mut values.1);
        take(&["completion_tokens", "output_tokens"], &mut values.2);
        take(
            &[
                "output_tokens_details.reasoning_tokens",
                "completion_tokens_details.reasoning_tokens",
            ],
            &mut values.3,
        );
        take(&["total_tokens"], &mut values.4);
        (
            self.prompt_tokens,
            self.cached_tokens,
            self.completion_tokens,
            self.reasoning_tokens,
            self.total_tokens,
        ) = values;
    }

    fn start(&mut self, root: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        self.response_id = root.get("id").bytes().into_owned();
        self.created = root.get("created").int();
        let mut model = request_model_name(&self.original, &self.translated);
        if model.is_empty() {
            model = self.model.as_bytes().to_vec();
        }
        for (name, template) in [
            (
                "response.created",
                &br#"{"type":"response.created","sequence_number":0,"response":{"id":"","object":"response","created_at":0,"status":"in_progress","background":false,"error":null,"output":[]}}"#[..],
            ),
            (
                "response.in_progress",
                br#"{"type":"response.in_progress","sequence_number":0,"response":{"id":"","object":"response","created_at":0,"status":"in_progress","output":[]}}"#,
            ),
        ] {
            let mut payload = template.to_vec();
            let seq = self.next_seq();
            gj::set_int(&mut payload, "sequence_number", seq);
            gj::set_str(&mut payload, "response.id", &self.response_id);
            gj::set_int(&mut payload, "response.created_at", self.created);
            if !model.is_empty() {
                gj::set_str(&mut payload, "response.model", &model);
            }
            out.push(event(name, &payload));
        }
        self.started = true;
    }

    /// One choice of a chunk; false stops the chunk.
    fn choice(&mut self, choice: &Res<'_>, out: &mut Vec<Vec<u8>>) -> bool {
        let index = choice.get("index").int();
        let delta = choice.get("delta");
        if delta.exists() {
            let mut rc = delta.get("reasoning_content");
            if !rc.exists() || rc.bytes().is_empty() {
                rc = delta.get("reasoning");
            }
            let reasoning = rc.bytes().into_owned();
            if rc.exists() && !reasoning.is_empty() {
                if self.reasoning_id.is_empty() {
                    self.reasoning_id =
                        format!("rs_{}_{index}", String::from_utf8_lossy(&self.response_id)).into_bytes();
                    self.reasoning_index = self.alloc_output_index();
                    let mut item = br#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"reasoning","status":"in_progress","summary":[]}}"#.to_vec();
                    let seq = self.next_seq();
                    gj::set_int(&mut item, "sequence_number", seq);
                    gj::set_int(&mut item, "output_index", self.reasoning_index);
                    gj::set_str(&mut item, "item.id", &self.reasoning_id);
                    out.push(event("response.output_item.added", &item));
                    let part = self.reasoning_event(
                        br#"{"type":"response.reasoning_summary_part.added","sequence_number":0,"item_id":"","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}"#,
                        "",
                        b"",
                    );
                    out.push(event("response.reasoning_summary_part.added", &part));
                }
                self.reasoning_buf.extend_from_slice(&reasoning);
                let msg = self.reasoning_event(
                    br#"{"type":"response.reasoning_summary_text.delta","sequence_number":0,"item_id":"","output_index":0,"summary_index":0,"delta":""}"#,
                    "delta",
                    &reasoning,
                );
                out.push(event("response.reasoning_summary_text.delta", &msg));
            }

            let content = delta.get("content");
            let text = content.bytes().into_owned();
            if content.exists() && !text.is_empty() {
                if !self.reasoning_id.is_empty() {
                    self.stop_reasoning(out);
                }
                if !self.msg_output_ix.contains_key(&index) {
                    let output_index = self.alloc_output_index();
                    self.msg_output_ix.insert(index, output_index);
                }
                let output_index = self.msg_output_ix[&index];
                let id = self.message_id(index);
                if self.msg_item_added.insert(index) {
                    let mut item = br#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"message","status":"in_progress","content":[],"role":"assistant"}}"#.to_vec();
                    let seq = self.next_seq();
                    gj::set_int(&mut item, "sequence_number", seq);
                    gj::set_int(&mut item, "output_index", output_index);
                    gj::set_str(&mut item, "item.id", &id);
                    out.push(event("response.output_item.added", &item));
                }
                if self.msg_content_added.insert(index) {
                    let mut part = br#"{"type":"response.content_part.added","sequence_number":0,"item_id":"","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}"#.to_vec();
                    let seq = self.next_seq();
                    gj::set_int(&mut part, "sequence_number", seq);
                    gj::set_str(&mut part, "item_id", &id);
                    gj::set_int(&mut part, "output_index", output_index);
                    gj::set_int(&mut part, "content_index", 0);
                    out.push(event("response.content_part.added", &part));
                }
                let mut msg = br#"{"type":"response.output_text.delta","sequence_number":0,"item_id":"","output_index":0,"content_index":0,"delta":"","logprobs":[]}"#.to_vec();
                let seq = self.next_seq();
                gj::set_int(&mut msg, "sequence_number", seq);
                gj::set_str(&mut msg, "item_id", &id);
                gj::set_int(&mut msg, "output_index", output_index);
                gj::set_int(&mut msg, "content_index", 0);
                gj::set_str(&mut msg, "delta", &text);
                out.push(event("response.output_text.delta", &msg));
                self.msg_text.entry(index).or_default().extend_from_slice(&text);
            }

            let calls = delta.get("tool_calls");
            if calls.is_array() && !calls.array().is_empty() {
                if !self.reasoning_id.is_empty() {
                    self.stop_reasoning(out);
                }
                self.message_done(index, out);
                for call in calls.array() {
                    if !self.tool_call(index, &call, out) {
                        break;
                    }
                }
            }
        }
        if self.tool_error.is_some() {
            return false;
        }
        let finish = choice.get("finish_reason").bytes().into_owned();
        if !finish.is_empty() {
            self.finish_reason = finish;
            self.finalize_open_items(out);
        }
        self.tool_error.is_none()
    }

    fn tool_call(&mut self, choice_index: i64, call: &Res<'_>, out: &mut Vec<Vec<u8>>) -> bool {
        let key = format!("{choice_index}:{}", call.get("index").int());
        if !self.func_args.contains_key(&key) {
            self.func_args.insert(key.clone(), vec![]);
            let output_index = self.alloc_output_index();
            self.func_output_ix.insert(key.clone(), output_index);
        }
        let new_id = call.get("id").bytes().into_owned();
        let name_chunk = call.get("function.name").bytes().into_owned();
        let new_name = self.index().canonical_name(&name_chunk);
        let old_id = self.func_call_ids.get(&key).cloned().unwrap_or_default();
        let old_name = self
            .index()
            .canonical_name(&self.func_names.get(&key).cloned().unwrap_or_default());
        if !new_id.is_empty() && !old_id.is_empty() && new_id != old_id {
            self.func_conflicts.insert(key.clone());
        }
        if (self.index().is_apply_patch(&old_name) || self.index().is_apply_patch(&new_name))
            && (self.func_conflicts.contains(&key)
                || (!new_name.is_empty() && !old_name.is_empty() && new_name != old_name))
        {
            self.fail("conflicting apply_patch call identity".into(), out);
            return false;
        }
        if !new_id.is_empty() && old_id.is_empty() {
            self.func_call_ids.insert(key.clone(), new_id);
        }
        if !name_chunk.is_empty() && !self.func_item_added.contains(&key) {
            self.func_names.insert(key.clone(), name_chunk);
        }
        let args = call.get("function.arguments");
        let args_text = args.bytes();
        if args.exists() && !args_text.is_empty() {
            self.func_args.get_mut(&key).unwrap().extend_from_slice(&args_text);
        }
        self.emit_tool_item(&key, false, out);
        self.emit_pending_args(&key, out);
        self.tool_error.is_none()
    }
}

impl GoStream for State {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        if self.tool_error.is_some() || self.completed_emitted {
            return Ok(vec![]);
        }
        let mut raw = line;
        if let Some(rest) = raw.strip_prefix(b"data:") {
            raw = trim_space(rest);
        }
        let raw = trim_space(raw);
        if raw.is_empty() {
            return Ok(vec![]);
        }
        let done = raw == b"[DONE]";
        if done && (!self.started || self.completed_emitted) {
            return Ok(vec![]);
        }
        let root = gj::parse(raw);
        if !done {
            let object = root.get("object").bytes();
            if !object.is_empty() && object.as_ref() != b"chat.completion.chunk" {
                return Ok(vec![]);
            }
            if !root.get("choices").is_array() {
                return Ok(vec![]);
            }
        }
        let usage = root.get("usage");
        if usage.exists() {
            self.usage(&usage);
        }
        let mut out = vec![];
        if !self.started {
            self.start(&root, &mut out);
        }
        if done {
            self.finalize_open_items(&mut out);
            if self.tool_error.is_some() {
                return Ok(out);
            }
            if self.func_item_added.iter().any(|k| !self.func_item_done.contains(k)) {
                return Ok(out);
            }
            if self.msg_item_added.is_empty() && self.func_item_added.is_empty() {
                return Ok(out);
            }
            self.completed_emitted = true;
            let completed = self.completed_event();
            out.push(completed);
            return Ok(out);
        }
        for choice in root.get("choices").array() {
            if !self.choice(&choice, &mut out) {
                break;
            }
        }
        Ok(out)
    }

    fn tool_input_failed(&self) -> bool {
        self.tool_error.is_some()
    }

    /// FinalizeToolInput: a patch-enabled stream that ends without its terminator fails.
    fn finalize_tool_input(&mut self) -> Vec<Vec<u8>> {
        if self.tool_error.is_some() || self.completed_emitted || !self.index().has_apply_patch() {
            return vec![];
        }
        self.tool_error = Some("upstream apply_patch stream ended before protocol completion".into());
        self.seq += 1;
        vec![event(
            "response.failed",
            &apply_patch::failure(&self.response_id, self.seq),
        )]
    }
}

/// ConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream.
pub fn non_stream(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let root = gj::parse(body);
    let index = ToolIndex::from_raw(pick_request(ctx.original_request, ctx.translated_request));
    let request = ctx.translated_request;
    let finish = root.get("choices.0.finish_reason").bytes().into_owned();
    let incomplete = incomplete_details(&finish);

    let mut resp = br#"{"id":"","object":"response","created_at":0,"status":"completed","background":false,"error":null,"incomplete_details":null}"#.to_vec();
    gj::set_str(&mut resp, "status", status(&finish));
    if let Some(details) = incomplete {
        gj::set_raw(&mut resp, "incomplete_details", details);
    }
    let mut id = root.get("id").bytes().into_owned();
    if id.is_empty() {
        id = format!(
            "resp_{:x}_{}",
            now_nanos(),
            RESPONSE_IDS.fetch_add(1, Ordering::Relaxed) + 1
        )
        .into_bytes();
    }
    gj::set_str(&mut resp, "id", &id);
    let mut created = root.get("created").int();
    if created == 0 {
        created = now_unix();
    }
    gj::set_int(&mut resp, "created_at", created);
    let model = root.get("model");
    let model = model.exists().then(|| model.bytes().into_owned());
    if !request.is_empty() {
        copy_request_fields(
            &mut resp,
            request,
            "",
            Echo {
                model: model.as_deref(),
                max_tokens: true,
            },
        );
    } else if let Some(model) = &model {
        gj::set_str(&mut resp, "model", model);
    }

    let mut outputs: Vec<Vec<u8>> = vec![];
    let mut rc = gj::get(body, "choices.0.message.reasoning_content");
    if !rc.exists() || rc.bytes().is_empty() {
        rc = gj::get(body, "choices.0.message.reasoning");
    }
    let rc_text = rc.bytes().into_owned();
    let include_reasoning = !rc_text.is_empty() || (!request.is_empty() && gj::get(request, "reasoning").exists());
    if include_reasoning {
        let rid = id.strip_prefix(b"resp_").unwrap_or(&id);
        let mut item = br#"{"id":"","type":"reasoning","encrypted_content":"","summary":[]}"#.to_vec();
        gj::set_str(&mut item, "id", with_prefix(b"rs_", rid));
        if !rc_text.is_empty() {
            gj::set_str(&mut item, "summary.0.type", "summary_text");
            gj::set_str(&mut item, "summary.0.text", &rc_text);
        }
        outputs.push(item);
    }

    let item_status = status(&finish);
    let id_text = String::from_utf8_lossy(&id).into_owned();
    let mut failed = false;
    for choice in root.get("choices").array() {
        let message = choice.get("message");
        if !message.exists() {
            continue;
        }
        let choice_index = choice.get("index").int();
        let content = message.get("content");
        if content.exists() && !content.bytes().is_empty() {
            let mut item = br#"{"id":"","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":""}],"role":"assistant"}"#.to_vec();
            gj::set_str(&mut item, "id", format!("msg_{id_text}_{choice_index}"));
            gj::set_str(&mut item, "status", item_status);
            gj::set_str(&mut item, "content.0.text", content.bytes());
            outputs.push(item);
        }
        let calls = message.get("tool_calls");
        if !calls.is_array() {
            continue;
        }
        for (tool_index, call) in calls.array().iter().enumerate() {
            let mut call_id = call.get("id").bytes().into_owned();
            if call_id.is_empty() {
                call_id = format!("call_{id_text}_{choice_index}_{tool_index}").into_bytes();
            }
            let name = index.canonical_name(&call.get("function.name").bytes());
            let args = call.get("function.arguments").bytes().into_owned();
            let mut item;
            if index.custom.contains(&name) {
                item = br#"{"id":"","type":"custom_tool_call","status":"completed","input":"","call_id":"","name":""}"#
                    .to_vec();
                gj::set_str(&mut item, "id", with_prefix(b"ctc_", &call_id));
                gj::set_str(&mut item, "status", item_status);
                let input = if index.is_apply_patch(&name) {
                    match CallState::default().finish_arguments(&args) {
                        Ok((_, full)) => full.into_bytes(),
                        Err(_) => {
                            failed = true;
                            break;
                        }
                    }
                } else {
                    unwrap_custom_tool_input(&args)
                };
                gj::set_str(&mut item, "input", &input);
            } else {
                item =
                    br#"{"id":"","type":"function_call","status":"completed","arguments":"","call_id":"","name":""}"#
                        .to_vec();
                gj::set_str(&mut item, "id", with_prefix(b"fc_", &call_id));
                gj::set_str(&mut item, "status", item_status);
                gj::set_str_no_html(&mut item, "arguments", &args);
            }
            gj::set_str(&mut item, "call_id", &call_id);
            index.apply_identity(&mut item, &name, "");
            outputs.push(item);
        }
        if failed {
            break;
        }
    }
    if failed {
        return Err(Error(apply_patch::UPSTREAM_ERROR_MESSAGE.into()));
    }
    if !outputs.is_empty() {
        gj::set_raw(&mut resp, "output", gj::join(&outputs));
    }
    let usage = root.get("usage");
    if usage.exists() {
        if ["prompt_tokens", "completion_tokens", "total_tokens"]
            .iter()
            .any(|k| usage.get(*k).exists())
        {
            gj::set_int(&mut resp, "usage.input_tokens", usage.get("prompt_tokens").int());
            let cached = usage.get("prompt_tokens_details.cached_tokens");
            if cached.exists() {
                gj::set_int(&mut resp, "usage.input_tokens_details.cached_tokens", cached.int());
            }
            gj::set_int(&mut resp, "usage.output_tokens", usage.get("completion_tokens").int());
            let reasoning = usage.get("output_tokens_details.reasoning_tokens");
            if reasoning.exists() {
                gj::set_int(
                    &mut resp,
                    "usage.output_tokens_details.reasoning_tokens",
                    reasoning.int(),
                );
            }
            gj::set_int(&mut resp, "usage.total_tokens", usage.get("total_tokens").int());
        } else {
            AnyValue::from_res(&usage).set(&mut resp, "usage");
        }
    }
    Ok(resp)
}
