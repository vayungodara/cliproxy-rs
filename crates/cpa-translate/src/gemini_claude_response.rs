//! Gemini generateContent responses -> Claude Messages
//! (internal/translator/gemini/claude/gemini_claude_response.go).
//!
//! The stream converter is a block state machine (none, text, thinking, tool use);
//! events carry three trailing newlines, as Go writes them.

use crate::{
    Error, ResponseCtx,
    common::{self, map_tool_name, restore_sanitized_tool_name, sanitize_claude_tool_id},
    stream::GoStream,
};
use cpa_common::json::{self as gj, Res};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
};

type NameMap = Option<HashMap<Vec<u8>, Vec<u8>>>;

/// toolUseIDCounter.
static TOOL_USE_IDS: AtomicU64 = AtomicU64::new(0);

const NONE: u8 = 0;
const TEXT: u8 = 1;
const THINKING: u8 = 2;
const TOOL: u8 = 3;

/// common.AppendSSEEventString with three trailing newlines.
fn push_event(out: &mut Vec<u8>, event: &str, payload: &[u8]) {
    out.extend_from_slice(b"event: ");
    out.extend_from_slice(event.as_bytes());
    out.extend_from_slice(b"\ndata: ");
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\n\n\n");
}

fn signature<'a>(part: &Res<'a>) -> Res<'a> {
    let sig = part.get("thoughtSignature");
    if sig.exists() {
        sig
    } else {
        part.get("thought_signature")
    }
}

pub fn go_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    Box::new(State {
        tool_names: common::tool_name_map_from_claude_request(ctx.original_request),
        sanitized: common::sanitized_tool_name_map(ctx.original_request),
        ..State::default()
    })
}

#[derive(Default)]
struct State {
    started: bool,
    block: u8,
    index: i64,
    has_content: bool,
    tool_names: NameMap,
    sanitized: NameMap,
    saw_tool_call: bool,
    final_sent: bool,
}

