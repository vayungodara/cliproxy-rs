//! The chunk joining of Go's Responses route (openai_responses_handlers.go
//! responsesSSEFramer.WriteChunk / Flush, without the route's repairFrame), for executors
//! that pass upstream Responses lines through untranslated.
//!
//! ponytail: adapter, owner the translators thread. cpa-translate has the same joiner as a
//! private `stream::ResponsesFramer`; replace this module once it is exported. Checked
//! against Go by the `frames` of tests/device_fixtures/kimi/responses-stream-*.json.

use bytes::Bytes;

use crate::meta_codex::go_trim_space;

#[derive(Default)]
pub(crate) struct Joiner {
    pending: Vec<u8>,
}

impl Joiner {
    /// WriteChunk: completed frames, each ending in a blank line.
    pub(crate) fn write(&mut self, chunk: &[u8]) -> Vec<Bytes> {
        let mut out = Vec::new();
        if chunk.is_empty() {
            return out;
        }
        if starts_new_data_frame(&self.pending, chunk) {
            out.extend(terminated(&std::mem::take(&mut self.pending)));
        }
        if needs_line_break(&self.pending, chunk) {
            self.pending.push(b'\n');
        }
        self.pending.extend_from_slice(chunk);
        loop {
            let n = frame_len(&self.pending);
            if n == 0 {
                break;
            }
            let frame: Vec<u8> = self.pending.drain(..n).collect();
            out.extend(terminated(&frame));
        }
        if go_trim_space(&self.pending).is_empty() {
            self.pending.clear();
            return out;
        }
        if can_emit_without_delimiter(&self.pending) {
            out.extend(terminated(&std::mem::take(&mut self.pending)));
        }
        out
    }

    /// Flush: the pending frame when it carries valid data; otherwise it is dropped. Go
    /// flushes at the end of the stream and before writing a terminal error.
    pub(crate) fn flush(&mut self) -> Vec<Bytes> {
        let pending = std::mem::take(&mut self.pending);
        let trimmed = go_trim_space(&pending);
        if !trimmed.is_empty() && has_field(trimmed, b"data:") && data_lines_valid(trimmed) {
            return terminated(&pending).into_iter().collect();
        }
        Vec::new()
    }
}

/// writeResponsesSSEChunk: the frame plus whatever completes its blank line.
fn terminated(frame: &[u8]) -> Option<Bytes> {
    if frame.is_empty() {
        return None;
    }
    let mut out = frame.to_vec();
    if !(frame.ends_with(b"\n\n") || frame.ends_with(b"\r\n\r\n")) {
        out.extend_from_slice(if frame.ends_with(b"\r\n") {
            b"\r\n"
        } else if frame.ends_with(b"\n") {
            b"\n"
        } else {
            b"\n\n"
        });
    }
    Some(Bytes::from(out))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// responsesSSEFrameLen.
fn frame_len(chunk: &[u8]) -> usize {
    match (find(chunk, b"\n\n"), find(chunk, b"\r\n\r\n")) {
        (None, None) => 0,
        (None, Some(crlf)) => crlf + 4,
        (Some(lf), None) => lf + 2,
        (Some(lf), Some(crlf)) if lf < crlf => lf + 2,
        (Some(_), Some(crlf)) => crlf + 4,
    }
}

/// responsesSSEHasField.
fn has_field(chunk: &[u8], prefix: &[u8]) -> bool {
    chunk
        .split(|b| *b == b'\n')
        .any(|line| go_trim_space(line).starts_with(prefix))
}

/// responsesSSEDataPayload then responsesSSEDataLinesValid.
fn data_lines_valid(chunk: &[u8]) -> bool {
    let mut payload: Option<Vec<u8>> = None;
    for line in chunk.split(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(data) = go_trim_space(line).strip_prefix(b"data:") else {
            continue;
        };
        let data = go_trim_space(data);
        match &mut payload {
            Some(p) => {
                p.push(b'\n');
                p.extend_from_slice(data);
            }
            None => payload = Some(data.to_vec()),
        }
    }
    let Some(payload) = payload else {
        return true;
    };
    let payload = go_trim_space(&payload);
    payload.is_empty() || payload == b"[DONE]" || crate::meta_wire::check_valid(payload).is_ok()
}

/// responsesSSECanEmitWithoutDelimiter.
fn can_emit_without_delimiter(chunk: &[u8]) -> bool {
    let trimmed = go_trim_space(chunk);
    let needs_more_data = has_field(trimmed, b"event:") && !has_field(trimmed, b"data:");
    !trimmed.is_empty()
        && !needs_more_data
        && has_field(trimmed, b"event:")
        && has_field(trimmed, b"data:")
        && data_lines_valid(trimmed)
}

/// responsesSSEStartsNewDataFrame.
fn starts_new_data_frame(pending: &[u8], chunk: &[u8]) -> bool {
    let trimmed = go_trim_space(pending);
    if trimmed.is_empty()
        || has_field(trimmed, b"event:")
        || !has_field(trimmed, b"data:")
        || !data_lines_valid(trimmed)
    {
        return false;
    }
    let start = chunk
        .iter()
        .position(|c| !matches!(c, b' ' | b'\t' | b'\r' | b'\n'))
        .unwrap_or(chunk.len());
    chunk[start..].starts_with(b"data:")
}

/// responsesSSENeedsLineBreak.
fn needs_line_break(pending: &[u8], chunk: &[u8]) -> bool {
    if pending.is_empty() || chunk.is_empty() || pending.ends_with(b"\n") || pending.ends_with(b"\r") {
        return false;
    }
    if chunk[0] == b'\n' || chunk[0] == b'\r' {
        return false;
    }
    let start = chunk
        .iter()
        .position(|c| !matches!(c, b' ' | b'\t'))
        .unwrap_or(chunk.len());
    let trimmed = &chunk[start..];
    !trimmed.is_empty()
        && [&b"data:"[..], b"event:", b"id:", b"retry:", b":"]
            .iter()
            .any(|p| trimmed.starts_with(p))
}
