//! Adapts Go-shaped stream translators to the [`StreamTranslator`] contract.
//!
//! Go executors read upstream bodies line by line and call the translator per line (or,
//! for OpenAI-compatible upstreams, per SSE frame). Translators return chunks, which the
//! client's route handler then frames. Here every framed upstream event is turned back
//! into the lines the Go executor would have passed, each goes through the Go-shaped
//! [`GoStream`], and each chunk is framed the way that client's Go handler writes it, so
//! the bytes returned are the bytes the client receives.

use crate::{Error, StreamTranslator};
use bytes::Bytes;
use cpa_core::format::Format;

/// One Go `ResponseStreamTransform` with its request-local state.
#[doc(hidden)]
pub trait GoStream: Send {
    /// One upstream line (no terminator) in, zero or more Go chunks out.
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error>;
    /// Go's `ToolInputError() != nil` on the translator state.
    fn tool_input_failed(&self) -> bool {
        false
    }
    /// Go's `FinalizeToolInput()`.
    fn finalize_tool_input(&mut self) -> Vec<Vec<u8>> {
        vec![]
    }
}

/// bufio.ScanLines over one event: split on `\n`, drop one trailing `\r` per line, and
/// no empty token after a final newline.
pub(crate) fn scan_lines(event: &[u8]) -> impl Iterator<Item = &[u8]> {
    let body = event.strip_suffix(b"\n").unwrap_or(event);
    let empty = event.is_empty();
    body.split(|&c| c == b'\n')
        .filter(move |_| !empty)
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
}

/// How the client's Go handler writes one translator chunk. Go's stream layer drops
/// empty chunks before any handler sees them (handlers_stream.go).
pub fn frame(client: Format, chunk: &[u8]) -> Option<Vec<u8>> {
    if chunk.is_empty() {
        return None;
    }
    match client {
        // openai_handlers.go: `data: %s\n\n`. gemini_handlers.go writes the same for the
        // default (no `alt`) streaming mode.
        Format::OpenAI | Format::Gemini => {
            let mut out = Vec::with_capacity(chunk.len() + 8);
            out.extend_from_slice(b"data: ");
            out.extend_from_slice(chunk);
            out.extend_from_slice(b"\n\n");
            Some(out)
        }
        // interactions_handlers.go: empty chunks are dropped, chunks that already carry
        // SSE fields are written as is, and a missing blank line is added.
        Format::Interactions => {
            let trimmed = crate::common::trim_space(chunk);
            let mut out = vec![];
            if !(trimmed.starts_with(b"event:") || trimmed.starts_with(b"data:")) {
                out.extend_from_slice(b"data: ");
            }
            out.extend_from_slice(chunk);
            if !chunk.ends_with(b"\n\n") {
                out.extend_from_slice(b"\n\n");
            }
            Some(out)
        }
        // Claude translators emit complete SSE events; Responses chunks are joined per
        // upstream event by `join_responses_frames`.
        _ => Some(chunk.to_vec()),
    }
}

fn responses_client(client: Format) -> bool {
    matches!(client, Format::OpenAIResponse | Format::Codex)
}

/// Go's responsesSSEFramer.WriteChunk/Flush joining and emission rules
/// (openai_responses_handlers.go): line chunks are joined into frames, a valid data-only
/// frame closes before the next `data:` line, and every frame ends with a blank line.
///
/// For executors that pass upstream Responses lines through untranslated: `write` each
/// chunk, then `flush` at the end of the stream and before writing a terminal error (Go
/// flushes in both places; an incomplete or invalid pending frame is dropped).
// ponytail: repairFrame (private-event filtering, completed-output and error repair,
// terminal tracking) is route logic and stays with the Responses route in cpa-server.
#[derive(Default)]
pub struct ResponsesFramer {
    pending: Vec<u8>,
}

fn has_field(chunk: &[u8], prefix: &[u8]) -> bool {
    chunk
        .split(|&c| c == b'\n')
        .any(|line| crate::common::trim_space(line).starts_with(prefix))
}

