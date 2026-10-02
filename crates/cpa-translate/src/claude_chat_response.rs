//! Claude Messages responses -> OpenAI Chat Completions
//! (internal/translator/claude/openai/chat-completions/claude_openai_response.go).
//! Tool arguments are collected until content_block_stop, like Go.

use crate::{Error, ResponseCtx, common::now_unix, stream::GoStream};
use cpa_common::json::{self as gj, Res};
use std::collections::{BTreeMap, HashMap};

#[derive(Default)]
struct Usage {
    input: i64,
    output: i64,
    creation: i64,
    read: i64,
    present: bool,
}

impl Usage {
    fn merge(&mut self, usage: &Res<'_>) {
        if !usage.exists() {
            return;
        }
        self.present = true;
        for (name, target) in [
            ("input_tokens", &mut self.input),
            ("output_tokens", &mut self.output),
            ("cache_creation_input_tokens", &mut self.creation),
            ("cache_read_input_tokens", &mut self.read),
        ] {
            let value = usage.get(name);
            if value.exists() {
                *target = value.int();
            }
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        let prompt = self.input.wrapping_add(self.creation).wrapping_add(self.read);
        for (path, value) in [
            ("usage.prompt_tokens", prompt),
            ("usage.completion_tokens", self.output),
            ("usage.total_tokens", prompt.wrapping_add(self.output)),
            ("usage.prompt_tokens_details.cached_tokens", self.read),
            ("usage.prompt_tokens_details.cached_creation_tokens", self.creation),
            ("usage.prompt_tokens_details.cache_write_tokens", self.creation),
        ] {
            gj::set_int(out, path, value);
        }
    }
}

struct Tool {
    id: Vec<u8>,
    name: Vec<u8>,
    index: i64,
    arguments: Vec<u8>,
}

fn finish_reason(reason: &[u8]) -> &'static str {
    match reason {
        b"tool_use" => "tool_calls",
        b"max_tokens" => "length",
        b"refusal" | b"sensitive" => "content_filter",
        _ => "stop",
    }
}

/// The payload of a `data:` line, as Go slices and trims it.
pub(crate) fn data_payload(line: &[u8]) -> Option<&[u8]> {
    line.strip_prefix(b"data:").map(crate::common::trim_space)
}

pub fn go_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    Box::new(State {
        model: ctx.model.as_bytes().to_vec(),
        ..State::default()
    })
}

#[derive(Default)]
struct State {
    model: Vec<u8>,
    id: Vec<u8>,
    created: i64,
    usage: Usage,
    tools: HashMap<i64, Tool>,
    next_tool: i64,
    trailing_sent: bool,
}

impl GoStream for State {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        Ok(self.convert(line).into_iter().collect())
    }
}

impl State {
    fn convert(&mut self, line: &[u8]) -> Option<Vec<u8>> {
        let payload = data_payload(line)?;
        let root = gj::parse(payload);
        let mut out =
            br#"{"id":"","object":"chat.completion.chunk","created":0,"model":"","choices":[{"index":0,"delta":{},"finish_reason":null}]}"#
                .to_vec();
        if !self.model.is_empty() {
            gj::set_str(&mut out, "model", &self.model);
        }
        if !self.id.is_empty() {
            gj::set_str(&mut out, "id", &self.id);
        }
        if self.created > 0 {
            gj::set_int(&mut out, "created", self.created);
        }
        let index = root.get("index").int();
        match &*root.get("type").str() {
            "message_start" => {
                let message = root.get("message");
                if message.exists() {
                    self.id = message.get("id").bytes().into_owned();
                    self.created = now_unix();
                    gj::set_str(&mut out, "id", &self.id);
                    gj::set_str(&mut out, "model", &self.model);
                    gj::set_int(&mut out, "created", self.created);
                    gj::set_str(&mut out, "choices.0.delta.role", "assistant");
                    self.next_tool = 0;
                    self.usage.merge(&message.get("usage"));
                }
                Some(out)
            }
            "content_block_start" => {
                let block = root.get("content_block");
                if block.get("type").str() == "tool_use" {
                    let tool = Tool {
                        id: block.get("id").bytes().into_owned(),
                        name: block.get("name").bytes().into_owned(),
                        index: self.next_tool,
                        arguments: vec![],
                    };
                    self.next_tool += 1;
                    self.tools.insert(index, tool);
                }
                None
            }
            "content_block_delta" => {
                let delta = root.get("delta");
                let (field, target) = match &*delta.get("type").str() {
                    "text_delta" => ("text", "choices.0.delta.content"),
                    "thinking_delta" => ("thinking", "choices.0.delta.reasoning_content"),
                    "input_json_delta" => {
                        let partial = delta.get("partial_json");
                        if partial.exists()
                            && let Some(tool) = self.tools.get_mut(&index)
                        {
                            tool.arguments.extend_from_slice(&partial.bytes());
                        }
                        return None;
                    }
                    _ => return None,
                };
                let value = delta.get(field);
                if !value.exists() {
                    return None;
                }
                gj::set_str(&mut out, target, value.bytes());
                Some(out)
            }
            "content_block_stop" => {
                let mut tool = self.tools.remove(&index)?;
                if tool.arguments.is_empty() {
                    tool.arguments = b"{}".to_vec();
                }
                gj::set_int(&mut out, "choices.0.delta.tool_calls.0.index", tool.index);
                gj::set_str(&mut out, "choices.0.delta.tool_calls.0.id", &tool.id);
                gj::set_str(&mut out, "choices.0.delta.tool_calls.0.type", "function");
                gj::set_str(&mut out, "choices.0.delta.tool_calls.0.function.name", &tool.name);
                gj::set_str(
                    &mut out,
                    "choices.0.delta.tool_calls.0.function.arguments",
                    &tool.arguments,
                );
                Some(out)
            }
            "message_delta" => {
                let reason = root.get("delta.stop_reason");
                if root.get("delta").exists() && reason.exists() {
                    gj::set_str(&mut out, "choices.0.finish_reason", finish_reason(&reason.bytes()));
                }
                let usage = root.get("usage");
                if usage.exists() {
                    self.usage.merge(&usage);
                    self.usage.write(&mut out);
                }
                Some(out)
            }
            "message_stop" => {
                if !self.usage.present || self.trailing_sent {
                    return None;
                }
                self.trailing_sent = true;
                let mut out =
                    br#"{"id":"","object":"chat.completion.chunk","created":0,"model":"","choices":[]}"#.to_vec();
                if !self.id.is_empty() {
                    gj::set_str(&mut out, "id", &self.id);
                }
                if !self.model.is_empty() {
                    gj::set_str(&mut out, "model", &self.model);
                }
                if self.created > 0 {
                    gj::set_int(&mut out, "created", self.created);
                }
                self.usage.write(&mut out);
                Some(out)
            }
            "error" => {
                let error = root.get("error");
                if !error.exists() {
                    return None;
                }
                let mut out = br#"{"error":{"message":"","type":""}}"#.to_vec();
                gj::set_str(&mut out, "error.message", error.get("message").bytes());
                gj::set_str(&mut out, "error.type", error.get("type").bytes());
                Some(out)
            }
            _ => None,
        }
    }
}

