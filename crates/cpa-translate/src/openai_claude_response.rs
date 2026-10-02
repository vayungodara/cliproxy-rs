//! OpenAI Chat Completions responses -> Claude Messages
//! (internal/translator/openai/claude/openai_claude_response.go).
//!
//! The stream converter keeps Claude's content blocks strictly sequential: text or
//! thinking that arrives while a tool call is open is buffered and emitted as separate
//! blocks once the tool calls are finalized.

use crate::{
    Error, ResponseCtx,
    common::{self, fix_json, map_tool_name, sanitize_claude_tool_id, sse_event, trim_space},
    stream::GoStream,
};
use cpa_common::json::{self as gj, Kind, Res};
use std::collections::{BTreeMap, HashMap};

type ToolNames = Option<HashMap<Vec<u8>, Vec<u8>>>;

/// extractOpenAIUsage: input tokens exclude cache reads and writes (saturating).
fn usage_tokens(usage: &Res<'_>) -> (i64, i64, i64, i64) {
    if !usage.exists() || usage.kind == Kind::Null {
        return (0, 0, 0, 0);
    }
    let mut input = usage.get("prompt_tokens").int();
    let output = usage.get("completion_tokens").int();
    let cached = usage.get("prompt_tokens_details.cached_tokens").int();
    let mut write = usage.get("prompt_tokens_details.cache_write_tokens").int();
    if write <= 0 {
        write = usage.get("prompt_tokens_details.cache_creation_tokens").int();
    }
    let mut deduct = cached.max(0);
    if write > 0 {
        deduct = deduct.checked_add(write).unwrap_or(i64::MAX);
    }
    if deduct > 0 {
        input = if input >= deduct { input - deduct } else { 0 };
    }
    (input.max(0), output, cached, write)
}

fn set_usage(out: &mut Vec<u8>, prefix: &str, (input, output, cached, write): (i64, i64, i64, i64)) {
    gj::set_int(out, &format!("{prefix}input_tokens"), input);
    gj::set_int(out, &format!("{prefix}output_tokens"), output);
    if cached > 0 {
        gj::set_int(out, &format!("{prefix}cache_read_input_tokens"), cached);
    }
    if write > 0 {
        gj::set_int(out, &format!("{prefix}cache_creation_input_tokens"), write);
    }
}

fn stop_reason(reason: &[u8]) -> &'static str {
    match reason {
        b"length" => "max_tokens",
        b"tool_calls" | b"function_call" => "tool_use",
        _ => "end_turn",
    }
}

/// collectOpenAIObjectReasoningTexts: the first of `reasoning_content`, `reasoning` and
/// `reasoning_details` that yields any text.
fn reasoning_texts(obj: &Res<'_>) -> Vec<Vec<u8>> {
    if !obj.exists() {
        return vec![];
    }
    ["reasoning_content", "reasoning", "reasoning_details"]
        .iter()
        .map(|path| {
            let mut texts = vec![];
            collect_reasoning(&obj.get(path), &mut texts);
            texts
        })
        .find(|texts| !texts.is_empty())
        .unwrap_or_default()
}

fn collect_reasoning(node: &Res<'_>, texts: &mut Vec<Vec<u8>>) {
    if !node.exists() {
        return;
    }
    if node.is_array() {
        node.each(|_, v| {
            collect_reasoning(&v, texts);
            true
        });
        return;
    }
    match node.kind {
        Kind::String if !node.s.is_empty() => texts.push(node.s.to_vec()),
        Kind::Json => {
            let text = node.get("text");
            if text.exists() {
                let text = text.bytes();
                if !text.is_empty() {
                    texts.push(text.into_owned());
                }
            } else if !node.raw.is_empty() && !node.raw.starts_with(b"{") && !node.raw.starts_with(b"[") {
                texts.push(node.raw.to_vec());
            }
        }
        _ => {}
    }
}

