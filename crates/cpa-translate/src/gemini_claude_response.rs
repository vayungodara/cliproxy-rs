//! Gemini generateContent responses -> Claude Messages
//! (internal/translator/gemini/claude/gemini_claude_response.go).
//!
//! The stream converter is a block state machine (none, text, thinking, tool use);
//! events carry three trailing newlines, as Go writes them. A signature on a part
//! that is not a thought travels in a "carrier" thinking block: an empty thinking
//! block holding only that signature.

use crate::{
    Error, ResponseCtx,
    claude_tool_args::{OpenTool, clean_claude_tool_arguments_with_translated, map_claude_tool_name},
    common::{self, restore_sanitized_tool_name, sanitize_claude_tool_id},
    stream::GoStream,
};
use cpa_common::json::{self as gj, Res};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
};

type NameMap = Option<HashMap<Vec<u8>, Vec<u8>>>;

/// toolUseIDCounter, shared by the stream and non-stream converters.
static TOOL_USE_IDS: AtomicU64 = AtomicU64::new(0);

const NONE: u8 = 0;
const TEXT: u8 = 1;
const THINKING: u8 = 2;
const TOOL: u8 = 3;

/// geminiMalformedFunctionCallErrorPayload: the Claude error a `MALFORMED_FUNCTION_CALL`
/// finish without a tool call becomes (Go 7f2f17d7); the executors answer it with 502.
const MALFORMED_ERROR: &[u8] =
    br#"{"type":"error","error":{"type":"api_error","message":"Gemini response finished with MALFORMED_FUNCTION_CALL"}}"#;

/// isGeminiMalformedFunctionCall.
fn malformed(finish: &[u8], saw_tool_call: bool) -> bool {
    finish == b"MALFORMED_FUNCTION_CALL" && !saw_tool_call
}

/// resolveGeminiClaudeStopReason.
fn stop_reason(finish: &[u8], saw_tool_call: bool) -> &'static str {
    if saw_tool_call {
        return "tool_use";
    }
    match finish {
        b"MAX_TOKENS" => "max_tokens",
        b"SAFETY" | b"RECITATION" | b"PROHIBITED_CONTENT" | b"SPII" | b"BLOCKLIST" | b"IMAGE_SAFETY" => "refusal",
        _ => "end_turn",
    }
}

fn next_tool_id(upstream: &[u8]) -> Vec<u8> {
    let n = TOOL_USE_IDS.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    sanitize_claude_tool_id(&[upstream, format!("-{n}").as_bytes()].concat())
}

/// common.AppendSSEEventString with three trailing newlines.
fn push_event(out: &mut Vec<u8>, event: &str, payload: &[u8]) {
    out.extend_from_slice(b"event: ");
    out.extend_from_slice(event.as_bytes());
    out.extend_from_slice(b"\ndata: ");
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\n\n\n");
}

/// The part's non-empty thought signature, or empty.
fn signature(part: &Res<'_>) -> Vec<u8> {
    let mut sig = part.get("thoughtSignature");
    if !sig.exists() {
        sig = part.get("thought_signature");
    }
    if sig.exists() { sig.bytes().into_owned() } else { vec![] }
}

