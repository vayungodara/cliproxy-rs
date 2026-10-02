//! Codex (OpenAI Responses) events -> OpenAI Chat Completions
//! (internal/translator/codex/openai/chat-completions/codex_openai_response.go).

use crate::{
    Error, ResponseCtx, apply_patch,
    claude_responses::{qualify_namespace_name, tool_descriptors, tool_winners},
    codex_chat_request::{request_tool_names, short_name_map},
    common::{now_unix, trim_space},
    stream::GoStream,
};
use cpa_common::json::{self as gj, Kind, Res};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// buildReverseMapFromOriginalOpenAI: short name -> declared name.
fn reverse_names(original: &[u8]) -> HashMap<Vec<u8>, Vec<u8>> {
    short_name_map(&request_tool_names(original))
        .into_iter()
        .map(|(name, short)| (short, name))
        .collect()
}

fn restore_name(original: &[u8], name: Vec<u8>) -> Vec<u8> {
    reverse_names(original).remove(&name).unwrap_or(name)
}

/// mimeTypeFromCodexOutputFormat.
fn image_mime(format: &[u8]) -> Vec<u8> {
    if format.is_empty() {
        return b"image/png".to_vec();
    }
    if format.contains(&b'/') {
        return format.to_vec();
    }
    match crate::common::go_lower(format).as_slice() {
        b"jpg" | b"jpeg" => b"image/jpeg".to_vec(),
        b"webp" => b"image/webp".to_vec(),
        b"gif" => b"image/gif".to_vec(),
        _ => b"image/png".to_vec(),
    }
}

/// codexResponseServiceTier: a nonempty string tier, trimmed.
fn response_tier(response: &Res<'_>) -> Vec<u8> {
    let tier = response.get("service_tier");
    if tier.kind != Kind::String {
        return vec![];
    }
    trim_space(&tier.s).to_vec()
}

/// setCodexCacheWriteTokens: copies an all-digit integer as is.
fn set_cache_write_tokens(out: &mut Vec<u8>, usage: &Res<'_>) {
    let value = usage.get("input_tokens_details.cache_write_tokens");
    if !value.exists() || value.kind == Kind::Null {
        return;
    }
    if value.kind != Kind::Number || value.raw.is_empty() || !value.raw.iter().all(u8::is_ascii_digit) {
        return;
    }
    gj::set_raw(out, "usage.prompt_tokens_details.cache_write_tokens", &value.raw);
    gj::set_raw(out, "usage.prompt_tokens_details.cached_creation_tokens", &value.raw);
}

fn set_usage(out: &mut Vec<u8>, usage: &Res<'_>) {
    if !usage.exists() {
        return;
    }
    for (from, to) in [
        ("output_tokens", "usage.completion_tokens"),
        ("total_tokens", "usage.total_tokens"),
        ("input_tokens", "usage.prompt_tokens"),
        (
            "input_tokens_details.cached_tokens",
            "usage.prompt_tokens_details.cached_tokens",
        ),
    ] {
        let v = usage.get(from);
        if v.exists() {
            gj::set_int(out, to, v.int());
        }
    }
    set_cache_write_tokens(out, usage);
    let reasoning = usage.get("output_tokens_details.reasoning_tokens");
    if reasoning.exists() {
        gj::set_int(out, "usage.completion_tokens_details.reasoning_tokens", reasoning.int());
    }
}

fn tool_arguments(item: &Res<'_>) -> Vec<u8> {
    if item.get("type").bytes().as_ref() == b"custom_tool_call" {
        return item.get("input").bytes().into_owned();
    }
    item.get("arguments").bytes().into_owned()
}

fn is_tool_call(kind: &[u8]) -> bool {
    kind == b"function_call" || kind == b"custom_tool_call"
}

/// isOriginalCustomPatch: a custom call whose winning declaration is the apply_patch
/// custom tool, unless an ordinary function shares its name.
fn is_custom_patch(original: &[u8], item: &Res<'_>) -> bool {
    if item.get("type").bytes().as_ref() != b"custom_tool_call" {
        return false;
    }
    let name = qualify_namespace_name(&item.get("namespace").bytes(), &item.get("name").bytes());
    let tools = gj::get(original, "tools");
    if tools.array().iter().any(|t| {
        t.get("type").bytes().as_ref() == b"function" && t.get("function.name").bytes().as_ref() == name.as_slice()
    }) {
        return false;
    }
    let root = gj::parse(original);
    let descriptors = tool_descriptors(&root);
    tool_winners(&descriptors)
        .get(&name)
        .is_some_and(|&w| apply_patch::is_custom_tool(&descriptors[w].tool))
}