/// A tool_use block from an OpenAI tool call; arguments that are not a JSON object
/// (after FixJSON) become `{}`.
fn tool_use_block(call: &Res<'_>, names: Option<&ToolNames>) -> Vec<u8> {
    let mut block = br#"{"type":"tool_use","id":"","name":"","input":{}}"#.to_vec();
    gj::set_str(&mut block, "id", sanitize_claude_tool_id(&call.get("id").bytes()));
    let name = call.get("function.name").bytes();
    match names {
        Some(names) => gj::set_str(&mut block, "name", map_tool_name(names.as_ref(), &name)),
        None => gj::set_str(&mut block, "name", name),
    };
    let args = fix_json(&call.get("function.arguments").bytes());
    let parsed = gj::parse(&args);
    if !args.is_empty() && gj::valid(&args) && parsed.is_object() {
        gj::set_raw(&mut block, "input", &parsed.raw);
    } else {
        gj::set_raw(&mut block, "input", b"{}");
    }
    block
}

fn text_block(kind: &str, text: &[u8]) -> Vec<u8> {
    let mut block = format!(r#"{{"type":"{kind}","{kind}":""}}"#).into_bytes();
    gj::set_str(&mut block, kind, text);
    block
}

const MESSAGE_TEMPLATE: &[u8] = br#"{"id":"","type":"message","role":"assistant","model":"","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}"#;

/// convertOpenAINonStreamingToAnthropic: a whole chat completion seen on the stream path
/// of a non-streaming Claude request.
fn stream_non_streaming(payload: &[u8]) -> Vec<u8> {
    let root = gj::parse(payload);
    let mut out = MESSAGE_TEMPLATE.to_vec();
    gj::set_str(&mut out, "id", root.get("id").bytes());
    gj::set_str(&mut out, "model", root.get("model").bytes());
    let choices = root.get("choices");
    if choices.is_array()
        && let Some(choice) = choices.array().into_iter().next()
    {
        let mut blocks: Vec<Vec<u8>> = reasoning_texts(&choice.get("message"))
            .iter()
            .filter(|t| !t.is_empty())
            .map(|t| text_block("thinking", t))
            .collect();
        let content = choice.get("message.content");
        if content.exists() && !content.bytes().is_empty() {
            blocks.push(text_block("text", &content.bytes()));
        }
        let calls = choice.get("message.tool_calls");
        if calls.is_array() {
            calls.each(|_, call| {
                blocks.push(tool_use_block(&call, None));
                true
            });
        }
        if !blocks.is_empty() {
            gj::set_items(&mut out, "content", &blocks);
        }
        let finish = choice.get("finish_reason");
        if finish.exists() {
            gj::set_str(&mut out, "stop_reason", stop_reason(&finish.bytes()));
        }
    }
    let usage = root.get("usage");
    if usage.exists() {
        set_usage(&mut out, "usage.", usage_tokens(&usage));
    }
    out
}

/// ConvertOpenAIResponseToClaudeNonStream.
pub fn non_stream(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let root = gj::parse(body);
    let names = common::tool_name_map_from_claude_request(ctx.original_request);
    let mut out = MESSAGE_TEMPLATE.to_vec();
    gj::set_str(&mut out, "id", root.get("id").bytes());
    gj::set_str(&mut out, "model", root.get("model").bytes());
    let mut has_tool_call = false;
    let mut stop_set = false;
    let mut blocks: Vec<Vec<u8>> = vec![];
    let choices = root.get("choices");
    if choices.is_array()
        && let Some(choice) = choices.array().into_iter().next()
    {
        let finish = choice.get("finish_reason");
        if finish.exists() {
            gj::set_str(&mut out, "stop_reason", stop_reason(&finish.bytes()));
            stop_set = true;
        }
        let message = choice.get("message");
        if message.exists() {
            let content = message.get("content");
            if content.is_array() {
                let (mut text, mut thinking) = (vec![], vec![]);
                let flush = |blocks: &mut Vec<Vec<u8>>, buf: &mut Vec<u8>, kind: &str| {
                    if !buf.is_empty() {
                        blocks.push(text_block(kind, buf));
                        buf.clear();
                    }
                };
                for item in content.array() {
                    match item.get("type").bytes().as_ref() {
                        b"text" => {
                            flush(&mut blocks, &mut thinking, "thinking");
                            text.extend_from_slice(&item.get("text").bytes());
                        }
                        b"tool_calls" => {
                            flush(&mut blocks, &mut thinking, "thinking");
                            flush(&mut blocks, &mut text, "text");
                            let calls = item.get("tool_calls");
                            if calls.is_array() {
                                calls.each(|_, call| {
                                    has_tool_call = true;
                                    blocks.push(tool_use_block(&call, Some(&names)));
                                    true
                                });
                            }
                        }
                        b"reasoning" => {
                            flush(&mut blocks, &mut text, "text");
                            thinking.extend_from_slice(&item.get("text").bytes());
                        }
                        _ => {
                            flush(&mut blocks, &mut thinking, "thinking");
                            flush(&mut blocks, &mut text, "text");
                        }
                    }
                }
                flush(&mut blocks, &mut thinking, "thinking");
                flush(&mut blocks, &mut text, "text");
            } else if content.kind == Kind::String && !content.s.is_empty() {
                blocks.push(text_block("text", &content.s));
            }
            for text in reasoning_texts(&message) {
                if !text.is_empty() {
                    blocks.push(text_block("thinking", &text));
                }
            }
            let calls = message.get("tool_calls");
            if calls.is_array() {
                calls.each(|_, call| {
                    has_tool_call = true;
                    blocks.push(tool_use_block(&call, Some(&names)));
                    true
                });
            }
        }
    }
    if !blocks.is_empty() {
        gj::set_raw(&mut out, "content", gj::join(&blocks));
    }
    let usage = root.get("usage");
    if usage.exists() {
        set_usage(&mut out, "usage.", usage_tokens(&usage));
    }
    if !stop_set {
        gj::set_str(
            &mut out,
            "stop_reason",
            if has_tool_call { "tool_use" } else { "end_turn" },
        );
    }
    Ok(out)
}

pub fn go_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    let stream = gj::get(ctx.original_request, "stream");
    Box::new(State {
        original: ctx.original_request.to_vec(),
        streaming: stream.exists() && stream.kind != Kind::False,
        names: None,
        text_index: -1,
        thinking_index: -1,
        open_tool: -1,
        ..State::default()
    })
}