/// responsesSSEDataLinesValid: no data, `[DONE]`, or one valid JSON payload.
fn data_lines_valid(chunk: &[u8]) -> bool {
    let mut payload: Option<Vec<u8>> = None;
    for line in chunk.split(|&c| c == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(data) = crate::common::trim_space(line).strip_prefix(b"data:") {
            match &mut payload {
                Some(p) => {
                    p.push(b'\n');
                    p.extend_from_slice(crate::common::trim_space(data));
                }
                None => payload = Some(crate::common::trim_space(data).to_vec()),
            }
        }
    }
    data_payload_valid(payload)
}

fn data_payload_valid(payload: Option<Vec<u8>>) -> bool {
    let Some(payload) = payload else {
        return true;
    };
    let payload = crate::common::trim_space(&payload);
    payload.is_empty() || payload == b"[DONE]" || cpa_common::json::std_valid(payload)
}

fn starts_new_data_frame(pending: &[u8], chunk: &[u8]) -> bool {
    let trimmed = crate::common::trim_space(pending);
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

/// responsesSSENeedsLineBreak: a field line appended to an unterminated line.
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

fn frame_len(chunk: &[u8]) -> usize {
    let lf = chunk.windows(2).position(|w| w == b"\n\n");
    let crlf = chunk.windows(4).position(|w| w == b"\r\n\r\n");
    match (lf, crlf) {
        (None, None) => 0,
        (None, Some(c)) => c + 4,
        (Some(l), None) => l + 2,
        (Some(l), Some(c)) => {
            if l < c {
                l + 2
            } else {
                c + 4
            }
        }
    }
}

fn can_emit_without_delimiter(chunk: &[u8]) -> bool {
    let trimmed = crate::common::trim_space(chunk);
    let needs_more_data = has_field(trimmed, b"event:") && !has_field(trimmed, b"data:");
    !trimmed.is_empty()
        && !needs_more_data
        && has_field(trimmed, b"event:")
        && has_field(trimmed, b"data:")
        && data_lines_valid(trimmed)
}

/// writeResponsesSSEChunk: the frame, then whatever completes its blank line.
fn terminated(frame: &[u8]) -> Option<Vec<u8>> {
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
    Some(out)
}

impl ResponsesFramer {
    /// WriteChunk: the frames this chunk completes, each ending in a blank line.
    pub fn write(&mut self, chunk: &[u8]) -> Vec<Bytes> {
        let mut out = vec![];
        self.write_into(chunk, &mut out);
        out.into_iter().map(Bytes::from).collect()
    }

    /// Flush: the pending frame when it carries valid data; otherwise nothing.
    pub fn flush(&mut self) -> Vec<Bytes> {
        let mut out = vec![];
        self.flush_into(&mut out);
        out.into_iter().map(Bytes::from).collect()
    }

    fn write_into(&mut self, chunk: &[u8], out: &mut Vec<Vec<u8>>) {
        if chunk.is_empty() {
            return;
        }
        if starts_new_data_frame(&self.pending, chunk) {
            out.extend(terminated(&self.pending));
            self.pending.clear();
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
            out.extend(terminated(&self.pending[..n]));
            self.pending.drain(..n);
        }
        if crate::common::trim_space(&self.pending).is_empty() {
            self.pending.clear();
            return;
        }
        if can_emit_without_delimiter(&self.pending) {
            out.extend(terminated(&self.pending));
            self.pending.clear();
        }
    }

    fn flush_into(&mut self, out: &mut Vec<Vec<u8>>) {
        let pending = std::mem::take(&mut self.pending);
        let trimmed = crate::common::trim_space(&pending);
        if !trimmed.is_empty() && has_field(trimmed, b"data:") && data_lines_valid(trimmed) {
            out.extend(terminated(&pending));
        }
    }
}

struct Framed {
    client: Format,
    upstream: Format,
    inner: Box<dyn GoStream>,
    responses: ResponsesFramer,
    /// Interactions upstreams: the lines of the SSE frame being read.
    frame: Vec<u8>,
    /// Nesting of the requests the translator reads.
    request_levels: usize,
    /// Nesting of everything read from upstream so far (state the translator retains).
    seen: crate::Depth,
    /// A stack reservation that failed in `finalize_tool_input`; the stream is broken.
    failed: Option<Error>,
    options: StreamOptions,
}

/// A transform applied to each translated chunk ([`StreamOptions::chunk`]).
pub type ChunkHook = fn(&[u8]) -> Vec<u8>;

/// How a Go executor drives a pair's stream translator beyond reading lines; see
/// [`crate::stream_with`].
#[derive(Clone, Copy, Default)]
pub struct StreamOptions {
    /// Each event is one Go translator call, newlines included, as Go's AI Studio
    /// executor passes every relay chunk (Gemini upstreams only).
    pub whole_events: bool,
    /// Applied to every chunk the translator returns for an event, before the client's
    /// framing (AI Studio's `ensureColonSpacedJSON`). The tool-input finalization is
    /// written without it, as Go's `EndApplyPatchStream` does.
    pub chunk: Option<ChunkHook>,
}

/// geminiInteractionsSSEPayload (gemini_executor.go): a JSON frame as is, else its
/// non-empty, non-`[DONE]` `data:` payloads joined with newlines. Public for the Gemini
/// executor's usage reporting, which observes the same payload per frame.
pub fn interactions_frame_payload(frame: &[u8]) -> Vec<u8> {
    let trimmed = crate::common::trim_space(frame);
    if trimmed.starts_with(b"{") {
        return trimmed.to_vec();
    }
    let mut payload = vec![];
    for line in frame.split(|&c| c == b'\n') {
        let Some(data) = crate::common::trim_space(line).strip_prefix(b"data:") else {
            continue;
        };
        let data = crate::common::trim_space(data);
        if data.is_empty() || data == b"[DONE]" {
            continue;
        }
        if !payload.is_empty() {
            payload.push(b'\n');
        }
        payload.extend_from_slice(data);
    }
    payload
}

/// geminiInteractionsSSEDone: a `[DONE]` frame or data line, or an `event: done` line.
fn interactions_frame_done(frame: &[u8]) -> bool {
    if crate::common::trim_space(frame) == b"[DONE]" {
        return true;
    }
    let mut done_event = false;
    for line in frame.split(|&c| c == b'\n') {
        let line = crate::common::trim_space(line);
        // strings.EqualFold: no letter of "event: done" has a non-ASCII fold partner.
        if line.eq_ignore_ascii_case(b"event: done") {
            done_event = true;
        } else if let Some(data) = line.strip_prefix(b"data:")
            && crate::common::trim_space(data) == b"[DONE]"
        {
            return true;
        }
    }
    done_event
}

/// Go's OpenAI-compatible executor joins a frame's `data:` lines with `\n` and passes the
/// result as one translator line, so for OpenAI upstreams a line that does not start an
/// SSE field continues the previous line.
fn openai_lines(event: &[u8]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = vec![];
    for line in scan_lines(event) {
        let field = [&b"data:"[..], b"event:", b"id:", b"retry:", b":"]
            .iter()
            .any(|p| line.trim_ascii_start().starts_with(p));
        match out.last_mut() {
            Some(previous) if !field && !line.is_empty() && previous.starts_with(b"data:") => {
                previous.push(b'\n');
                previous.extend_from_slice(line);
            }
            _ => out.push(line.to_vec()),
        }
    }
    out
}

impl Framed {
    fn framed(&mut self, chunks: Vec<Vec<u8>>) -> Vec<Bytes> {
        if responses_client(self.client) {
            let mut out = vec![];
            for chunk in &chunks {
                self.responses.write_into(chunk, &mut out);
            }
            return out.into_iter().map(Bytes::from).collect();
        }
        chunks
            .iter()
            .filter_map(|c| frame(self.client, c))
            .map(Bytes::from)
            .collect()
    }
}

impl StreamTranslator for Framed {
    fn flush_frames(&mut self) -> Vec<Bytes> {
        self.responses.flush()
    }

    fn event(&mut self, event: &[u8]) -> Result<Vec<Bytes>, Error> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        self.seen.feed(event);
        crate::deep_stack(self.levels(), || self.translate(event))
    }

    fn finish(&mut self) -> Result<Vec<Bytes>, Error> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        crate::deep_stack(self.levels(), || self.finish_frames())
    }

    fn tool_input_failed(&self) -> bool {
        self.inner.tool_input_failed()
    }

    /// A stack that cannot be reserved emits nothing; every later `event` and `finish`
    /// then returns that error.
    fn finalize_tool_input(&mut self) -> Vec<Bytes> {
        if self.failed.is_some() {
            return vec![];
        }
        let finalized = crate::deep_stack(self.levels(), || {
            let chunks = self.inner.finalize_tool_input();
            Ok(self.framed(chunks))
        });
        finalized.unwrap_or_else(|e| {
            self.failed = Some(e);
            vec![]
        })
    }
}

