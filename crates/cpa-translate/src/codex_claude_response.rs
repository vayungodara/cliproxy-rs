//! Codex (OpenAI Responses) events -> Claude Messages responses
//! (internal/translator/codex/claude/codex_claude_response.go,
//! codex_claude_response_web_search.go).
//!
//! Function calls stream one at a time: while a call is open, unrelated events are
//! deferred and replayed once it closes, so Claude content blocks never interleave.

use std::collections::{HashMap, HashSet};

use cpa_common::json::{self as gj, Kind, Res};

use crate::codex_claude::{short_to_original, shorten_call_id};
use crate::common::{sanitize_claude_tool_id, sse_event, trim_space};
use crate::stream::GoStream;
use crate::{Error, ResponseCtx};

/// Joins the summary parts of one Codex reasoning item inside one thinking block.
const SUMMARY_PART_SEPARATOR: &[u8] = b"\n\n";

#[derive(Default)]
struct Call {
    call_id: Vec<u8>,
    name: Vec<u8>,
    block_index: i64,
    arguments: Vec<u8>,
    emitted: usize,
    has_delta: bool,
    emit_initial_empty_delta: bool,
    started: bool,
    done: bool,
    closed: bool,
}

#[derive(Default)]
struct State {
    original: Vec<u8>,
    has_emitted_tool_use: bool,
    block_index: i64,
    has_text_delta: bool,
    text_open: bool,
    thinking_open: bool,
    thinking_signature: Vec<u8>,
    thinking_summary_seen: bool,
    web_search_uses: HashSet<Vec<u8>>,
    web_search_results: HashSet<Vec<u8>>,
    last_web_search_id: Vec<u8>,
    /// Calls by alias key (output index, call ID, item ID); values index `calls`.
    by_key: HashMap<Vec<u8>, usize>,
    calls: Vec<Call>,
    queue: Vec<usize>,
    active: Option<usize>,
    last: Option<usize>,
    deferred: Vec<Vec<u8>>,
}

pub fn go_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    Box::new(State {
        original: ctx.original_request.to_vec(),
        ..Default::default()
    })
}

impl GoStream for State {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        Ok(self.translate(line))
    }
}

fn event(out: &mut Vec<u8>, name: &str, payload: &[u8]) {
    out.extend_from_slice(&sse_event(name, payload));
}

fn indexed(template: &[u8], index: i64) -> Vec<u8> {
    let mut payload = template.to_vec();
    gj::set_int(&mut payload, "index", index);
    payload
}

/// shouldDeferCodexStreamEvent.
fn should_defer(kind: &[u8], root: &Res<'_>) -> bool {
    match kind {
        b"error"
        | b"response.completed"
        | b"response.incomplete"
        | b"response.function_call_arguments.delta"
        | b"response.function_call_arguments.done" => false,
        b"response.output_item.added" | b"response.output_item.done" => {
            root.get("item.type").bytes().as_ref() != b"function_call"
        }
        _ => true,
    }
}

/// codexFunctionCallKeys.
fn call_keys(root: &Res<'_>, item: &Res<'_>) -> Vec<Vec<u8>> {
    let mut keys: Vec<Vec<u8>> = vec![];
    let mut add = |prefix: &[u8], value: &[u8]| {
        let key = [prefix, value].concat();
        if !keys.contains(&key) {
            keys.push(key);
        }
    };
    let output_index = root.get("output_index");
    if output_index.exists() {
        add(b"output:", &output_index.raw);
    }
    for (prefix, value) in [
        (&b"call:"[..], item.get("call_id").bytes()),
        (b"call:", root.get("call_id").bytes()),
        (b"item:", item.get("id").bytes()),
        (b"item:", root.get("item_id").bytes()),
    ] {
        if !value.is_empty() {
            add(prefix, &value);
        }
    }
    keys
}