/// Buffered Claude SSE (the executor streams upstream for non-Claude clients) to one
/// chat.completion.
pub fn non_stream(_: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let mut out = br#"{"id":"","object":"chat.completion","created":0,"model":"","choices":[{"index":0,"message":{"role":"assistant","content":""},"finish_reason":"stop"}],"usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}}"#.to_vec();
    let mut id = vec![];
    let mut model = vec![];
    let mut created = 0;
    let mut reason = vec![];
    let mut content = vec![];
    let mut reasoning: Option<Vec<u8>> = None;
    let mut usage = Usage::default();
    let mut tools: BTreeMap<i64, Tool> = BTreeMap::new();
    for line in body.split(|&c| c == b'\n') {
        let Some(payload) = data_payload(line) else {
            continue;
        };
        let root = gj::parse(payload);
        let index = root.get("index").int();
        match &*root.get("type").str() {
            "message_start" => {
                let message = root.get("message");
                if message.exists() {
                    id = message.get("id").bytes().into_owned();
                    model = message.get("model").bytes().into_owned();
                    created = now_unix();
                    usage.merge(&message.get("usage"));
                }
            }
            "content_block_start" => {
                let block = root.get("content_block");
                if block.get("type").str() == "tool_use" {
                    tools.insert(
                        index,
                        Tool {
                            id: block.get("id").bytes().into_owned(),
                            name: block.get("name").bytes().into_owned(),
                            index: 0,
                            arguments: vec![],
                        },
                    );
                }
            }
            "content_block_delta" => {
                let delta = root.get("delta");
                match &*delta.get("type").str() {
                    "text_delta" => {
                        let text = delta.get("text");
                        if text.exists() {
                            content.extend_from_slice(&text.bytes());
                        }
                    }
                    "thinking_delta" => {
                        let thinking = delta.get("thinking");
                        if thinking.exists() {
                            reasoning.get_or_insert_default().extend_from_slice(&thinking.bytes());
                        }
                    }
                    "input_json_delta" => {
                        let partial = delta.get("partial_json");
                        if partial.exists()
                            && let Some(tool) = tools.get_mut(&index)
                        {
                            tool.arguments.extend_from_slice(&partial.bytes());
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some(tool) = tools.get_mut(&index)
                    && tool.arguments.is_empty()
                {
                    tool.arguments = b"{}".to_vec();
                }
            }
            "message_delta" => {
                let stop = root.get("delta.stop_reason");
                if root.get("delta").exists() && stop.exists() {
                    reason = stop.bytes().into_owned();
                }
                usage.merge(&root.get("usage"));
            }
            _ => {}
        }
    }
    if usage.present {
        usage.write(&mut out);
    }
    gj::set_str(&mut out, "id", &id);
    gj::set_int(&mut out, "created", created);
    gj::set_str(&mut out, "model", &model);
    gj::set_str(&mut out, "choices.0.message.content", &content);
    if let Some(reasoning) = reasoning {
        gj::set_str(&mut out, "choices.0.message.reasoning_content", &reasoning);
    }
    let mut count = 0;
    // Go walks indexes 0..=max; negative indexes never appear.
    for tool in tools.range(0..).map(|(_, t)| t) {
        let path = format!("choices.0.message.tool_calls.{count}");
        gj::set_str(&mut out, &format!("{path}.id"), &tool.id);
        gj::set_str(&mut out, &format!("{path}.type"), "function");
        gj::set_str(&mut out, &format!("{path}.function.name"), &tool.name);
        gj::set_str(&mut out, &format!("{path}.function.arguments"), &tool.arguments);
        count += 1;
    }
    if count > 0 {
        gj::set_str(&mut out, "choices.0.finish_reason", "tool_calls");
    } else if finish_reason(&reason) != "stop" {
        gj::set_str(&mut out, "choices.0.finish_reason", finish_reason(&reason));
    }
    Ok(out)
}