pub fn go_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    Box::new(State {
        tool_names: common::tool_name_map_from_claude_request(ctx.original_request),
        sanitized: common::sanitized_tool_name_map(ctx.original_request),
        original: ctx.original_request.to_vec(),
        translated: ctx.translated_request.to_vec(),
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
    original: Vec<u8>,
    translated: Vec<u8>,
    saw_tool_call: bool,
    final_sent: bool,
    finish: Vec<u8>,
    input_tokens: i64,
    output_tokens: i64,
    cached_tokens: i64,
    tool: OpenTool,
}

impl GoStream for State {
    fn line(&mut self, raw: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let mut out = vec![];
        if raw == b"[DONE]" {
            if malformed(&self.finish, self.saw_tool_call) {
                self.malformed_error(&mut out);
                return Ok(if out.is_empty() { vec![] } else { vec![out] });
            }
            if self.started && !self.has_content {
                self.empty_text_block(&mut out);
            }
            if !self.has_content {
                return Ok(vec![]);
            }
            self.finish_message(&mut out);
            push_event(&mut out, "message_stop", br#"{"type":"message_stop"}"#);
            return Ok(vec![out]);
        }
        if malformed(&self.finish, self.saw_tool_call) && self.final_sent {
            return Ok(vec![]);
        }
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
        let finish = gj::get(raw, "candidates.0.finishReason");
        if finish.exists() && !finish.bytes().is_empty() {
            self.finish = finish.bytes().into_owned();
        }
        let usage = gj::get(raw, "usageMetadata");
        if usage.exists() {
            let cached = usage.get("cachedContentTokenCount").int();
            let prompt = usage.get("promptTokenCount").int();
            let mut output = usage
                .get("candidatesTokenCount")
                .int()
                .wrapping_add(usage.get("thoughtsTokenCount").int());
            let total = usage.get("totalTokenCount").int();
            if output == 0 && total > 0 {
                output = total.wrapping_sub(prompt).max(0);
            }
            self.input_tokens = prompt.wrapping_sub(cached).max(0);
            self.output_tokens = output;
            self.cached_tokens = cached;
        }
        if malformed(&self.finish, self.saw_tool_call) && !self.final_sent {
            self.malformed_error(&mut out);
            return Ok(vec![out]);
        }
        let has_finish = raw.windows(14).any(|w| w == br#""finishReason""#);
        if usage.exists() && has_finish && !self.final_sent {
            if !self.has_content && self.started {
                self.empty_text_block(&mut out);
            }
            if self.has_content {
                self.finish_message(&mut out);
            }
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

    fn start_block(&self, out: &mut Vec<u8>, kind: &str) {
        push_event(
            out,
            "content_block_start",
            format!(
                r#"{{"type":"content_block_start","index":{},"content_block":{{"type":"{kind}","{kind}":""}}}}"#,
                self.index
            )
            .as_bytes(),
        );
    }

    fn stop_block(&self, out: &mut Vec<u8>) {
        push_event(
            out,
            "content_block_stop",
            format!(r#"{{"type":"content_block_stop","index":{}}}"#, self.index).as_bytes(),
        );
    }

    /// Closes the open block, if any, and moves to the next index.
    fn close(&mut self, out: &mut Vec<u8>) {
        if self.block != NONE {
            self.stop_block(out);
            self.index += 1;
            self.block = NONE;
        }
    }

    fn empty_text_block(&mut self, out: &mut Vec<u8>) {
        self.start_block(out, "text");
        self.block = TEXT;
        self.has_content = true;
    }

    /// Closes the open block without advancing and sends the malformed-call error once.
    fn malformed_error(&mut self, out: &mut Vec<u8>) {
        if self.block != NONE {
            self.stop_block(out);
            self.block = NONE;
        }
        if !self.final_sent {
            push_event(out, "error", MALFORMED_ERROR);
            self.final_sent = true;
        }
    }

    /// Closes the open block without advancing and sends message_delta once.
    fn finish_message(&mut self, out: &mut Vec<u8>) {
        if self.block != NONE {
            self.stop_block(out);
            self.block = NONE;
        }
        if self.final_sent {
            return;
        }
        let reason = stop_reason(&self.finish, self.saw_tool_call);
        let mut delta = format!(
            r#"{{"type":"message_delta","delta":{{"stop_reason":"{reason}","stop_sequence":null}},"usage":{{"input_tokens":0,"output_tokens":0}}}}"#
        )
        .into_bytes();
        gj::set_int(&mut delta, "usage.output_tokens", self.output_tokens);
        gj::set_int(&mut delta, "usage.input_tokens", self.input_tokens);
        if self.cached_tokens > 0 {
            gj::set_int(&mut delta, "usage.cache_read_input_tokens", self.cached_tokens);
        }
        push_event(out, "message_delta", &delta);
        self.final_sent = true;
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

    /// appendCarrierThinkingBlock: a closed thinking block holding only `sig`.
    fn carrier(&mut self, sig: &[u8], out: &mut Vec<u8>) {
        if sig.is_empty() {
            return;
        }
        self.close(out);
        self.start_block(out, "thinking");
        push_event(
            out,
            "content_block_delta",
            &self.delta("signature_delta", "signature", sig),
        );
        self.stop_block(out);
        self.index += 1;
        self.has_content = true;
    }

    /// A lone signature continues an open thinking block, or else is carried.
    fn lone_signature(&mut self, sig: &[u8], out: &mut Vec<u8>) {
        if self.block == THINKING {
            self.signature_delta(sig, out);
        } else {
            self.carrier(sig, out);
        }
    }

    fn tool_input(&mut self, args: &[u8], out: &mut Vec<u8>) {
        let partial = self.tool.input(args, &self.original, &self.translated);
        push_event(
            out,
            "content_block_delta",
            &self.delta("input_json_delta", "partial_json", &partial),
        );
    }

    fn part(&mut self, part: &Res<'_>, out: &mut Vec<u8>) {
        let text = part.get("text");
        let call = part.get("functionCall");
        let sig = signature(part);
        let has_sig = !sig.is_empty();
        let text_value = text.bytes();
        let text_empty = !text.exists() || text_value.is_empty();

        if has_sig && text_empty && !call.exists() {
            self.lone_signature(&sig, out);
            return;
        }

        if part.get("thought").bool() {
            if has_sig && text_empty {
                self.lone_signature(&sig, out);
                return;
            }
            if self.block != THINKING {
                self.close(out);
                self.start_block(out, "thinking");
                self.block = THINKING;
            }
            push_event(
                out,
                "content_block_delta",
                &self.delta("thinking_delta", "thinking", &text_value),
            );
            self.has_content = true;
            self.signature_delta(&sig, out);
            return;
        }

        if call.exists() {
            self.saw_tool_call = true;
            let upstream = restore_sanitized_tool_name(self.sanitized.as_ref(), &call.get("name").bytes());
            let client = map_claude_tool_name(self.tool_names.as_ref(), &self.original, &upstream);
            let args = call.get("args");
            if self.block == TOOL && upstream.is_empty() {
                if args.exists() {
                    self.tool_input(&args.raw, out);
                }
                self.carrier(&sig, out);
                return;
            }
            self.carrier(&sig, out);
            self.close(out);
            self.tool.open(&client, &self.original);
            let mut start = format!(
                r#"{{"type":"content_block_start","index":{},"content_block":{{"type":"tool_use","id":"","name":"","input":{{}}}}}}"#,
                self.index
            )
            .into_bytes();
            gj::set_str(&mut start, "content_block.id", next_tool_id(&upstream));
            gj::set_str(&mut start, "content_block.name", &client);
            push_event(out, "content_block_start", &start);
            if args.exists() {
                self.tool_input(&args.raw, out);
            }
            self.block = TOOL;
            self.has_content = true;
            return;
        }

        if text.exists() {
            if has_sig && text_value.is_empty() {
                self.lone_signature(&sig, out);
                return;
            }
            self.carrier(&sig, out);
            if self.block != TEXT {
                self.close(out);
                self.start_block(out, "text");
                self.block = TEXT;
            }
            push_event(
                out,
                "content_block_delta",
                &self.delta("text_delta", "text", &text_value),
            );
            self.has_content = true;
            return;
        }

        self.carrier(&sig, out);
    }
}

/// ConvertGeminiResponseToClaudeNonStream.
pub fn non_stream(ctx: &ResponseCtx<'_>, raw: &[u8]) -> Result<Vec<u8>, Error> {
    let root = gj::parse(raw);
    let original = ctx.original_request;
    let tool_names = common::tool_name_map_from_claude_request(original);
    let sanitized = common::sanitized_tool_name_map(original);
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
    let carrier = |blocks: &mut Vec<Vec<u8>>, sig: &[u8]| {
        if !sig.is_empty() {
            let mut block = br#"{"type":"thinking","thinking":"","signature":""}"#.to_vec();
            gj::set_str(&mut block, "signature", sig);
            blocks.push(block);
        }
    };
    let parts = root.get("candidates.0.content.parts");
    if parts.is_array() {
        for part in parts.array() {
            let sig = signature(&part);
            let part_text = part.get("text");
            let value = part_text.bytes();
            let text_empty = !part_text.exists() || value.is_empty();
            let call = part.get("functionCall");

            if part.get("thought").bool() {
                flush_text(&mut blocks, &mut text);
                if !sig.is_empty() {
                    thinking_sig = sig;
                }
                thinking.extend_from_slice(&value);
                continue;
            }
            if text_empty && !call.exists() {
                if !thinking.is_empty() && !sig.is_empty() {
                    thinking_sig = sig;
                } else if !sig.is_empty() {
                    flush_thinking(&mut blocks, &mut thinking, &mut thinking_sig);
                    flush_text(&mut blocks, &mut text);
                    carrier(&mut blocks, &sig);
                }
                continue;
            }
            flush_thinking(&mut blocks, &mut thinking, &mut thinking_sig);
            if call.exists() {
                flush_text(&mut blocks, &mut text);
                carrier(&mut blocks, &sig);
                has_call = true;
                let upstream = restore_sanitized_tool_name(sanitized.as_ref(), &call.get("name").bytes());
                let client = map_claude_tool_name(tool_names.as_ref(), original, &upstream);
                let mut block = br#"{"type":"tool_use","id":"","name":"","input":{}}"#.to_vec();
                gj::set_str(&mut block, "id", next_tool_id(&upstream));
                gj::set_str(&mut block, "name", &client);
                let args = call.get("args");
                let input = if args.exists() && gj::valid(&args.raw) && args.is_object() {
                    clean_claude_tool_arguments_with_translated(&args.raw, &client, original, ctx.translated_request)
                } else {
                    b"{}".to_vec()
                };
                gj::set_raw(&mut block, "input", &input);
                blocks.push(block);
                continue;
            }
            if !sig.is_empty() {
                flush_text(&mut blocks, &mut text);
                carrier(&mut blocks, &sig);
            }
            text.extend_from_slice(&value);
        }
    }
    flush_thinking(&mut blocks, &mut thinking, &mut thinking_sig);
    flush_text(&mut blocks, &mut text);
    if !blocks.is_empty() {
        gj::set_raw(&mut out, "content", gj::join(&blocks));
    }
    let finish = root.get("candidates.0.finishReason").bytes();
    if malformed(&finish, has_call) {
        return Ok(MALFORMED_ERROR.to_vec());
    }
    gj::set_str(&mut out, "stop_reason", stop_reason(&finish, has_call));
    if input == 0 && output == 0 && !root.get("usageMetadata").exists() {
        gj::delete(&mut out, "usage");
    }
    Ok(out)
}