impl GoStream for State {
    fn line(&mut self, raw: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        if raw == b"[DONE]" {
            if !self.has_content {
                return Ok(vec![]);
            }
            let mut out = vec![];
            push_event(&mut out, "message_stop", br#"{"type":"message_stop"}"#);
            return Ok(vec![out]);
        }
        let mut out = vec![];
        if !self.started {
            let mut start = br#"{"type":"message_start","message":{"id":"msg_1nZdL29xx5MUA1yADyHTEsnR8uuvGzszyY","type":"message","role":"assistant","content":[],"model":"claude-3-5-sonnet-20241022","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}}"#.to_vec();
            let version = gj::get(raw, "modelVersion");
            if version.exists() {
                gj::set_str(&mut start, "message.model", version.bytes());
            }
            let id = gj::get(raw, "responseId");
            if id.exists() {
                gj::set_str(&mut start, "message.id", id.bytes());
            }
            push_event(&mut out, "message_start", &start);
            self.started = true;
        }
        let parts = gj::get(raw, "candidates.0.content.parts");
        if parts.is_array() {
            for part in parts.array() {
                self.part(&part, &mut out);
            }
        }
        let usage = gj::get(raw, "usageMetadata");
        let has_finish = raw.windows(14).any(|w| w == br#""finishReason""#);
        if usage.exists() && has_finish && !self.final_sent && self.has_content {
            if self.block != NONE {
                self.stop_block(&mut out);
                self.block = NONE;
            }
            let reason = if self.saw_tool_call {
                "tool_use"
            } else if gj::get(raw, "candidates.0.finishReason").bytes().as_ref() == b"MAX_TOKENS" {
                "max_tokens"
            } else {
                "end_turn"
            };
            let mut delta = format!(
                r#"{{"type":"message_delta","delta":{{"stop_reason":"{reason}","stop_sequence":null}},"usage":{{"input_tokens":0,"output_tokens":0}}}}"#
            )
            .into_bytes();
            let cached = usage.get("cachedContentTokenCount").int();
            let output = usage
                .get("candidatesTokenCount")
                .int()
                .wrapping_add(usage.get("thoughtsTokenCount").int());
            gj::set_int(&mut delta, "usage.output_tokens", output);
            gj::set_int(
                &mut delta,
                "usage.input_tokens",
                usage.get("promptTokenCount").int().wrapping_sub(cached).max(0),
            );
            if cached > 0 {
                gj::set_int(&mut delta, "usage.cache_read_input_tokens", cached);
            }
            push_event(&mut out, "message_delta", &delta);
            self.final_sent = true;
        }
        Ok(vec![out])
    }
}

impl State {
    fn delta(&self, kind: &str, field: &str, value: &[u8]) -> Vec<u8> {
        let mut data = format!(
            r#"{{"type":"content_block_delta","index":{},"delta":{{"type":"{kind}","{field}":""}}}}"#,
            self.index
        )
        .into_bytes();
        gj::set_str(&mut data, &format!("delta.{field}"), value);
        data
    }

    fn stop_block(&self, out: &mut Vec<u8>) {
        push_event(
            out,
            "content_block_stop",
            format!(r#"{{"type":"content_block_stop","index":{}}}"#, self.index).as_bytes(),
        );
    }

    /// Closes the open block (if any) and moves to the next index.
    fn close(&mut self, out: &mut Vec<u8>) {
        if self.block != NONE {
            self.stop_block(out);
            self.index += 1;
        }
    }

    fn signature_delta(&mut self, sig: &[u8], out: &mut Vec<u8>) {
        if sig.is_empty() || self.block != THINKING {
            return;
        }
        push_event(
            out,
            "content_block_delta",
            &self.delta("signature_delta", "signature", sig),
        );
        self.has_content = true;
    }

    fn part(&mut self, part: &Res<'_>, out: &mut Vec<u8>) {
        let text = part.get("text");
        let call = part.get("functionCall");
        let sig = signature(part);
        let sig_text = sig.bytes().into_owned();
        let has_sig = sig.exists() && !sig_text.is_empty();
        if has_sig && !text.exists() && !call.exists() {
            self.signature_delta(&sig_text, out);
            return;
        }
        if text.exists() {
            let text = text.bytes();
            let (kind, field, block) = if part.get("thought").bool() || has_sig {
                if has_sig && text.is_empty() {
                    self.signature_delta(&sig_text, out);
                    return;
                }
                ("thinking_delta", "thinking", THINKING)
            } else {
                ("text_delta", "text", TEXT)
            };
            if self.block != block {
                self.close(out);
                let start_kind = if block == THINKING { "thinking" } else { "text" };
                push_event(
                    out,
                    "content_block_start",
                    format!(
                        r#"{{"type":"content_block_start","index":{},"content_block":{{"type":"{start_kind}","{start_kind}":""}}}}"#,
                        self.index
                    )
                    .as_bytes(),
                );
                self.block = block;
            }
            push_event(out, "content_block_delta", &self.delta(kind, field, &text));
            self.has_content = true;
            if block == THINKING {
                self.signature_delta(&sig_text, out);
            }
        } else if call.exists() {
            self.saw_tool_call = true;
            let upstream = restore_sanitized_tool_name(self.sanitized.as_ref(), &call.get("name").bytes());
            let client = map_tool_name(self.tool_names.as_ref(), &upstream);
            let args = call.get("args");
            if self.block == TOOL && upstream.is_empty() {
                if args.exists() {
                    push_event(
                        out,
                        "content_block_delta",
                        &self.delta("input_json_delta", "partial_json", &args.raw),
                    );
                }
                return;
            }
            if self.block == TOOL {
                self.close(out);
                self.block = NONE;
            }
            self.close(out);
            let n = TOOL_USE_IDS.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            let mut start = format!(
                r#"{{"type":"content_block_start","index":{},"content_block":{{"type":"tool_use","id":"","name":"","input":{{}}}}}}"#,
                self.index
            )
            .into_bytes();
            let id = sanitize_claude_tool_id(&[&upstream[..], format!("-{n}").as_bytes()].concat());
            gj::set_str(&mut start, "content_block.id", id);
            gj::set_str(&mut start, "content_block.name", client);
            push_event(out, "content_block_start", &start);
            if args.exists() {
                push_event(
                    out,
                    "content_block_delta",
                    &self.delta("input_json_delta", "partial_json", &args.raw),
                );
            }
            self.block = TOOL;
            self.has_content = true;
        }
    }
}

/// ConvertGeminiResponseToClaudeNonStream.
pub fn non_stream(ctx: &ResponseCtx<'_>, raw: &[u8]) -> Result<Vec<u8>, Error> {
    let root = gj::parse(raw);
    let tool_names = common::tool_name_map_from_claude_request(ctx.original_request);
    let sanitized = common::sanitized_tool_name_map(ctx.original_request);
    let mut out = br#"{"id":"","type":"message","role":"assistant","model":"","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}"#.to_vec();
    gj::set_str(&mut out, "id", root.get("responseId").bytes());
    gj::set_str(&mut out, "model", root.get("modelVersion").bytes());
    let cached = root.get("usageMetadata.cachedContentTokenCount").int();
    let input = root
        .get("usageMetadata.promptTokenCount")
        .int()
        .wrapping_sub(cached)
        .max(0);
    let output = root
        .get("usageMetadata.candidatesTokenCount")
        .int()
        .wrapping_add(root.get("usageMetadata.thoughtsTokenCount").int());
    gj::set_int(&mut out, "usage.input_tokens", input);
    gj::set_int(&mut out, "usage.output_tokens", output);
    if cached > 0 {
        gj::set_int(&mut out, "usage.cache_read_input_tokens", cached);
    }

    let mut blocks: Vec<Vec<u8>> = vec![];
    let mut text: Vec<u8> = vec![];
    let mut thinking: Vec<u8> = vec![];
    let mut thinking_sig: Vec<u8> = vec![];
    let mut tool_ids = 0;
    let mut has_call = false;
    let flush_text = |blocks: &mut Vec<Vec<u8>>, text: &mut Vec<u8>| {
        if !text.is_empty() {
            let mut block = br#"{"type":"text","text":""}"#.to_vec();
            gj::set_str(&mut block, "text", &*text);
            blocks.push(block);
            text.clear();
        }
    };
    let flush_thinking = |blocks: &mut Vec<Vec<u8>>, thinking: &mut Vec<u8>, sig: &mut Vec<u8>| {
        if thinking.is_empty() && sig.is_empty() {
            return;
        }
        let mut block = br#"{"type":"thinking","thinking":""}"#.to_vec();
        gj::set_str(&mut block, "thinking", &*thinking);
        if !sig.is_empty() {
            gj::set_str(&mut block, "signature", &*sig);
        }
        blocks.push(block);
        thinking.clear();
        sig.clear();
    };
    let parts = root.get("candidates.0.content.parts");
    if parts.is_array() {
        for part in parts.array() {
            let sig = signature(&part);
            let has_sig = sig.exists() && !sig.bytes().is_empty();
            if has_sig {
                thinking_sig = sig.bytes().into_owned();
            }
            let part_text = part.get("text");
            let call = part.get("functionCall");
            let value = part_text.bytes();
            if has_sig && value.is_empty() && !call.exists() {
                continue;
            }
            if part_text.exists() && !value.is_empty() {
                if part.get("thought").bool() || has_sig {
                    flush_text(&mut blocks, &mut text);
                    thinking.extend_from_slice(&value);
                } else {
                    flush_thinking(&mut blocks, &mut thinking, &mut thinking_sig);
                    text.extend_from_slice(&value);
                }
                continue;
            }
            if call.exists() {
                flush_thinking(&mut blocks, &mut thinking, &mut thinking_sig);
                flush_text(&mut blocks, &mut text);
                has_call = true;
                let upstream = restore_sanitized_tool_name(sanitized.as_ref(), &call.get("name").bytes());
                let client = map_tool_name(tool_names.as_ref(), &upstream);
                tool_ids += 1;
                let mut block = br#"{"type":"tool_use","id":"","name":"","input":{}}"#.to_vec();
                let id = sanitize_claude_tool_id(&[&upstream[..], format!("-{tool_ids}").as_bytes()].concat());
                gj::set_str(&mut block, "id", id);
                gj::set_str(&mut block, "name", client);
                let args = call.get("args");
                let input: &[u8] = if args.exists() && gj::valid(&args.raw) && args.is_object() {
                    &args.raw
                } else {
                    b"{}"
                };
                gj::set_raw(&mut block, "input", input);
                blocks.push(block);
            }
        }
    }
    flush_thinking(&mut blocks, &mut thinking, &mut thinking_sig);
    flush_text(&mut blocks, &mut text);
    if !blocks.is_empty() {
        gj::set_raw(&mut out, "content", gj::join(&blocks));
    }
    let stop = if has_call {
        "tool_use"
    } else if root.get("candidates.0.finishReason").bytes().as_ref() == b"MAX_TOKENS" {
        "max_tokens"
    } else {
        "end_turn"
    };
    gj::set_str(&mut out, "stop_reason", stop);
    if input == 0 && output == 0 && !root.get("usageMetadata").exists() {
        gj::delete(&mut out, "usage");
    }
    Ok(out)
}