impl Framed {
    fn levels(&self) -> usize {
        self.request_levels.max(self.seen.levels())
    }

    fn finish_frames(&mut self) -> Result<Vec<Bytes>, Error> {
        let mut out = vec![];
        if self.upstream == Format::Interactions {
            let chunks = self.interactions_frame()?;
            out.extend(self.framed(chunks));
        }
        let mut pending = vec![];
        self.responses.flush_into(&mut pending);
        out.extend(pending.into_iter().map(Bytes::from));
        Ok(out)
    }

    /// The Gemini Interactions executor's emitFrame for the frame read so far: an
    /// Interactions client gets the frame itself (no translator runs), other clients get
    /// the translation of its joined `data:` payload (`[DONE]` for a done frame).
    fn interactions_frame(&mut self) -> Result<Vec<Vec<u8>>, Error> {
        let frame = std::mem::take(&mut self.frame);
        let trimmed = crate::common::trim_space(&frame);
        if trimmed.is_empty() {
            return Ok(vec![]);
        }
        if self.client == Format::Interactions {
            let end = frame
                .iter()
                .rposition(|c| !matches!(c, b'\r' | b'\n'))
                .map_or(0, |i| i + 1);
            return Ok(vec![[&frame[..end], b"\n\n"].concat()]);
        }
        let mut payload = interactions_frame_payload(&frame);
        if payload.is_empty() && interactions_frame_done(&frame) {
            payload = b"[DONE]".to_vec();
        }
        if payload.is_empty() {
            return Ok(vec![]);
        }
        self.inner.line(&payload)
    }