#[derive(Default)]
struct ToolCall {
    id: Vec<u8>,
    name: Vec<u8>,
    arguments: Vec<u8>,
    started: bool,
}

#[derive(Default)]
struct State {
    original: Vec<u8>,
    streaming: bool,
    names: ToolNames,
    message_id: Vec<u8>,
    model: Vec<u8>,
    saw_tool_call: bool,
    content_len: usize,
    tools: BTreeMap<i64, ToolCall>,
    text_started: bool,
    thinking_started: bool,
    finish_reason: Vec<u8>,
    blocks_stopped: bool,
    delta_sent: bool,
    message_started: bool,
    stop_sent: bool,
    tool_blocks: HashMap<i64, i64>,
    text_index: i64,
    thinking_index: i64,
    next_index: i64,
    open_tool: i64,
    /// Text (`false`) or thinking (`true`) chunks that arrived while a tool call was open.
    interleaved: Vec<(bool, Vec<u8>)>,
    usage: (i64, i64, i64, i64),
}

fn block_event(name: &str, template: &[u8], index: i64, field: Option<(&str, &[u8])>) -> Vec<u8> {
    let mut json = template.to_vec();
    gj::set_int(&mut json, "index", index);
    if let Some((path, value)) = field {
        gj::set_str(&mut json, path, value);
    }
    sse_event(name, &json)
}