fn image_payload(index: usize, url: &[u8]) -> Vec<u8> {
    let mut payload = br#"{"type":"image_url","image_url":{"url":""}}"#.to_vec();
    gj::set_int(&mut payload, "index", index as i64);
    gj::set_str(&mut payload, "image_url.url", url);
    payload
}

#[derive(Default)]
struct ToolState {
    index: i64,
    arguments_emitted: bool,
    patch: bool,
    input_started: bool,
    input_closed: bool,
    done: bool,
}

impl ToolState {
    /// finishPatchChatArguments.
    fn finish_patch(&mut self, input: &[u8]) -> Vec<u8> {
        if self.input_closed {
            return vec![];
        }
        self.input_closed = true;
        if self.input_started {
            return br#""}"#.to_vec();
        }
        apply_patch::wrap_input(input)
    }
}

pub fn go_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    Box::new(State {
        original: ctx.original_request.to_vec(),
        model: ctx.model.as_bytes().to_vec(),
        model_name: ctx.model.as_bytes().to_vec(),
        function_index: -1,
        ..State::default()
    })
}

#[derive(Default)]
struct State {
    original: Vec<u8>,
    model_name: Vec<u8>,
    service_tier: Vec<u8>,
    response_id: Vec<u8>,
    created_at: i64,
    model: Vec<u8>,
    function_index: i64,
    tools: Vec<ToolState>,
    tool_keys: HashMap<Vec<u8>, usize>,
    current: Option<usize>,
    image_hashes: HashMap<Vec<u8>, [u8; 32]>,
}

impl State {
    fn register(&mut self, event: &Res<'_>, item: &Res<'_>, state: ToolState) -> usize {
        let index = self.tools.len();
        self.tools.push(state);
        for id in [event.get("item_id").bytes(), item.get("id").bytes()] {
            if !id.is_empty() {
                self.tool_keys.insert([&b"item:"[..], &id].concat(), index);
            }
        }
        let output = event.get("output_index");
        if output.exists() {
            self.tool_keys.insert([&b"output:"[..], &output.raw].concat(), index);
        }
        self.current = Some(index);
        index
    }

    fn find(&self, event: &Res<'_>, item: &Res<'_>) -> Option<usize> {
        for id in [event.get("item_id").bytes(), item.get("id").bytes()] {
            if !id.is_empty()
                && let Some(&i) = self.tool_keys.get(&[&b"item:"[..], &id].concat())
            {
                return Some(i);
            }
        }
        let output = event.get("output_index");
        if output.exists()
            && let Some(&i) = self.tool_keys.get(&[&b"output:"[..], &output.raw].concat())
        {
            return Some(i);
        }
        self.current
    }

    /// False when this image repeats the item's previous image.
    fn new_image(&mut self, item_id: &[u8], b64: &[u8]) -> bool {
        if item_id.is_empty() {
            return true;
        }
        let hash: [u8; 32] = Sha256::digest(b64).into();
        if self.image_hashes.get(item_id) == Some(&hash) {
            return false;
        }
        self.image_hashes.insert(item_id.to_vec(), hash);
        true
    }

    fn push_image(t: &mut Vec<u8>, format: &[u8], b64: &[u8]) {
        let url = [&b"data:"[..], &image_mime(format), b";base64,", b64].concat();
        let images = gj::get(t, "choices.0.delta.images");
        if !images.exists() || !images.is_array() {
            gj::set_raw(t, "choices.0.delta.images", b"[]");
        }
        let count = gj::get(t, "choices.0.delta.images").array().len();
        gj::set_str(t, "choices.0.delta.role", "assistant");
        gj::set_raw(t, "choices.0.delta.images.-1", image_payload(count, &url));
    }

    fn arguments_chunk(t: &mut Vec<u8>, index: i64, arguments: &[u8]) {
        let mut item = br#"{"index":0,"function":{"arguments":""}}"#.to_vec();
        gj::set_int(&mut item, "index", index);
        gj::set_str(&mut item, "function.arguments", arguments);
        gj::set_raw(t, "choices.0.delta.tool_calls", b"[]");
        gj::set_raw(t, "choices.0.delta.tool_calls.-1", item);
    }