/// codexStreamErrorToClaudeError.
fn stream_error(root: &Res<'_>) -> Vec<u8> {
    let error = root.get("error");
    let mut kind = trim_space(&error.get("type").bytes()).to_vec();
    if kind.is_empty() {
        kind = trim_space(&root.get("error_type").bytes()).to_vec();
    }
    if kind.is_empty() {
        kind = b"api_error".to_vec();
    }
    let code = trim_space(&error.get("code").bytes()).to_vec();
    let mut message = trim_space(&error.get("message").bytes()).to_vec();
    if message.is_empty() {
        message = trim_space(&root.get("message").bytes()).to_vec();
    }
    if message.is_empty() {
        message = code.clone();
    }
    if message.is_empty() {
        message = kind.clone();
    }
    if code == b"cyber_policy" || kind == b"invalid_request" {
        kind = b"invalid_request_error".to_vec();
    }
    let mut payload = br#"{"type":"error","error":{"type":"api_error","message":""}}"#.to_vec();
    gj::set_str(&mut payload, "error.type", &kind);
    gj::set_str(&mut payload, "error.message", &message);
    sse_event("error", &payload)
}

impl State {
    fn start_text(&mut self, out: &mut Vec<u8>) {
        if self.text_open {
            return;
        }
        let payload = indexed(
            br#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            self.block_index,
        );
        self.text_open = true;
        event(out, "content_block_start", &payload);
    }