const TEXT_START: &[u8] = br#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
const THINKING_START: &[u8] =
    br#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#;
const TEXT_DELTA: &[u8] = br#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":""}}"#;
const THINKING_DELTA: &[u8] =
    br#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}"#;
const BLOCK_STOP: &[u8] = br#"{"type":"content_block_stop","index":0}"#;

impl GoStream for State {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let Some(payload) = line.strip_prefix(b"data:") else {
            return Ok(vec![]);
        };
        let payload = trim_space(payload);
        if self.names.is_none() {
            self.names = common::tool_name_map_from_claude_request(&self.original);
        }
        let mut out = vec![];
        if trim_space(payload) == b"[DONE]" {
            self.finalize_blocks(&mut out);
            self.message_delta(&mut out);
            self.message_stop(&mut out);
        } else if !self.streaming {
            out.push(stream_non_streaming(payload));
        } else {
            self.chunk(payload, &mut out);
        }
        Ok(out)
    }
}

impl State {
    fn chunk(&mut self, payload: &[u8], out: &mut Vec<Vec<u8>>) {
        let root = gj::parse(payload);
        if self.message_id.is_empty() {
            self.message_id = root.get("id").bytes().into_owned();
        }
        if self.model.is_empty() {
            self.model = root.get("model").bytes().into_owned();
        }
        let delta = root.get("choices.0.delta");
        if delta.exists() {
            if !self.message_started {
                let mut start = br#"{"type":"message_start","message":{"id":"","type":"message","role":"assistant","model":"","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}}"#.to_vec();
                gj::set_str(&mut start, "message.id", &self.message_id);
                gj::set_str(&mut start, "message.model", &self.model);
                out.push(sse_event("message_start", &start));
                self.message_started = true;
            }
            for text in reasoning_texts(&delta) {
                if text.is_empty() {
                    continue;
                }
                if self.open_tool != -1 {
                    self.buffer(true, &text);
                    continue;
                }
                self.stop_text(out);
                if !self.thinking_started {
                    if self.thinking_index == -1 {
                        self.thinking_index = self.allocate();
                    }
                    out.push(block_event(
                        "content_block_start",
                        THINKING_START,
                        self.thinking_index,
                        None,
                    ));
                    self.thinking_started = true;
                }
                out.push(block_event(
                    "content_block_delta",
                    THINKING_DELTA,
                    self.thinking_index,
                    Some(("delta.thinking", &text)),
                ));
            }
            let content = delta.get("content");
            let text = content.bytes();
            if content.exists() && !text.is_empty() {
                if self.open_tool != -1 {
                    self.buffer(false, &text);
                } else {
                    if !self.text_started {
                        self.stop_thinking(out);
                        if self.text_index == -1 {
                            self.text_index = self.allocate();
                        }
                        out.push(block_event("content_block_start", TEXT_START, self.text_index, None));
                        self.text_started = true;
                    }
                    out.push(block_event(
                        "content_block_delta",
                        TEXT_DELTA,
                        self.text_index,
                        Some(("delta.text", &text)),
                    ));
                }
                self.content_len += text.len();
            }
            let calls = delta.get("tool_calls");
            if calls.is_array() {
                calls.each(|key, call| {
                    self.tool_call_delta(&key, &call, out);
                    true
                });
            }
        }

        let finish = root.get("choices.0.finish_reason");
        let reason = finish.bytes();
        if finish.exists() && !reason.is_empty() {
            self.finish_reason = match reason.as_ref() {
                b"length" | b"content_filter" => reason.to_vec(),
                _ if self.saw_tool_call => {
                    if self.valid_tool_arguments() {
                        b"tool_calls".to_vec()
                    } else {
                        b"length".to_vec()
                    }
                }
                b"tool_calls" => b"stop".to_vec(),
                _ => reason.to_vec(),
            };
            self.finalize_blocks(out);
        }

        let usage = root.get("usage");
        let has_usage = usage.exists() && usage.kind != Kind::Null;
        if has_usage {
            self.usage = usage_tokens(&usage);
        }
        let trailing_usage = has_usage
            && !root.get("choices.0").exists()
            && (!self.finish_reason.is_empty()
                || self.saw_tool_call
                || self.text_started
                || self.thinking_started
                || self.content_len > 0
                || !self.interleaved.is_empty());
        if !self.delta_sent && (!self.finish_reason.is_empty() || trailing_usage) && has_usage {
            self.finalize_blocks(out);
            self.message_delta(out);
            self.message_stop(out);
        }
    }