    fn call_item(&self, index: i64, item: &Res<'_>, arguments: &[u8]) -> Vec<u8> {
        let mut call = br#"{"index":0,"id":"","type":"function","function":{"name":"","arguments":""}}"#.to_vec();
        gj::set_int(&mut call, "index", index);
        gj::set_str(&mut call, "id", item.get("call_id").bytes());
        let name = restore_name(&self.original, item.get("name").bytes().into_owned());
        gj::set_str(&mut call, "function.name", name);
        gj::set_str(&mut call, "function.arguments", arguments);
        call
    }

    fn convert(&mut self, payload: &[u8]) -> Option<Vec<u8>> {
        let root = gj::parse(payload);
        let mut t = br#"{"id":"","object":"chat.completion.chunk","created":12345,"model":"model","choices":[{"index":0,"delta":{},"finish_reason":null,"native_finish_reason":null}]}"#.to_vec();
        let mut tier = response_tier(&root.get("response"));
        if tier.is_empty() {
            tier = response_tier(&root);
        }
        if !tier.is_empty() {
            self.service_tier = tier;
        }
        if !self.service_tier.is_empty() {
            gj::set_str(&mut t, "service_tier", &self.service_tier);
        }
        let kind = root.get("type").bytes().into_owned();
        if kind == b"response.created" {
            self.response_id = root.get("response.id").bytes().into_owned();
            self.created_at = root.get("response.created_at").int();
            self.model = root.get("response.model").bytes().into_owned();
            return None;
        }
        let model = root.get("model");
        if model.exists() {
            gj::set_str(&mut t, "model", model.bytes());
        } else if !self.model.is_empty() {
            gj::set_str(&mut t, "model", &self.model);
        } else if !self.model_name.is_empty() {
            gj::set_str(&mut t, "model", &self.model_name);
        }
        gj::set_int(&mut t, "created", self.created_at);
        gj::set_str(&mut t, "id", &self.response_id);
        set_usage(&mut t, &root.get("response.usage"));

        match kind.as_slice() {
            b"response.reasoning_summary_text.delta" | b"response.reasoning_text.delta" => {
                let delta = root.get("delta");
                if delta.exists() {
                    gj::set_str(&mut t, "choices.0.delta.role", "assistant");
                    gj::set_str(&mut t, "choices.0.delta.reasoning_content", delta.bytes());
                }
            }
            b"response.reasoning_summary_text.done" | b"response.reasoning_text.done" => {
                gj::set_str(&mut t, "choices.0.delta.role", "assistant");
                gj::set_str(&mut t, "choices.0.delta.reasoning_content", "\n\n");
            }
            b"response.output_text.delta" => {
                let delta = root.get("delta");
                if delta.exists() {
                    gj::set_str(&mut t, "choices.0.delta.role", "assistant");
                    gj::set_str(&mut t, "choices.0.delta.content", delta.bytes());
                }
            }
            b"response.image_generation_call.partial_image" => {
                let b64 = root.get("partial_image_b64").bytes();
                if b64.is_empty() || !self.new_image(&root.get("item_id").bytes(), &b64) {
                    return None;
                }
                Self::push_image(&mut t, &root.get("output_format").bytes(), &b64);
            }
            b"response.completed" | b"response.incomplete" => {
                let mut finish: Vec<u8> = b"stop".to_vec();
                let mut native = finish.clone();
                if kind == b"response.incomplete" {
                    native = root.get("response.incomplete_details.reason").bytes().into_owned();
                    match native.as_slice() {
                        b"max_tokens" | b"max_output_tokens" => finish = b"length".to_vec(),
                        b"content_filter" => finish = b"content_filter".to_vec(),
                        _ => {}
                    }
                } else if self.function_index != -1 {
                    finish = b"tool_calls".to_vec();
                    native = finish.clone();
                }
                gj::set_str(&mut t, "choices.0.finish_reason", finish);
                gj::set_str(&mut t, "choices.0.native_finish_reason", native);
            }
            b"response.output_item.added" => {
                let item = root.get("item");
                if !item.exists() || !is_tool_call(&item.get("type").bytes()) {
                    return None;
                }
                self.function_index += 1;
                let state = ToolState {
                    index: self.function_index,
                    patch: is_custom_patch(&self.original, &item),
                    ..ToolState::default()
                };
                self.register(&root, &item, state);
                let call = self.call_item(self.function_index, &item, b"");
                gj::set_str(&mut t, "choices.0.delta.role", "assistant");
                gj::set_raw(&mut t, "choices.0.delta.tool_calls", b"[]");
                gj::set_raw(&mut t, "choices.0.delta.tool_calls.-1", call);
            }
            b"response.function_call_arguments.delta" | b"response.custom_tool_call_input.delta" => {
                let mut delta = root.get("delta").bytes().into_owned();
                let i = self.find(&root, &Res::default())?;
                let state = &mut self.tools[i];
                if state.done || delta.is_empty() {
                    return None;
                }
                state.arguments_emitted = true;
                if state.patch {
                    delta = apply_patch::escape_input_fragment(&delta);
                    if !state.input_started {
                        delta = [&br#"{"input":""#[..], &delta].concat();
                        state.input_started = true;
                    }
                }
                Self::arguments_chunk(&mut t, state.index, &delta);
            }
            b"response.function_call_arguments.done" | b"response.custom_tool_call_input.done" => {
                let i = self.find(&root, &Res::default())?;
                let state = &mut self.tools[i];
                if state.done || state.input_closed || (state.arguments_emitted && !state.patch) {
                    return None;
                }
                let field = if kind == b"response.custom_tool_call_input.done" {
                    "input"
                } else {
                    "arguments"
                };
                state.arguments_emitted = true;
                let mut args = root.get(field).bytes().into_owned();
                if state.patch {
                    args = state.finish_patch(&args);
                }
                if args.is_empty() {
                    return None;
                }
                Self::arguments_chunk(&mut t, state.index, &args);
            }
            b"response.output_item.done" => {
                let item = root.get("item");
                if !item.exists() {
                    return None;
                }
                let item_kind = item.get("type").bytes().into_owned();
                if item_kind == b"image_generation_call" {
                    let b64 = item.get("result").bytes();
                    if b64.is_empty() || !self.new_image(&item.get("id").bytes(), &b64) {
                        return None;
                    }
                    Self::push_image(&mut t, &item.get("output_format").bytes(), &b64);
                    return Some(t);
                }
                if !is_tool_call(&item_kind) {
                    return None;
                }
                if let Some(i) = self.find(&root, &item) {
                    let state = &mut self.tools[i];
                    if state.done {
                        return None;
                    }
                    state.done = true;
                    if state.arguments_emitted && (!state.patch || state.input_closed) {
                        return None;
                    }
                    state.arguments_emitted = true;
                    let mut args = tool_arguments(&item);
                    if state.patch {
                        args = state.finish_patch(&args);
                    }
                    if args.is_empty() {
                        return None;
                    }
                    Self::arguments_chunk(&mut t, state.index, &args);
                    return Some(t);
                }
                // The upstream skipped output_item.added: emit the whole call now.
                self.function_index += 1;
                let mut state = ToolState {
                    index: self.function_index,
                    arguments_emitted: true,
                    done: true,
                    patch: is_custom_patch(&self.original, &item),
                    ..ToolState::default()
                };
                let mut args = tool_arguments(&item);
                if state.patch {
                    args = state.finish_patch(&args);
                }
                self.register(&root, &item, state);
                gj::set_raw(&mut t, "choices.0.delta.tool_calls", b"[]");
                let call = self.call_item(self.function_index, &item, &args);
                gj::set_str(&mut t, "choices.0.delta.role", "assistant");
                gj::set_raw(&mut t, "choices.0.delta.tool_calls.-1", call);
            }
            _ => return None,
        }
        Some(t)
    }
}

impl GoStream for State {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let Some(payload) = line.strip_prefix(b"data:") else {
            return Ok(vec![]);
        };
        Ok(self.convert(trim_space(payload)).into_iter().collect())
    }
}

