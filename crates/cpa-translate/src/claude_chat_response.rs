//! Claude Chat responses deliberately collect tool JSON until block_stop, like Go.
use crate::{
    Error, ResponseCtx, StreamTranslator,
    json::{self, set, set_string},
};
use bytes::Bytes;
use gjson::Value;
use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};

const CHUNK: &str = r#"{"id":"","object":"chat.completion.chunk","created":0,"model":"","choices":[{"index":0,"delta":{},"finish_reason":null}]}"#;
const RESPONSE: &str = r#"{"id":"","object":"chat.completion","created":0,"model":"","choices":[{"index":0,"message":{"role":"assistant","content":""},"finish_reason":"stop"}],"usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}}"#;

#[derive(Default)]
struct Usage {
    input: i64,
    output: i64,
    creation: i64,
    read: i64,
    present: bool,
}

impl Usage {
    fn merge(&mut self, usage: &Value<'_>) {
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
                *target = value.i64();
            }
        }
    }

    fn write(&self, out: &mut String) {
        let prompt = self.input.wrapping_add(self.creation).wrapping_add(self.read);
        for (path, value) in [
            ("usage.prompt_tokens", prompt),
            ("usage.completion_tokens", self.output),
            ("usage.total_tokens", prompt.wrapping_add(self.output)),
            ("usage.prompt_tokens_details.cached_tokens", self.read),
            ("usage.prompt_tokens_details.cached_creation_tokens", self.creation),
            ("usage.prompt_tokens_details.cache_write_tokens", self.creation),
        ] {
            set(out, path, &value.to_string());
        }
    }
}

struct Tool {
    id: String,
    name: String,
    index: usize,
    arguments: String,
}

impl Tool {
    fn from_block(block: &Value<'_>, index: usize) -> Self {
        Self {
            id: block.get("id").str().into(),
            name: block.get("name").str().into(),
            index,
            arguments: String::new(),
        }
    }
    fn write(&self, out: &mut String, path: &str, stream: bool) {
        if stream {
            set(out, &format!("{path}.index"), &self.index.to_string());
        }
        set_string(out, &format!("{path}.id"), &self.id);
        set_string(out, &format!("{path}.type"), "function");
        set_string(out, &format!("{path}.function.name"), &self.name);
        set_string(out, &format!("{path}.function.arguments"), &self.arguments);
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn finish_reason(reason: &str) -> &'static str {
    match reason {
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        "refusal" | "sensitive" => "content_filter",
        _ => "stop",
    }
}

pub fn stream(ctx: &ResponseCtx<'_>) -> Box<dyn StreamTranslator> {
    Box::new(Stream {
        model: ctx.model.into(),
        id: String::new(),
        created: 0,
        usage: Usage::default(),
        tools: BTreeMap::new(),
        next_tool: 0,
        trailing_sent: false,
    })
}

struct Stream {
    model: String,
    id: String,
    created: u64,
    usage: Usage,
    tools: BTreeMap<i64, Tool>,
    next_tool: usize,
    trailing_sent: bool,
}

impl Stream {
    fn chunk(&self) -> String {
        let mut out = CHUNK.to_owned();
        set_string(&mut out, "model", &self.model);
        set_string(&mut out, "id", &self.id);
        set(&mut out, "created", &self.created.to_string());
        out
    }