    fn tool_call_delta(&mut self, key: &Res<'_>, call: &Res<'_>, out: &mut Vec<Vec<u8>>) {
        let index_field = call.get("index");
        let index = if index_field.exists() {
            index_field.int()
        } else {
            key.int()
        };
        let tool = self.tools.entry(index).or_default();
        let id = call.get("id");
        if id.kind == Kind::String && !id.s.is_empty() {
            tool.id = id.s.to_vec();
        }
        let function = call.get("function");
        if function.exists() {
            if !tool.started {
                let name = function.get("name");
                if name.kind == Kind::String && !name.s.is_empty() {
                    tool.name = map_tool_name(self.names.as_ref(), &name.s);
                }
            }
            let args = function.get("arguments");
            if args.exists() {
                tool.arguments.extend_from_slice(&args.bytes());
            }
        }
        let ready = !tool.started && !tool.name.is_empty() && !tool.id.is_empty();
        if ready && !self.blocks_stopped && self.open_tool == -1 {
            self.tool_start(index, out);
        }
    }

    fn buffer(&mut self, thinking: bool, text: &[u8]) {
        match self.interleaved.last_mut() {
            Some((kind, buf)) if *kind == thinking => buf.extend_from_slice(text),
            _ => self.interleaved.push((thinking, text.to_vec())),
        }
    }