    fn translate(&mut self, event: &[u8]) -> Result<Vec<Bytes>, Error> {
        let mut chunks = vec![];
        if self.upstream == Format::Interactions {
            // The executor reads lines and ends a frame at each blank line.
            for line in scan_lines(event) {
                if crate::common::trim_space(line).is_empty() {
                    chunks.extend(self.interactions_frame()?);
                    continue;
                }
                if !self.frame.is_empty() {
                    self.frame.push(b'\n');
                }
                self.frame.extend_from_slice(line);
            }
        } else if self.upstream == Format::OpenAI {
            for line in openai_lines(event) {
                chunks.extend(self.inner.line(&line)?);
            }
        } else if self.options.whole_events {
            chunks.extend(self.inner.line(event)?);
        } else {
            for line in scan_lines(event) {
                chunks.extend(self.inner.line(line)?);
            }
        }
        if let Some(hook) = self.options.chunk {
            chunks = chunks.iter().map(|c| hook(c)).collect();
        }
        Ok(self.framed(chunks))
    }
}

/// Wraps a Go-shaped translator. Executor-side line handling stays with the executor:
/// Go's OpenAI-compatible executor joins a frame's `data:` lines into one line and feeds
/// `data: [DONE]` when a non-Responses stream ends without one; the Gemini executors feed
/// a final `[DONE]`. Executors that do so pass those lines as events. The one exception
/// is an Interactions upstream: pass its SSE events as read (any split into lines works)
/// and call `finish` at the end; the Gemini Interactions executor's frame handling (the
/// passthrough to Interactions clients, payload joining, done frames) happens here.
pub(crate) fn framed(
    client: Format,
    upstream: Format,
    inner: Box<dyn GoStream>,
    request_levels: usize,
) -> Box<dyn StreamTranslator> {
    framed_with(client, upstream, inner, request_levels, StreamOptions::default())
}