    fn stop_text(&mut self, out: &mut Vec<u8>) {
        if !self.text_open {
            return;
        }
        let payload = indexed(br#"{"type":"content_block_stop","index":0}"#, self.block_index);
        self.text_open = false;
        self.block_index += 1;
        event(out, "content_block_stop", &payload);
    }

    fn start_thinking(&mut self, out: &mut Vec<u8>) {
        if self.thinking_open {
            return;
        }
        let payload = indexed(
            br#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            self.block_index,
        );
        self.thinking_open = true;
        event(out, "content_block_start", &payload);
    }

    fn thinking_delta(&mut self, text: &[u8], out: &mut Vec<u8>) {
        if text.is_empty() {
            return;
        }
        let mut payload = indexed(
            br#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}"#,
            self.block_index,
        );
        gj::set_str(&mut payload, "delta.thinking", text);
        event(out, "content_block_delta", &payload);
    }

    fn finalize_thinking(&mut self, out: &mut Vec<u8>) {
        if !self.thinking_open {
            return;
        }
        if !self.thinking_signature.is_empty() {
            let mut payload = indexed(
                br#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":""}}"#,
                self.block_index,
            );
            gj::set_str(&mut payload, "delta.signature", &self.thinking_signature);
            event(out, "content_block_delta", &payload);
        }
        let payload = indexed(br#"{"type":"content_block_stop","index":0}"#, self.block_index);
        event(out, "content_block_stop", &payload);
        self.block_index += 1;
        self.thinking_open = false;
    }

    fn finalize_signature_only_thinking(&mut self, out: &mut Vec<u8>) {
        if self.thinking_signature.is_empty() {
            return;
        }
        self.start_thinking(out);
        self.finalize_thinking(out);
    }

    fn text_delta(&mut self, text: &[u8], out: &mut Vec<u8>) {
        let mut payload = indexed(
            br#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":""}}"#,
            self.block_index,
        );
        gj::set_str(&mut payload, "delta.text", text);
        event(out, "content_block_delta", &payload);
    }

    fn call_for_keys(&self, keys: &[Vec<u8>]) -> Option<usize> {
        keys.iter().find_map(|k| self.by_key.get(k).copied())
    }

    /// codexFunctionCallForEvent: by alias, or the last call when the event has no keys.
    fn call_for_event(&self, root: &Res<'_>, item: &Res<'_>) -> Option<usize> {
        let keys = call_keys(root, item);
        if keys.is_empty() {
            self.last
        } else {
            self.call_for_keys(&keys)
        }
    }

    fn add_aliases(&mut self, call: usize, keys: Vec<Vec<u8>>) {
        for key in keys {
            self.by_key.insert(key, call);
        }
    }

    fn new_call(&mut self) -> usize {
        self.calls.push(Call {
            block_index: -1,
            ..Default::default()
        });
        let call = self.calls.len() - 1;
        self.queue.push(call);
        call
    }

    /// recordCodexFunctionCall.
    fn record_call(&mut self, root: &Res<'_>, item: &Res<'_>) -> usize {
        let keys = call_keys(root, item);
        let call = match self.call_for_keys(&keys) {
            Some(call) => call,
            None => self.new_call(),
        };
        self.add_aliases(call, keys);
        self.last = Some(call);
        call
    }

    /// updateCodexFunctionCallIdentity.
    fn update_identity(&mut self, call: usize, root: &Res<'_>, item: &Res<'_>) {
        let call_id = item.get("call_id").bytes();
        if !call_id.is_empty() {
            self.calls[call].call_id = call_id.into_owned();
        }
        let name = item.get("name").bytes();
        if !name.is_empty() {
            self.calls[call].name = name.into_owned();
        }
        self.add_aliases(call, call_keys(root, item));
    }

    /// updateCodexFunctionCallArguments: deltas accumulate; a full value replaces the
    /// buffer unless deltas arrived and it does not extend them.
    fn update_arguments(&mut self, call: usize, arguments: &[u8], delta: bool) {
        let call = &mut self.calls[call];
        if arguments.is_empty() {
            return;
        }
        if delta {
            call.arguments.extend_from_slice(arguments);
            call.has_delta = true;
        } else if !call.has_delta || arguments.starts_with(&call.arguments) {
            call.arguments = arguments.to_vec();
        }
    }

    fn argument_delta(out: &mut Vec<u8>, partial: &[u8], index: i64) {
        let mut payload = indexed(
            br#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":""}}"#,
            index,
        );
        gj::set_str(&mut payload, "delta.partial_json", partial);
        event(out, "content_block_delta", &payload);
    }

    /// appendCodexFunctionCallBufferedArguments.
    fn buffered_arguments(&mut self, call: usize, out: &mut Vec<u8>) {
        let c = &self.calls[call];
        if self.active != Some(call) || !c.started || c.closed || c.emitted >= c.arguments.len() {
            return;
        }
        Self::argument_delta(out, &c.arguments[c.emitted..], c.block_index);
        self.calls[call].emitted = self.calls[call].arguments.len();
    }

    /// appendCodexFunctionCallQueue: closes the active call once done and starts the next
    /// named one.
    fn run_queue(&mut self, out: &mut Vec<u8>) {
        loop {
            if let Some(active) = self.active {
                self.buffered_arguments(active, out);
                if !self.calls[active].done {
                    return;
                }
                let index = self.calls[active].block_index;
                event(
                    out,
                    "content_block_stop",
                    &indexed(br#"{"type":"content_block_stop","index":0}"#, index),
                );
                if self.block_index <= index {
                    self.block_index = index + 1;
                }
                self.calls[active].closed = true;
                self.active = None;
                if let Some(position) = self.queue.iter().position(|&c| c == active) {
                    self.queue.remove(position);
                }
            }
            while self.queue.first().is_some_and(|&c| self.calls[c].closed) {
                self.queue.remove(0);
            }
            let Some(&call) = self.queue.first() else {
                return;
            };
            if self.calls[call].name.is_empty() {
                return;
            }
            self.calls[call].block_index = self.block_index;
            let c = &self.calls[call];
            let mut start = indexed(
                br#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"","name":"","input":{}}}"#,
                c.block_index,
            );
            gj::set_str(
                &mut start,
                "content_block.id",
                shorten_call_id(&sanitize_claude_tool_id(&c.call_id)),
            );
            let names = short_to_original(&self.original);
            let name = names.get(&c.name).cloned().unwrap_or_else(|| c.name.clone());
            gj::set_str(&mut start, "content_block.name", &name);
            event(out, "content_block_start", &start);
            if c.emit_initial_empty_delta {
                Self::argument_delta(out, b"", c.block_index);
            }
            self.calls[call].started = true;
            self.active = Some(call);
            self.has_emitted_tool_use = true;
            self.buffered_arguments(call, out);
        }
    }

    /// appendCodexFunctionCallsFromTerminal: calls listed in the terminal response close
    /// (and unnamed ones are dropped) before message_delta.
    fn calls_from_terminal(&mut self, response: &Res<'_>, out: &mut Vec<u8>) {
        response.get("output").each(|index, item| {
            if item.get("type").bytes().as_ref() != b"function_call" {
                return true;
            }
            let mut keys = call_keys(&Res::default(), &item);
            let mut add = |key: Vec<u8>| {
                if !keys.contains(&key) {
                    keys.push(key);
                }
            };
            let output_index = item.get("output_index");
            if output_index.exists() {
                add([&b"output:"[..], &output_index.raw].concat());
            }
            if index.exists() {
                add([&b"output:"[..], &index.bytes()].concat());
            }
            let call = match self.call_for_keys(&keys) {
                Some(call) => call,
                None => self.new_call(),
            };
            self.add_aliases(call, keys);
            self.update_identity(call, &Res::default(), &item);
            self.update_arguments(call, &item.get("arguments").bytes(), false);
            self.calls[call].done = true;
            true
        });
        let mut queued = vec![];
        for &call in &self.queue {
            let c = &mut self.calls[call];
            if c.closed {
                continue;
            }
            if c.name.is_empty() {
                c.closed = true;
                continue;
            }
            c.done = true;
            queued.push(call);
        }
        self.queue = queued;
        self.run_queue(out);
        self.by_key.clear();
        self.queue.clear();
        self.active = None;
        self.last = None;
    }

    fn append_deferred(&mut self, out: &mut Vec<u8>) {
        for event in std::mem::take(&mut self.deferred) {
            for chunk in self.translate(&event) {
                out.extend_from_slice(&chunk);
            }
        }
    }

    /// codexWebSearchToolUseID.
    fn web_search_id(&mut self, root: &Res<'_>, item: &Res<'_>) -> Vec<u8> {
        for path in ["id", "output_item_id", "call_id"] {
            for source in [item, root] {
                let value = trim_space(&source.get(path).bytes()).to_vec();
                if !value.is_empty() {
                    return value;
                }
            }
        }
        if !self.last_web_search_id.is_empty() {
            return self.last_web_search_id.clone();
        }
        for source in [item, root] {
            let value = trim_space(&source.get("item_id").bytes()).to_vec();
            if !value.is_empty() {
                return value;
            }
        }
        let id = format!("web_search_{}", self.block_index).into_bytes();
        self.last_web_search_id = id.clone();
        id
    }

    /// appendCodexWebSearchServerToolUse.
    fn web_search_use(&mut self, root: &Res<'_>, item: &Res<'_>, out: &mut Vec<u8>) {
        let id = self.web_search_id(root, item);
        if id.is_empty() {
            return;
        }
        let query = web_search_query(root, item);
        let started = self.web_search_uses.contains(&id);
        if started && query.is_empty() {
            return;
        }
        if !started {
            self.stop_text(out);
            self.finalize_thinking(out);
            let mut start = indexed(
                br#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"","name":"web_search","input":{}}}"#,
                self.block_index,
            );
            gj::set_str(&mut start, "content_block.id", &id);
            event(out, "content_block_start", &start);
        }
        if !query.is_empty() {
            Self::argument_delta(out, &query_json(&query), self.block_index);
        }
        if !started {
            event(
                out,
                "content_block_stop",
                &indexed(br#"{"type":"content_block_stop","index":0}"#, self.block_index),
            );
            self.web_search_uses.insert(id);
            self.block_index += 1;
        }
    }

    /// appendCodexWebSearchToolResult.
    fn web_search_result(&mut self, root: &Res<'_>, item: &Res<'_>, out: &mut Vec<u8>) {
        let id = self.web_search_id(root, item);
        if id.is_empty() {
            return;
        }
        self.web_search_use(root, item, out);
        if self.web_search_results.contains(&id) {
            return;
        }
        let content = web_search_results(root, item);
        if web_search_query(root, item).is_empty() && content.is_none() && !item.get("action").exists() {
            return;
        }
        let mut start = indexed(
            br#"{"type":"content_block_start","index":0,"content_block":{"type":"web_search_tool_result","tool_use_id":"","content":[]}}"#,
            self.block_index,
        );
        gj::set_str(&mut start, "content_block.tool_use_id", &id);
        if let Some(content) = content {
            gj::set_raw(&mut start, "content_block.content", content);
        }
        event(out, "content_block_start", &start);
        event(
            out,
            "content_block_stop",
            &indexed(br#"{"type":"content_block_stop","index":0}"#, self.block_index),
        );
        self.block_index += 1;
        if id == self.last_web_search_id {
            self.last_web_search_id.clear();
        }
        self.web_search_results.insert(id);
    }

    fn translate(&mut self, line: &[u8]) -> Vec<Vec<u8>> {
        let Some(rest) = line.strip_prefix(b"data:") else {
            return vec![];
        };
        let raw_event = line.to_vec();
        let root = gj::parse(trim_space(rest));
        let kind = root.get("type").bytes().into_owned();
        if self.active.is_some() && should_defer(&kind, &root) {
            self.deferred.push(raw_event);
            return vec![];
        }
        let mut out = vec![];
        match kind.as_slice() {
            b"error" => out.extend_from_slice(&stream_error(&root)),
            b"response.created" => {
                let mut payload = br#"{"type":"message_start","message":{"id":"","type":"message","role":"assistant","model":"claude-opus-4-1-20250805","stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0},"content":[],"stop_reason":null}}"#.to_vec();
                gj::set_str(&mut payload, "message.model", root.get("response.model").bytes());
                gj::set_str(&mut payload, "message.id", root.get("response.id").bytes());
                event(&mut out, "message_start", &payload);
            }
            b"response.reasoning_summary_part.added" => {
                self.stop_text(&mut out);
                // Codex splits one reasoning item into summary parts; only its
                // output_item.done carries the final signature, so one block stays open.
                if self.thinking_open {
                    self.thinking_delta(SUMMARY_PART_SEPARATOR, &mut out);
                } else {
                    self.start_thinking(&mut out);
                }
                self.thinking_summary_seen = true;
            }
            b"response.reasoning_summary_text.delta" => {
                self.stop_text(&mut out);
                self.start_thinking(&mut out);
                self.thinking_delta(&root.get("delta").bytes(), &mut out);
            }
            b"response.content_part.added" => {
                self.finalize_thinking(&mut out);
                if root.get("part.type").bytes().as_ref() == b"output_text" {
                    self.start_text(&mut out);
                }
            }
            b"response.output_text.delta" => {
                self.has_text_delta = true;
                self.finalize_thinking(&mut out);
                self.start_text(&mut out);
                self.text_delta(&root.get("delta").bytes(), &mut out);
            }
            b"response.content_part.done" => {
                if root.get("part.type").bytes().as_ref() == b"output_text" {
                    self.stop_text(&mut out);
                }
            }
            b"response.completed" | b"response.incomplete" => {
                let response = root.get("response");
                self.finalize_thinking(&mut out);
                self.stop_text(&mut out);
                self.calls_from_terminal(&response, &mut out);
                self.append_deferred(&mut out);
                self.finalize_thinking(&mut out);
                self.stop_text(&mut out);
                let mut payload = br#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"input_tokens":0,"output_tokens":0}}"#.to_vec();
                gj::set_str(
                    &mut payload,
                    "delta.stop_reason",
                    map_stop_reason(&stop_reason(&response), self.has_emitted_tool_use),
                );
                set_stop_sequence(&mut payload, "delta.stop_sequence", &response);
                set_usage(&mut payload, &response.get("usage"));
                event(&mut out, "message_delta", &payload);
                event(&mut out, "message_stop", br#"{"type":"message_stop"}"#);
            }
            b"response.output_item.added" => {
                let item = root.get("item");
                match item.get("type").bytes().as_ref() {
                    b"function_call" => {
                        self.finalize_thinking(&mut out);
                        self.stop_text(&mut out);
                        let call = self.record_call(&root, &item);
                        self.update_identity(call, &root, &item);
                        if !self.calls[call].name.is_empty() {
                            self.calls[call].emit_initial_empty_delta = true;
                        }
                        self.run_queue(&mut out);
                    }
                    b"reasoning" => {
                        self.stop_text(&mut out);
                        // An earlier reasoning item without output_item.done must not leak
                        // its open block into this one.
                        self.finalize_thinking(&mut out);
                        self.thinking_summary_seen = false;
                        // Fallback only: output_item.done carries the final signature.
                        self.thinking_signature = item.get("encrypted_content").bytes().into_owned();
                    }
                    _ => {}
                }
            }
            b"response.output_item.done" => {
                let item = root.get("item");
                match item.get("type").bytes().as_ref() {
                    b"message" => {
                        if self.has_text_delta {
                            return vec![out];
                        }
                        let content = item.get("content");
                        if !content.is_array() {
                            return vec![out];
                        }
                        let mut text = vec![];
                        content.each(|_, part| {
                            if part.get("type").bytes().as_ref() == b"output_text" {
                                text.extend_from_slice(&part.get("text").bytes());
                            }
                            true
                        });
                        if text.is_empty() {
                            return vec![out];
                        }
                        self.finalize_thinking(&mut out);
                        self.start_text(&mut out);
                        self.text_delta(&text, &mut out);
                        self.stop_text(&mut out);
                        self.has_text_delta = true;
                    }
                    b"function_call" => {
                        self.finalize_thinking(&mut out);
                        self.stop_text(&mut out);
                        let call = match self.call_for_event(&root, &item) {
                            Some(call) => call,
                            None => self.record_call(&root, &item),
                        };
                        self.update_identity(call, &root, &item);
                        self.update_arguments(call, &item.get("arguments").bytes(), false);
                        self.calls[call].done = true;
                        self.run_queue(&mut out);
                    }
                    b"reasoning" => {
                        self.stop_text(&mut out);
                        let signature = item.get("encrypted_content").bytes();
                        if !signature.is_empty() {
                            self.thinking_signature = signature.into_owned();
                        }
                        if self.thinking_summary_seen {
                            self.finalize_thinking(&mut out);
                        } else {
                            self.finalize_signature_only_thinking(&mut out);
                        }
                        self.thinking_signature.clear();
                        self.thinking_summary_seen = false;
                    }
                    b"web_search_call" => self.web_search_result(&root, &item, &mut out),
                    _ => {}
                }
            }
            b"response.function_call_arguments.delta" | b"response.function_call_arguments.done" => {
                let delta = kind.as_slice() == b"response.function_call_arguments.delta";
                let empty = Res::default();
                let call = match self.call_for_event(&root, &empty) {
                    Some(call) => call,
                    None => self.record_call(&root, &empty),
                };
                let arguments = root.get(if delta { "delta" } else { "arguments" }).bytes();
                self.update_arguments(call, &arguments, delta);
                self.buffered_arguments(call, &mut out);
            }
            _ => {}
        }
        if self.queue.is_empty() {
            self.append_deferred(&mut out);
        }
        vec![out]
    }
}

/// codexWebSearchQuery.
fn web_search_query(root: &Res<'_>, item: &Res<'_>) -> Vec<u8> {
    for path in ["action.query", "query", "input.query"] {
        for source in [item, root] {
            let value = trim_space(&source.get(path).bytes()).to_vec();
            if !value.is_empty() {
                return value;
            }
        }
    }
    vec![]
}

/// `json.Marshal(map[string]string{"query": query})`.
fn query_json(query: &[u8]) -> Vec<u8> {
    [&br#"{"query":"#[..], &gj::quote(query), b"}"].concat()
}

/// codexWebSearchResultContent: `web_search_result` blocks (`None`: no results array).
fn web_search_results(root: &Res<'_>, item: &Res<'_>) -> Option<Vec<u8>> {
    let mut results = item.get("results");
    if !results.is_array() {
        results = root.get("results");
    }
    if !results.is_array() {
        return None;
    }
    let mut blocks = vec![];
    results.each(|_, result| {
        let url = trim_space(&result.get("url").bytes()).to_vec();
        if url.is_empty() {
            return true;
        }
        let mut block = br#"{"type":"web_search_result","title":"","url":"","page_age":null}"#.to_vec();
        gj::set_str(&mut block, "url", &url);
        let mut title = trim_space(&result.get("title").bytes()).to_vec();
        if title.is_empty() {
            title = url;
        }
        gj::set_str(&mut block, "title", &title);
        blocks.push(block);
        true
    });
    Some(if blocks.is_empty() {
        b"[]".to_vec()
    } else {
        gj::join(&blocks)
    })
}

/// codexStopReason.
fn stop_reason(response: &Res<'_>) -> Vec<u8> {
    let has_sequence = !response.get("stop_sequence").bytes().is_empty();
    let reason = response.get("stop_reason").bytes();
    if !reason.is_empty() {
        if reason.as_ref() == b"stop" && has_sequence {
            return b"stop_sequence".to_vec();
        }
        return reason.into_owned();
    }
    let incomplete = response.get("incomplete_details.reason").bytes();
    if !incomplete.is_empty() {
        return incomplete.into_owned();
    }
    if has_sequence {
        return b"stop_sequence".to_vec();
    }
    vec![]
}

/// mapCodexStopReasonToClaude.
fn map_stop_reason(reason: &[u8], has_tool_call: bool) -> Vec<u8> {
    if has_tool_call {
        return b"tool_use".to_vec();
    }
    match reason {
        b"max_tokens" | b"max_output_tokens" => b"max_tokens".to_vec(),
        b"end_turn" | b"stop_sequence" | b"pause_turn" | b"refusal" | b"model_context_window_exceeded" => {
            reason.to_vec()
        }
        b"content_filter" => b"refusal".to_vec(),
        _ => b"end_turn".to_vec(),
    }
}

/// setClaudeStopSequence.
fn set_stop_sequence(out: &mut Vec<u8>, path: &str, response: &Res<'_>) {
    let sequence = response.get("stop_sequence");
    if !sequence.bytes().is_empty() {
        gj::set_raw(out, path, &sequence.raw);
    }
}

/// extractResponsesUsage: input tokens net of cache reads and writes.
fn usage_tokens(usage: &Res<'_>) -> (i64, i64, i64, i64) {
    if !usage.exists() || usage.kind == Kind::Null {
        return (0, 0, 0, 0);
    }
    let mut input = usage.get("input_tokens").int();
    let output = usage.get("output_tokens").int();
    let cached = usage.get("input_tokens_details.cached_tokens").int();
    let mut written = usage.get("input_tokens_details.cache_write_tokens").int();
    if written <= 0 {
        written = usage.get("input_tokens_details.cache_creation_tokens").int();
    }
    let mut deduct: i64 = 0;
    if cached > 0 {
        deduct += cached;
    }
    if written > 0 {
        deduct = deduct.checked_add(written).unwrap_or(i64::MAX);
    }
    if deduct > 0 {
        input = if input >= deduct { input - deduct } else { 0 };
    }
    (input.max(0), output, cached, written)
}

/// Usage fields of message_delta and the non-stream message.
fn set_usage(out: &mut Vec<u8>, usage: &Res<'_>) {
    let (input, output, cached, written) = usage_tokens(usage);
    gj::set_int(out, "usage.input_tokens", input);
    gj::set_int(out, "usage.output_tokens", output);
    if cached > 0 {
        gj::set_int(out, "usage.cache_read_input_tokens", cached);
    }
    if written > 0 {
        gj::set_int(out, "usage.cache_creation_input_tokens", written);
    }
    // setClaudeReasoningUsage: reasoning tokens, capped at the output tokens.
    let detail = usage.get("output_tokens_details.reasoning_tokens");
    if detail.kind != Kind::Number || detail.raw.starts_with(b"-") || detail.num < 0.0 {
        return;
    }
    let output = usage.get("output_tokens").int().max(0);
    let tokens = if detail.num >= output as f64 {
        output
    } else {
        detail.int()
    };
    gj::set_int(out, "usage.output_tokens_details.thinking_tokens", tokens);
}

fn text_of(part: &Res<'_>) -> Vec<u8> {
    let text = part.get("text");
    if text.exists() {
        text.bytes().into_owned()
    } else {
        part.bytes().into_owned()
    }
}

/// Concatenated `text` of an array (or the value itself).
fn joined_text(value: &Res<'_>) -> Vec<u8> {
    let mut out = vec![];
    if value.is_array() {
        value.each(|_, part| {
            out.extend_from_slice(&text_of(&part));
            true
        });
    } else {
        out.extend_from_slice(&value.bytes());
    }
    out
}

/// ConvertCodexResponseToClaudeNonStream.
pub fn non_stream(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let names = short_to_original(ctx.original_request);
    let root = gj::parse(body);
    let kind = root.get("type").bytes();
    if kind.as_ref() != b"response.completed" && kind.as_ref() != b"response.incomplete" {
        return Ok(vec![]);
    }
    let response = root.get("response");
    if !response.exists() {
        return Ok(vec![]);
    }
    let mut out = br#"{"id":"","type":"message","role":"assistant","model":"","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}"#.to_vec();
    gj::set_str(&mut out, "id", response.get("id").bytes());
    gj::set_str(&mut out, "model", response.get("model").bytes());
    set_usage(&mut out, &response.get("usage"));

    let mut has_tool_call = false;
    let mut seen_searches: HashSet<Vec<u8>> = HashSet::new();
    let mut blocks: Vec<Vec<u8>> = vec![];
    let output = response.get("output");
    if output.is_array() {
        output.each(|_, item| {
            match item.get("type").bytes().as_ref() {
                b"reasoning" => {
                    let signature = item.get("encrypted_content").bytes().into_owned();
                    let summary = item.get("summary");
                    let mut thinking = if summary.exists() {
                        joined_text(&summary)
                    } else {
                        vec![]
                    };
                    if thinking.is_empty() {
                        let content = item.get("content");
                        if content.exists() {
                            thinking = joined_text(&content);
                        }
                    }
                    if !thinking.is_empty() || !signature.is_empty() {
                        let mut block = br#"{"type":"thinking","thinking":""}"#.to_vec();
                        gj::set_str(&mut block, "thinking", &thinking);
                        if !signature.is_empty() {
                            gj::set_str(&mut block, "signature", &signature);
                        }
                        blocks.push(block);
                    }
                }
                b"message" => {
                    let content = item.get("content");
                    let mut add_text = |text: &[u8]| {
                        if !text.is_empty() {
                            let mut block = br#"{"type":"text","text":""}"#.to_vec();
                            gj::set_str(&mut block, "text", text);
                            blocks.push(block);
                        }
                    };
                    if content.is_array() {
                        content.each(|_, part| {
                            if part.get("type").bytes().as_ref() == b"output_text" {
                                add_text(&part.get("text").bytes());
                            }
                            true
                        });
                    } else if content.exists() {
                        add_text(&content.bytes());
                    }
                }
                b"web_search_call" => web_search_blocks(&mut blocks, &item, &mut seen_searches),
                b"function_call" => {
                    has_tool_call = true;
                    let name = item.get("name").bytes().into_owned();
                    let name = names.get(&name).cloned().unwrap_or(name);
                    let mut block = br#"{"type":"tool_use","id":"","name":"","input":{}}"#.to_vec();
                    gj::set_str(
                        &mut block,
                        "id",
                        shorten_call_id(&sanitize_claude_tool_id(&item.get("call_id").bytes())),
                    );
                    gj::set_str(&mut block, "name", &name);
                    let arguments = item.get("arguments").bytes();
                    let mut input: Vec<u8> = b"{}".to_vec();
                    if !arguments.is_empty() && gj::valid(&arguments) {
                        let parsed = gj::parse(&arguments);
                        if parsed.is_object() {
                            input = parsed.raw.to_vec();
                        }
                    }
                    gj::set_raw(&mut block, "input", &input);
                    blocks.push(block);
                }
                _ => {}
            }
            true
        });
    }
    gj::set_items(&mut out, "content", &blocks);
    gj::set_str(
        &mut out,
        "stop_reason",
        map_stop_reason(&stop_reason(&response), has_tool_call),
    );
    set_stop_sequence(&mut out, "stop_sequence", &response);
    Ok(out)
}

/// appendCodexWebSearchNonStreamBlocks: a server_tool_use and its web_search_tool_result.
fn web_search_blocks(blocks: &mut Vec<Vec<u8>>, item: &Res<'_>, seen: &mut HashSet<Vec<u8>>) {
    let id = trim_space(&item.get("id").bytes()).to_vec();
    if id.is_empty() || seen.contains(&id) {
        return;
    }
    let empty = Res::default();
    let query = web_search_query(&empty, item);
    let results = web_search_results(&empty, item);
    if query.is_empty() && results.is_none() {
        return;
    }
    let mut use_block = br#"{"type":"server_tool_use","id":"","name":"web_search","input":{}}"#.to_vec();
    gj::set_str(&mut use_block, "id", &id);
    if !query.is_empty() {
        gj::set_raw(&mut use_block, "input", query_json(&query));
    }
    blocks.push(use_block);
    let mut result_block = br#"{"type":"web_search_tool_result","tool_use_id":"","content":[]}"#.to_vec();
    gj::set_str(&mut result_block, "tool_use_id", &id);
    if let Some(results) = results {
        gj::set_raw(&mut result_block, "content", results);
    }
    blocks.push(result_block);
    seen.insert(id);
}