    fn allocate(&mut self) -> i64 {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    fn tool_block_index(&mut self, tool: i64) -> i64 {
        if let Some(&index) = self.tool_blocks.get(&tool) {
            return index;
        }
        let index = self.allocate();
        self.tool_blocks.insert(tool, index);
        index
    }

    fn stop_thinking(&mut self, out: &mut Vec<Vec<u8>>) {
        if self.thinking_started {
            out.push(block_event("content_block_stop", BLOCK_STOP, self.thinking_index, None));
            self.thinking_started = false;
            self.thinking_index = -1;
        }
    }

    fn stop_text(&mut self, out: &mut Vec<Vec<u8>>) {
        if self.text_started {
            out.push(block_event("content_block_stop", BLOCK_STOP, self.text_index, None));
            self.text_started = false;
            self.text_index = -1;
        }
    }

    /// emitToolUseStart.
    fn tool_start(&mut self, tool: i64, out: &mut Vec<Vec<u8>>) {
        self.stop_thinking(out);
        self.stop_text(out);
        let index = self.tool_block_index(tool);
        let call = &self.tools[&tool];
        let mut start = br#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"","name":"","input":{}}}"#.to_vec();
        gj::set_int(&mut start, "index", index);
        gj::set_str(&mut start, "content_block.id", sanitize_claude_tool_id(&call.id));
        gj::set_str(&mut start, "content_block.name", &call.name);
        out.push(sse_event("content_block_start", &start));
        self.tools.get_mut(&tool).unwrap().started = true;
        self.saw_tool_call = true;
        self.open_tool = tool;
    }

    /// finalizeSingleToolCall: a call that never started gets a belated start (named
    /// `tool_<index>` when the upstream never sent a name), then its arguments and stop.
    fn finalize_tool(&mut self, tool: i64, out: &mut Vec<Vec<u8>>) {
        let Some(call) = self.tools.get_mut(&tool) else {
            return;
        };
        if !call.started {
            if call.name.is_empty() && call.id.is_empty() && call.arguments.is_empty() {
                return;
            }
            if call.name.is_empty() {
                call.name = format!("tool_{tool}").into_bytes();
            }
            self.tool_start(tool, out);
        }
        let index = self.tool_block_index(tool);
        let arguments = &self.tools[&tool].arguments;
        if !arguments.is_empty() {
            let mut delta =
                br#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":""}}"#
                    .to_vec();
            gj::set_int(&mut delta, "index", index);
            gj::set_str(&mut delta, "delta.partial_json", fix_json(arguments));
            out.push(sse_event("content_block_delta", &delta));
        }
        out.push(block_event("content_block_stop", BLOCK_STOP, index, None));
        self.tool_blocks.remove(&tool);
        self.open_tool = -1;
    }

    /// finalizeOpenAIAnthropicContentBlocks.
    fn finalize_blocks(&mut self, out: &mut Vec<Vec<u8>>) {
        self.stop_thinking(out);
        self.stop_text(out);
        if self.blocks_stopped {
            return;
        }
        if self.open_tool != -1 {
            self.finalize_tool(self.open_tool, out);
        }
        let pending: Vec<i64> = self.tools.iter().filter(|(_, t)| !t.started).map(|(&i, _)| i).collect();
        for tool in pending {
            if self.tools.get(&tool).is_some_and(|t| !t.started) {
                self.finalize_tool(tool, out);
            }
        }
        self.blocks_stopped = true;
        for (thinking, text) in std::mem::take(&mut self.interleaved) {
            if text.is_empty() {
                continue;
            }
            let index = self.allocate();
            let (start, delta, path) = if thinking {
                (THINKING_START, THINKING_DELTA, "delta.thinking")
            } else {
                (TEXT_START, TEXT_DELTA, "delta.text")
            };
            out.push(block_event("content_block_start", start, index, None));
            out.push(block_event("content_block_delta", delta, index, Some((path, &text))));
            out.push(block_event("content_block_stop", BLOCK_STOP, index, None));
        }
    }

    /// hasValidToolCallArguments: every call with arguments has a JSON object (after
    /// FixJSON) or exactly `{}`.
    fn valid_tool_arguments(&self) -> bool {
        self.tools.values().all(|call| {
            if call.arguments.is_empty() {
                return true;
            }
            let args = trim_space(&call.arguments);
            if args.is_empty() {
                return false;
            }
            if args == b"{}" {
                return true;
            }
            let fixed = fix_json(args);
            gj::valid(&fixed) && gj::parse(&fixed).is_object()
        })
    }

    /// terminalOpenAIFinishReason over effectiveOpenAIFinishReason.
    fn terminal_reason(&self) -> Vec<u8> {
        let reason = match self.finish_reason.as_slice() {
            b"length" | b"content_filter" => self.finish_reason.clone(),
            _ if self.saw_tool_call => {
                if self.valid_tool_arguments() {
                    b"tool_calls".to_vec()
                } else {
                    b"length".to_vec()
                }
            }
            _ => self.finish_reason.clone(),
        };
        if reason.is_empty() { b"stop".to_vec() } else { reason }
    }

    fn message_delta(&mut self, out: &mut Vec<Vec<u8>>) {
        if self.delta_sent {
            return;
        }
        let mut delta = br#"{"type":"message_delta","delta":{"stop_reason":"","stop_sequence":null},"usage":{"input_tokens":0,"output_tokens":0}}"#.to_vec();
        gj::set_str(&mut delta, "delta.stop_reason", stop_reason(&self.terminal_reason()));
        set_usage(&mut delta, "usage.", self.usage);
        out.push(sse_event("message_delta", &delta));
        self.delta_sent = true;
    }

    fn message_stop(&mut self, out: &mut Vec<Vec<u8>>) {
        if !self.stop_sent {
            out.push(sse_event("message_stop", br#"{"type":"message_stop"}"#));
            self.stop_sent = true;
        }
    }
}