/// [`framed`] with a Go executor's [`StreamOptions`].
pub(crate) fn framed_with(
    client: Format,
    upstream: Format,
    inner: Box<dyn GoStream>,
    request_levels: usize,
    options: StreamOptions,
) -> Box<dyn StreamTranslator> {
    Box::new(Framed {
        client,
        upstream,
        inner,
        responses: ResponsesFramer::default(),
        frame: vec![],
        request_levels,
        seen: crate::Depth::default(),
        failed: None,
        options,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_lines_matches_bufio() {
        let lines: Vec<&[u8]> = scan_lines(b"event: a\r\ndata: 1\n\n").collect();
        assert_eq!(lines, [&b"event: a"[..], b"data: 1", b""]);
        let lines: Vec<&[u8]> = scan_lines(b"data: tail").collect();
        assert_eq!(lines, [&b"data: tail"[..]]);
        assert_eq!(scan_lines(b"").count(), 0);
        assert_eq!(scan_lines(b"\n").collect::<Vec<_>>(), [&b""[..]]);
    }

    struct Echo(Vec<Vec<u8>>);
    impl GoStream for Echo {
        fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
            self.0.push(line.to_vec());
            Ok(vec![line.to_vec()])
        }
    }

    #[test]
    fn a_failed_finalize_breaks_the_stream() {
        let mut s = framed(Format::OpenAI, Format::OpenAI, Box::new(Echo(vec![])), 1000);
        assert_eq!(s.event(b"data: {}\n\n").unwrap().len(), 1, "deep stack available");
        crate::FAIL_SPAWN.with(|f| f.set(true));
        assert!(s.finalize_tool_input().is_empty());
        crate::FAIL_SPAWN.with(|f| f.set(false));
        let err = s.finish().unwrap_err();
        assert!(err.0.contains("stack"), "{err}");
        assert_eq!(s.event(b"data: {}\n\n").unwrap_err(), err);
        assert!(s.finalize_tool_input().is_empty());
    }

    fn responses(chunks: &[&[u8]]) -> Vec<Vec<u8>> {
        let mut framer = ResponsesFramer::default();
        let mut out = vec![];
        for c in chunks {
            out.extend(framer.write(c).into_iter().map(|b| b.to_vec()));
        }
        out.extend(framer.flush().into_iter().map(|b| b.to_vec()));
        out
    }

    /// Chunks and frames recorded from Go's responsesSSEFramer (the Kimi device fixtures
    /// responses-stream-clamp and responses-stream-data-only-frames).
    #[test]
    fn responses_framer_matches_recorded_go_frames() {
        let clamp: [&[u8]; 9] = [
            b"event: response.created\n",
            b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_2\"}}\n",
            b"\n",
            b"event: response.output_text.delta\n",
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n",
            b"\n",
            b"event: response.completed\n",
            b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\"}}\n",
            b"\n",
        ];
        assert_eq!(
            responses(&clamp),
            vec![
                b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_2\"}}\n\n".to_vec(),
                b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n".to_vec(),
                b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\"}}\n\n".to_vec(),
            ]
        );
        // Data-only events stay separate frames; the invalid tail is dropped on flush.
        let data_only: [&[u8]; 6] = [
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"a\"}\n",
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"b\"}\n",
            b"\n",
            b"data: [DONE]\n",
            b": keep\n",
            b"data: {\"type\":\n",
        ];
        assert_eq!(
            responses(&data_only),
            vec![
                b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"a\"}\n\n".to_vec(),
                b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"b\"}\n\n".to_vec(),
                b"data: [DONE]\n: keep\n\n".to_vec(),
            ]
        );
        // Flushing before a terminal error emits a valid pending frame exactly once.
        let mut framer = ResponsesFramer::default();
        assert!(framer.write(b"event: response.created\n").is_empty());
        assert_eq!(framer.write(b"data: {\"a\":1}").len(), 1);
        assert!(framer.write(b"data: {\"b\":2}").is_empty());
        assert_eq!(framer.flush(), vec![Bytes::from_static(b"data: {\"b\":2}\n\n")]);
        assert!(framer.flush().is_empty());
    }

    #[test]
    fn responses_framer_matches_go_joining() {
        assert_eq!(
            responses(&[b"event: a", b"data: {}", b""]),
            [b"event: a\ndata: {}\n\n".to_vec()]
        );
        assert_eq!(
            responses(&[b"event: a\ndata: 1\n\nevent: b\ndata: 2\n\n"]),
            [b"event: a\ndata: 1\n\n".to_vec(), b"event: b\ndata: 2\n\n".to_vec()]
        );
        // A valid data-only frame closes before the next data line.
        assert_eq!(
            responses(&[b"data: {}", b"data: []"]),
            [b"data: {}\n\n".to_vec(), b"data: []\n\n".to_vec()]
        );
        // An event line waits for its data, even across upstream events.
        assert_eq!(
            responses(&[b"event: x", b"", b"data: {}"]),
            [b"event: x\ndata: {}\n\n".to_vec()]
        );
        // An event without data is dropped at flush; blank input emits nothing.
        assert!(responses(&[b"event: lonely"]).is_empty());
        assert!(responses(&[b"", b"  "]).is_empty());
        assert_eq!(responses(&[b"data: x\r\n"]), Vec::<Vec<u8>>::new());
        assert_eq!(responses(&[b"data: [DONE]"]), [b"data: [DONE]\n\n".to_vec()]);
    }

    #[test]
    fn openai_joined_data_payload_stays_one_line() {
        // Go scenario stream_multiline_data: the executor's joined payload keeps its newline.
        let mut s = framed(Format::OpenAI, Format::OpenAI, Box::new(Echo(vec![])), 0);
        let out = s.event(b"data: {\"id\":\"m\",\n\"choices\":[]}\n\n").unwrap();
        assert_eq!(
            out,
            [Bytes::from_static(b"data: data: {\"id\":\"m\",\n\"choices\":[]}\n\n")]
        );
        // Field lines still split; raw single lines (Kimi) pass as they are.
        let out = s.event(b"event: x\ndata: 1\n\n").unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(
            s.event(b": keep-alive").unwrap(),
            [Bytes::from_static(b"data: : keep-alive\n\n")]
        );
    }

    #[test]
    fn events_become_scanner_lines_and_chunks_get_client_framing() {
        let mut s = framed(Format::OpenAI, Format::OpenAI, Box::new(Echo(vec![])), 0);
        let out = s.event(b"event: x\r\ndata: {}\n\n").unwrap();
        assert_eq!(
            out,
            [
                Bytes::from_static(b"data: event: x\n\n"),
                Bytes::from_static(b"data: data: {}\n\n")
            ]
        );
        // A single line (how some executors feed) is one scanner line.
        assert_eq!(
            s.event(b": keep-alive").unwrap(),
            [Bytes::from_static(b"data: : keep-alive\n\n")]
        );
        assert!(s.finish().unwrap().is_empty());
        let mut s = framed(Format::Claude, Format::OpenAI, Box::new(Echo(vec![])), 0);
        assert_eq!(
            s.event(b"event: e\ndata: 1\n\n").unwrap(),
            [Bytes::from_static(b"event: e"), Bytes::from_static(b"data: 1")]
        );
    }

    /// The Gemini Interactions executor (gemini_executor.go executeInteractionsStream):
    /// frames end at blank or blank-looking lines, possibly across events; Interactions
    /// clients get each frame with its line breaks normalized and no translator call.
    #[test]
    fn interactions_upstream_frames_pass_through_to_interactions_clients() {
        let mut s = framed(Format::Interactions, Format::Interactions, Box::new(Echo(vec![])), 0);
        assert_eq!(
            s.event(b"event: interaction.created\r\ndata: {\"a\":1}\r\n\r\n")
                .unwrap(),
            [Bytes::from_static(b"event: interaction.created\ndata: {\"a\":1}\n\n")]
        );
        assert!(s.event(b"data: {\"b\":2}\n").unwrap().is_empty());
        assert_eq!(
            s.event(b" \n{\"c\":3}\n\n").unwrap(),
            [
                Bytes::from_static(b"data: {\"b\":2}\n\n"),
                Bytes::from_static(b"data: {\"c\":3}\n\n")
            ]
        );
        assert!(s.event(b"\n\n").unwrap().is_empty());
        assert!(s.event(b": tail").unwrap().is_empty());
        // The pending frame is emitted at EOF; the handler prefixes non-field chunks.
        assert_eq!(s.finish().unwrap(), [Bytes::from_static(b"data: : tail\n\n")]);
    }

    /// Other clients get the translation of each frame's joined `data:` payload; a frame
    /// whose data lines continue one JSON document translates as that document.
    #[test]
    fn interactions_upstream_frames_translate_their_joined_payload() {
        let ctx = crate::ResponseCtx {
            model: "m",
            original_request: b"",
            translated_request: b"",
        };
        let mut s = (crate::pair(Format::Gemini, Format::Interactions).unwrap().stream)(&ctx);
        let text = |t: &str| {
            Bytes::from(
                format!(
                    r#"data: {{"candidates":[{{"content":{{"parts":[{{"text":"{t}"}}],"role":"model"}},"index":0}}],"modelVersion":"m"}}"#
                ) + "\n\n",
            )
        };
        assert_eq!(
            s.event(b"event: step.delta\ndata: {\"event_type\":\"step.delta\",\"delta\":{\"type\":\"text\",\"text\":\"a\"}}\n\n")
                .unwrap(),
            [text("a")]
        );
        assert_eq!(
            s.event(b"data: {\"event_type\":\"step.delta\",\ndata:  \"delta\":{\"type\":\"text\",\"text\":\"b\"}}\n\n")
                .unwrap(),
            [text("b")]
        );
        assert!(s.event(b"event: done\ndata: [DONE]\n\n").unwrap().is_empty());
        assert!(
            s.event(b"data: {\"event_type\":\"step.delta\",\"delta\":{\"type\":\"text\",\"text\":\"c\"}}")
                .unwrap()
                .is_empty()
        );
        assert_eq!(s.finish().unwrap(), [text("c")]);
    }

    #[test]
    fn interactions_done_frames_match_go() {
        assert!(interactions_frame_done(b" [DONE] "));
        assert!(interactions_frame_done(b"EVENT: Done\n: x"));
        assert!(interactions_frame_done(b"event: x\n data:  [DONE] "));
        assert!(!interactions_frame_done(b"event: done!"));
        assert!(!interactions_frame_done(b"data: [DONE]x"));
        assert_eq!(
            interactions_frame_payload(b"data: [DONE]\ndata:\n data: a \nid: 1"),
            b"a"
        );
        assert_eq!(interactions_frame_payload(b" {\"x\":1}\n"), b"{\"x\":1}");
    }
}
