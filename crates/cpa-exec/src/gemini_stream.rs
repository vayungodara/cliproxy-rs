//! Stream helpers of the Gemini-family executors: Go's usage filtering for Gemini event
//! lines (helps.FilterSSEUsageMetadata, JSONPayload), Interactions SSE frame parsing
//! (gemini_executor.go) and the Claude `message_start` input-token estimate every
//! non-Claude upstream applies for Claude clients (helps/claude_input_tokens.go).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use cpa_common::gostr::trim_space;
use cpa_common::json::{self as gj, Kind};
use cpa_core::format::Format;

/// How long a stop chunk without usage is remembered per trace ID (Go `time.AfterFunc`).
const STOP_MEMORY: Duration = Duration::from_secs(600);

/// Go's process-wide `stopChunkWithoutUsage` map: trace IDs whose stop chunk arrived
/// without usage, so the usage chunk that follows keeps its `usageMetadata`.
fn stop_without_usage() -> &'static Mutex<HashMap<Vec<u8>, Instant>> {
    static MAP: OnceLock<Mutex<HashMap<Vec<u8>, Instant>>> = OnceLock::new();
    MAP.get_or_init(Mutex::default)
}

fn remember_stop(trace: &[u8]) {
    let mut map = stop_without_usage()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = Instant::now();
    map.retain(|_, at| now.duration_since(*at) < STOP_MEMORY);
    map.insert(trace.to_vec(), now);
}

/// Removes `trace` when it is remembered and not expired.
fn forget_stop(trace: &[u8]) -> bool {
    let mut map = stop_without_usage()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match map.remove(trace) {
        Some(at) => Instant::now().duration_since(at) < STOP_MEMORY,
        None => false,
    }
}

fn is_remembered(trace: &[u8]) -> bool {
    let map = stop_without_usage()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    map.get(trace)
        .is_some_and(|at| Instant::now().duration_since(*at) < STOP_MEMORY)
}

fn finish_reason(json: &[u8]) -> gj::Res<'_> {
    let reason = gj::get(json, "candidates.0.finishReason");
    if reason.exists() {
        reason
    } else {
        gj::get(json, "response.candidates.0.finishReason")
    }
}

fn has_usage(json: &[u8]) -> bool {
    !json.is_empty()
        && gj::valid(json)
        && (gj::get(json, "usageMetadata").exists() || gj::get(json, "response.usageMetadata").exists())
}

/// `isStopChunkWithoutUsage`.
fn is_stop_without_usage(json: &[u8]) -> bool {
    if json.is_empty() || !gj::valid(json) {
        return false;
    }
    let reason = finish_reason(json);
    if !reason.exists() || trim_space(&reason.bytes()).is_empty() {
        return false;
    }
    !has_usage(json)
}

/// `StripUsageMetadataFromJSON`: non-terminal chunks keep their usage under
/// `cpaUsageMetadata` (top level and `response.`); terminal chunks are untouched.
pub(crate) fn strip_usage_metadata(raw: &[u8]) -> Option<Vec<u8>> {
    let json = trim_space(raw);
    if json.is_empty() || !gj::valid(json) {
        return None;
    }
    let reason = finish_reason(json);
    if reason.exists() && !trim_space(&reason.bytes()).is_empty() {
        return None;
    }
    if !gj::get(json, "usageMetadata").exists() && !gj::get(json, "response.usageMetadata").exists() {
        return None;
    }
    let mut cleaned = json.to_vec();
    for (from, to) in [
        ("usageMetadata", "cpaUsageMetadata"),
        ("response.usageMetadata", "response.cpaUsageMetadata"),
    ] {
        let usage = gj::get(&cleaned, from);
        if usage.exists() {
            let raw = usage.raw().to_vec();
            gj::set_raw(&mut cleaned, to, &raw);
            gj::delete(&mut cleaned, from);
        }
    }
    Some(cleaned)
}