/// ConvertCodexResponseToOpenAINonStream: a terminal Codex event as one chat completion.
pub fn non_stream(ctx: &ResponseCtx<'_>, raw: &[u8]) -> Result<Vec<u8>, Error> {
    let root = gj::parse(raw);
    let kind = root.get("type").bytes();
    if kind.as_ref() != b"response.completed" && kind.as_ref() != b"response.incomplete" {
        return Ok(vec![]);
    }
    let response = root.get("response");
    let mut t = br#"{"id":"","object":"chat.completion","created":123456,"model":"model","choices":[{"index":0,"message":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":null},"finish_reason":null,"native_finish_reason":null}]}"#.to_vec();
    let mut tier = response_tier(&response);
    if tier.is_empty() {
        tier = response_tier(&root);
    }
    if !tier.is_empty() {
        gj::set_str(&mut t, "service_tier", tier);
    }
    let model = response.get("model");
    if model.exists() {
        gj::set_str(&mut t, "model", model.bytes());
    }
    let created = response.get("created_at");
    gj::set_int(
        &mut t,
        "created",
        if created.exists() { created.int() } else { now_unix() },
    );
    let id = response.get("id");
    if id.exists() {
        gj::set_str(&mut t, "id", id.bytes());
    }
    set_usage(&mut t, &response.get("usage"));

    let mut calls: Vec<Vec<u8>> = vec![];
    let output = response.get("output");
    if output.is_array() {
        let (mut text, mut reasoning) = (vec![], vec![]);
        let mut images = vec![];
        for item in output.array() {
            match item.get("type").bytes().as_ref() {
                b"reasoning" => {
                    let summary = item.get("summary");
                    if summary.is_array()
                        && let Some(first) = summary
                            .array()
                            .into_iter()
                            .find(|s| s.get("type").bytes().as_ref() == b"summary_text")
                    {
                        reasoning.extend_from_slice(&first.get("text").bytes());
                    }
                    let content = item.get("content");
                    if content.is_array() {
                        for c in content.array() {
                            if c.get("type").bytes().as_ref() == b"reasoning_text" {
                                reasoning.extend_from_slice(&c.get("text").bytes());
                            }
                        }
                    }
                }
                b"message" => {
                    let content = item.get("content");
                    if content.is_array()
                        && let Some(first) = content
                            .array()
                            .into_iter()
                            .find(|c| c.get("type").bytes().as_ref() == b"output_text")
                    {
                        text.extend_from_slice(&first.get("text").bytes());
                    }
                }
                b"function_call" | b"custom_tool_call" => {
                    let mut call = br#"{"id":"","type":"function","function":{"name":"","arguments":""}}"#.to_vec();
                    let call_id = item.get("call_id");
                    if call_id.exists() {
                        gj::set_str(&mut call, "id", call_id.bytes());
                    }
                    let name = item.get("name");
                    if name.exists() {
                        gj::set_str(
                            &mut call,
                            "function.name",
                            restore_name(ctx.original_request, name.bytes().into_owned()),
                        );
                    }
                    let mut args = tool_arguments(&item);
                    if is_custom_patch(ctx.original_request, &item) {
                        args = apply_patch::wrap_input(&args);
                    }
                    gj::set_str(&mut call, "function.arguments", args);
                    calls.push(call);
                }
                b"image_generation_call" => {
                    let b64 = item.get("result").bytes();
                    if !b64.is_empty() {
                        let url = [
                            &b"data:"[..],
                            &image_mime(&item.get("output_format").bytes()),
                            b";base64,",
                            &b64,
                        ]
                        .concat();
                        images.push(image_payload(images.len(), &url));
                    }
                }
                _ => {}
            }
        }
        if !text.is_empty() {
            gj::set_str(&mut t, "choices.0.message.content", text);
        }
        if !reasoning.is_empty() {
            gj::set_str(&mut t, "choices.0.message.reasoning_content", reasoning);
        }
        if !calls.is_empty() {
            gj::set_raw(&mut t, "choices.0.message.tool_calls", gj::join(&calls));
        }
        if !images.is_empty() {
            gj::set_raw(&mut t, "choices.0.message.images", gj::join(&images));
        }
    }
    let status = response.get("status");
    if status.exists() {
        let (finish, native): (Vec<u8>, Vec<u8>) = match status.bytes().as_ref() {
            b"completed" if !calls.is_empty() => (b"tool_calls".to_vec(), b"tool_calls".to_vec()),
            b"completed" => (b"stop".to_vec(), b"stop".to_vec()),
            b"incomplete" => {
                let native = response.get("incomplete_details.reason").bytes().into_owned();
                let finish = match native.as_slice() {
                    b"max_tokens" | b"max_output_tokens" => b"length".to_vec(),
                    b"content_filter" => b"content_filter".to_vec(),
                    _ => b"stop".to_vec(),
                };
                (finish, native)
            }
            _ => (vec![], vec![]),
        };
        if !finish.is_empty() {
            gj::set_str(&mut t, "choices.0.finish_reason", finish);
            gj::set_str(&mut t, "choices.0.native_finish_reason", native);
        }
    }
    Ok(t)
}