    fn convert(&mut self, payload: &str) -> Option<String> {
        let root = gjson::parse(payload);
        let mut out = self.chunk();
        let index = root.get("index").i64();
        match root.get("type").str() {
            "message_start" => {
                let message = root.get("message");
                if message.exists() {
                    self.id = message.get("id").str().into();
                    self.created = now();
                    self.next_tool = 0;
                    self.usage.merge(&message.get("usage"));
                    out = self.chunk();
                    set_string(&mut out, "choices.0.delta.role", "assistant");
                }
            }
            "content_block_start" => {
                let block = root.get("content_block");
                if block.get("type").str() == "tool_use" {
                    self.tools.insert(index, Tool::from_block(&block, self.next_tool));
                    self.next_tool += 1;
                }
                return None;
            }
            "content_block_delta" => {
                let delta = root.get("delta");
                match delta.get("type").str() {
                    "text_delta" | "thinking_delta" => {
                        let (input, output) = if delta.get("type").str() == "text_delta" {
                            ("text", "choices.0.delta.content")
                        } else {
                            ("thinking", "choices.0.delta.reasoning_content")
                        };
                        let value = delta.get(input);
                        if !value.exists() {
                            return None;
                        }
                        set_string(&mut out, output, value.str());
                    }
                    "input_json_delta" => {
                        if let Some(tool) = self.tools.get_mut(&index) {
                            tool.arguments.push_str(delta.get("partial_json").str());
                        }
                        return None;
                    }
                    _ => return None,
                }
            }
            "content_block_stop" => {
                let mut tool = self.tools.remove(&index)?;
                if tool.arguments.is_empty() {
                    tool.arguments = "{}".into();
                }
                tool.write(&mut out, "choices.0.delta.tool_calls.0", true);
            }
            "message_delta" => {
                let delta = root.get("delta");
                let reason = delta.get("stop_reason");
                if reason.exists() {
                    set_string(&mut out, "choices.0.finish_reason", finish_reason(reason.str()));
                }
                let usage = root.get("usage");
                if usage.exists() {
                    self.usage.merge(&usage);
                    self.usage.write(&mut out);
                }
            }
            "message_stop" => {
                if !self.usage.present || self.trailing_sent {
                    return None;
                }
                self.trailing_sent = true;
                set(&mut out, "choices", "[]");
                self.usage.write(&mut out);
            }
            "error" => {
                let error = root.get("error");
                if !error.exists() {
                    return None;
                }
                out = r#"{"error":{"message":"","type":""}}"#.into();
                set_string(&mut out, "error.message", error.get("message").str());
                set_string(&mut out, "error.type", error.get("type").str());
            }
            _ => return None,
        }
        Some(out)
    }
}

impl StreamTranslator for Stream {
    fn event(&mut self, event: &[u8]) -> Result<Vec<Bytes>, Error> {
        Ok(json::data_lines(json::text(event)?)
            .filter_map(|p| self.convert(p))
            .map(|p| json::frame(&p))
            .collect())
    }
    fn finish(&mut self) -> Result<Vec<Bytes>, Error> {
        Ok(vec![])
    }
}

pub fn non_stream(_: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let mut out = RESPONSE.to_owned();
    let mut id = String::new();
    let mut model = String::new();
    let mut created = 0;
    let mut reason = String::new();
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut has_reasoning = false;
    let mut usage = Usage::default();
    let mut tools = BTreeMap::<i64, Tool>::new();
    for payload in json::data_lines(json::text(body)?) {
        let root = gjson::parse(payload);
        let index = root.get("index").i64();
        match root.get("type").str() {
            "message_start" => {
                let message = root.get("message");
                if message.exists() {
                    id = message.get("id").str().into();
                    model = message.get("model").str().into();
                    created = now();
                    usage.merge(&message.get("usage"));
                }
            }
            "content_block_start" => {
                let block = root.get("content_block");
                if block.get("type").str() == "tool_use" {
                    tools.insert(index, Tool::from_block(&block, 0));
                }
            }
            "content_block_delta" => {
                let delta = root.get("delta");
                match delta.get("type").str() {
                    "text_delta" => content.push_str(delta.get("text").str()),
                    "thinking_delta" => {
                        has_reasoning |= delta.get("thinking").exists();
                        reasoning.push_str(delta.get("thinking").str());
                    }
                    "input_json_delta" => {
                        if let Some(tool) = tools.get_mut(&index) {
                            tool.arguments.push_str(delta.get("partial_json").str());
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some(tool) = tools.get_mut(&index)
                    && tool.arguments.is_empty()
                {
                    tool.arguments = "{}".into();
                }
            }
            "message_delta" => {
                let delta = root.get("delta");
                if delta.get("stop_reason").exists() {
                    reason = delta.get("stop_reason").str().into();
                }
                usage.merge(&root.get("usage"));
            }
            _ => {}
        }
    }
    if usage.present {
        usage.write(&mut out);
    }
    set_string(&mut out, "id", &id);
    set(&mut out, "created", &created.to_string());
    set_string(&mut out, "model", &model);
    set_string(&mut out, "choices.0.message.content", &content);
    if has_reasoning {
        set_string(&mut out, "choices.0.message.reasoning_content", &reasoning);
    }
    for (i, (_, tool)) in tools.iter().filter(|(index, _)| **index >= 0).enumerate() {
        tool.write(&mut out, &format!("choices.0.message.tool_calls.{i}"), false);
    }
    let reason = if tools.keys().any(|i| *i >= 0) {
        "tool_calls"
    } else {
        finish_reason(&reason)
    };
    set_string(&mut out, "choices.0.finish_reason", reason);
    Ok(out.into_bytes())
}