/// `helps.FilterSSEUsageMetadata` over one scanned line (or a multi-line payload).
pub(crate) fn filter_sse_usage_metadata(payload: &[u8]) -> Vec<u8> {
    if payload.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<Vec<u8>> = payload.split(|b| *b == b'\n').map(<[u8]>::to_vec).collect();
    let mut modified = false;
    let mut found_data = false;
    for line in &mut lines {
        let trimmed = trim_space(line);
        if trimmed.is_empty() || !trimmed.starts_with(b"data:") {
            continue;
        }
        found_data = true;
        let Some(data_index) = find(line, b"data:") else {
            continue;
        };
        let raw = trim_space(&line[data_index + 5..]).to_vec();
        let trace = gj::get(&raw, "traceId").bytes().into_owned();
        if is_stop_without_usage(&raw) && !trace.is_empty() {
            remember_stop(&trace);
            continue;
        }
        if !trace.is_empty() && is_remembered(&trace) && has_usage(&raw) {
            forget_stop(&trace);
            continue;
        }
        let Some(cleaned) = strip_usage_metadata(&raw) else {
            continue;
        };
        let mut rebuilt = line[..data_index].to_vec();
        rebuilt.extend_from_slice(b"data:");
        if !cleaned.is_empty() {
            rebuilt.push(b' ');
            rebuilt.extend_from_slice(&cleaned);
        }
        *line = rebuilt;
        modified = true;
    }
    if !modified {
        if !found_data {
            // Raw JSON without an SSE `data:` prefix.
            return strip_usage_metadata(trim_space(payload)).unwrap_or_else(|| payload.to_vec());
        }
        return payload.to_vec();
    }
    lines.join(&b'\n')
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// `helps.JSONPayload`: the JSON object of a line, without its `data:` prefix; `None`
/// for blank lines, `[DONE]`, `event:` lines and anything that is not an object.
pub(crate) fn json_payload(line: &[u8]) -> Option<&[u8]> {
    let mut trimmed = trim_space(line);
    if trimmed.is_empty() || trimmed == b"[DONE]" || trimmed.starts_with(b"event:") {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix(b"data:") {
        trimmed = trim_space(rest);
    }
    (trimmed.first() == Some(&b'{')).then_some(trimmed)
}

/// `geminiInteractionsSSEPayload`: a bare JSON frame, or its `data:` lines joined by
/// newlines (`[DONE]` and empty data skipped).
pub(crate) fn interactions_sse_payload(frame: &[u8]) -> Option<Vec<u8>> {
    let trimmed = trim_space(frame);
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with(b"{") {
        return Some(trimmed.to_vec());
    }
    let mut payload: Vec<u8> = Vec::new();
    for line in frame.split(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if !trim_space(line).starts_with(b"data:") {
            continue;
        }
        let at = find(line, b"data:").unwrap_or(0);
        let data = trim_space(&line[at + 5..]);
        if data.is_empty() || data == b"[DONE]" {
            continue;
        }
        if !payload.is_empty() {
            payload.push(b'\n');
        }
        payload.extend_from_slice(data);
    }
    (!payload.is_empty()).then_some(payload)
}

/// `geminiInteractionsSSEDone`: a `[DONE]` frame, a `data: [DONE]` line or an
/// `event: done` line.
pub(crate) fn interactions_sse_done(frame: &[u8]) -> bool {
    if trim_space(frame) == b"[DONE]" {
        return true;
    }
    let mut saw_done_event = false;
    for line in frame.split(|b| *b == b'\n') {
        let line = trim_space(line.strip_suffix(b"\r").unwrap_or(line));
        if line.eq_ignore_ascii_case(b"event: done") {
            saw_done_event = true;
            continue;
        }
        if let Some(data) = line.strip_prefix(b"data:")
            && trim_space(data) == b"[DONE]"
        {
            return true;
        }
    }
    saw_done_event
}

/// `ClaudeInputTokenState`: for Claude clients of a non-Claude upstream, the first
/// `message_start` without input tokens gets the O200kBase estimate of the client's
/// original request.
pub(crate) struct ClaudeInputTokens {
    original: Bytes,
    handled: bool,
}

impl ClaudeInputTokens {
    pub(crate) fn new(source: Format, upstream: Format, response: Format, original: Bytes) -> Self {
        let enabled = source == Format::Claude && upstream != Format::Claude && response == Format::Claude;
        Self {
            original,
            handled: !enabled,
        }
    }

    /// `state.apply`: patches the first chunk holding a `message_start` event.
    pub(crate) fn apply(&mut self, chunks: &mut [Bytes]) {
        if self.handled {
            return;
        }
        for chunk in chunks.iter_mut() {
            if let Some(updated) = self.apply_chunk(chunk) {
                self.handled = true;
                if let Some(updated) = updated {
                    *chunk = Bytes::from(updated);
                }
                return;
            }
        }
    }

    /// `None` when the chunk has no `message_start`; `Some(None)` when it has one that
    /// stays unchanged.
    fn apply_chunk(&self, chunk: &[u8]) -> Option<Option<Vec<u8>>> {
        let mut line_start = 0;
        while line_start < chunk.len() {
            let line_end = chunk[line_start..]
                .iter()
                .position(|b| *b == b'\n')
                .map_or(chunk.len(), |i| line_start + i);
            let mut content_end = line_end;
            if content_end > line_start && chunk[content_end - 1] == b'\r' {
                content_end -= 1;
            }
            let line = &chunk[line_start..content_end];
            let left = line
                .iter()
                .position(|b| !matches!(b, b' ' | b'\t'))
                .unwrap_or(line.len());
            if line[left..].starts_with(b"data:") {
                let mut payload_offset = left + 5;
                while payload_offset < line.len() && matches!(line[payload_offset], b' ' | b'\t') {
                    payload_offset += 1;
                }
                let mut payload_end = line.len();
                while payload_end > payload_offset && matches!(line[payload_end - 1], b' ' | b'\t') {
                    payload_end -= 1;
                }
                let payload = &line[payload_offset..payload_end];
                if *gj::get(payload, "type").bytes() == *b"message_start" {
                    let input = gj::get(payload, "message.usage.input_tokens");
                    if input.exists() && input.int() != 0 {
                        return Some(None);
                    }
                    let count = match claude_input_tokens(&self.original) {
                        Some(count) if count != 0 => count,
                        _ => return Some(None),
                    };
                    let Ok(updated) = gj::try_set_raw(payload, "message.usage.input_tokens", count.to_string()) else {
                        return Some(None);
                    };
                    let start = line_start + payload_offset;
                    let stop = line_start + payload_end;
                    let mut out = Vec::with_capacity(chunk.len() + updated.len());
                    out.extend_from_slice(&chunk[..start]);
                    out.extend_from_slice(&updated);
                    out.extend_from_slice(&chunk[stop..]);
                    return Some(Some(out));
                }
            }
            if line_end == chunk.len() {
                break;
            }
            line_start = line_end + 1;
        }
        None
    }
}

/// `CountClaudeInputTokens`: O200kBase tokens of the request's text segments joined by
/// newlines; `None` when the request is not JSON (Go logs and leaves the event as is).
pub(crate) fn claude_input_tokens(payload: &[u8]) -> Option<i64> {
    if trim_space(payload).is_empty() {
        return Some(0);
    }
    if !gj::valid(payload) {
        return None;
    }
    let root = gj::parse(payload);
    let mut segments: Vec<String> = Vec::new();
    let system = root.get("system");
    if system.kind == Kind::String {
        push(&mut segments, &system.str());
    } else if system.is_array() {
        for part in system.array() {
            if part.kind == Kind::String {
                push(&mut segments, &part.str());
            } else if *part.get("type").bytes() == *b"text" {
                push(&mut segments, &part.get("text").str());
            }
        }
    }
    let messages = root.get("messages");
    if messages.is_array() {
        for message in messages.array() {
            push(&mut segments, &message.get("role").str());
            content_segments(&message.get("content"), &mut segments);
        }
    }
    let tools = root.get("tools");
    if tools.is_array() {
        for tool in tools.array() {
            for field in ["type", "name", "description"] {
                push(&mut segments, &tool.get(field).str());
            }
            push_json(&mut segments, &tool.get("input_schema"));
        }
    }
    let choice = root.get("tool_choice");
    if choice.exists() {
        if choice.kind == Kind::String {
            push(&mut segments, &choice.str());
        } else {
            push(&mut segments, &choice.get("type").str());
            push(&mut segments, &choice.get("name").str());
        }
    }
    if segments.is_empty() {
        return Some(0);
    }
    static ENCODER: OnceLock<Option<tiktoken_rs::CoreBPE>> = OnceLock::new();
    let encoder = ENCODER.get_or_init(|| tiktoken_rs::o200k_base().ok()).as_ref()?;
    Some(encoder.encode_ordinary(&segments.join("\n")).len() as i64)
}

fn content_segments(content: &gj::Res<'_>, segments: &mut Vec<String>) {
    if !content.exists() {
        return;
    }
    if content.kind == Kind::String {
        push(segments, &content.str());
        return;
    }
    if content.is_array() {
        for part in content.array() {
            content_segments(&part, segments);
        }
        return;
    }
    if !content.is_object() {
        return;
    }
    let text = |path: &str| content.get(path).str().into_owned();
    match &*content.get("type").bytes() {
        b"text" => push(segments, &text("text")),
        b"thinking" => push(segments, &text("thinking")),
        b"document" => {
            let source = content.get("source");
            if *source.get("type").bytes() == *b"text" {
                push(segments, &text("title"));
                push(segments, &text("context"));
                push(segments, &source.get("data").str());
                push(segments, &source.get("content").str());
            }
        }
        b"tool_use" | b"server_tool_use" | b"mcp_tool_use" => {
            push(segments, &text("id"));
            push(segments, &text("name"));
            push_json(segments, &content.get("input"));
        }
        b"tool_result"
        | b"mcp_tool_result"
        | b"web_search_tool_result"
        | b"web_fetch_tool_result"
        | b"code_execution_tool_result"
        | b"bash_code_execution_tool_result"
        | b"text_editor_code_execution_tool_result" => {
            push(segments, &text("tool_use_id"));
            push(segments, &text("tool_call_id"));
            content_segments(&content.get("content"), segments);
        }
        b"web_search_result" | b"search_result" => {
            let source = content.get("source");
            if source.kind == Kind::String {
                push(segments, &source.str());
            }
            for field in ["title", "url", "page_age"] {
                push(segments, &text(field));
            }
            content_segments(&content.get("content"), segments);
        }
        b"web_fetch_result" => {
            push(segments, &text("url"));
            push(segments, &text("retrieved_at"));
            content_segments(&content.get("content"), segments);
        }
        b"code_execution_result" | b"bash_code_execution_result" | b"text_editor_code_execution_result" => {
            for field in ["stdout", "stderr", "return_code"] {
                push(segments, &text(field));
            }
            content_segments(&content.get("content"), segments);
            content_segments(&content.get("output"), segments);
        }
        b"tool_reference" => push(segments, &text("tool_name")),
        b"image" | b"input_audio" | b"audio" | b"video" | b"redacted_thinking" => {}
        b"" => push_json(segments, content),
        _ => push(segments, &text("text")),
    }
}

/// `appendClaudeTokenString`.
fn push(segments: &mut Vec<String>, value: &str) {
    let trimmed = value.trim_matches(|c: char| c.is_whitespace());
    if !trimmed.is_empty() {
        segments.push(trimmed.to_owned());
    }
}

/// `appendClaudeTokenJSON`: strings as text, other values compacted (raw on failure).
fn push_json(segments: &mut Vec<String>, value: &gj::Res<'_>) {
    if !value.exists() {
        return;
    }
    if value.kind == Kind::String {
        push(segments, &value.str());
        return;
    }
    let raw = trim_space(value.raw());
    if raw.is_empty() {
        return;
    }
    let text = if gj::valid(raw) {
        gj::compact(raw, false)
    } else {
        raw.to_vec()
    };
    push(segments, &String::from_utf8_lossy(&text));
}
