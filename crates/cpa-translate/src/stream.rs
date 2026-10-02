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
            let trimmed = chunk.trim_ascii();
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

/// responsesSSENeedsLineBreak: a field line appended to an unterminated line.
fn needs_line_break(pending: &[u8], chunk: &[u8]) -> bool {
    if pending.is_empty() || chunk.is_empty() || pending.ends_with(b"\n") || pending.ends_with(b"\r") {
        return false;
    }
    if chunk[0] == b'\n' || chunk[0] == b'\r' {
        return false;
    }
    let trimmed = chunk.trim_ascii_start();
    [&b"data:"[..], b"event:", b"id:", b"retry:", b":"]
        .iter()
        .any(|p| trimmed.starts_with(p))
}

/// The Responses route writes translator chunks through responsesSSEFramer: line chunks
/// are joined into frames and every frame ends with a blank line (writeResponsesSSEChunk).
// ponytail: the route's private-event filtering, completed-output repair and terminal
// tracking (responsesSSEFramer.repairFrame) stay with the Responses route in cpa-server;
// a frame without data is closed at the end of its upstream event instead of being held.
fn join_responses_frames(chunks: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut pending: Vec<u8> = vec![];
    for chunk in chunks.iter().filter(|c| !c.is_empty()) {
        if needs_line_break(&pending, chunk) {
            pending.push(b'\n');
        }
        pending.extend_from_slice(chunk);
    }
    let mut frames = vec![];
    let mut rest = &pending[..];
    loop {
        let lf = rest.windows(2).position(|w| w == b"\n\n").map(|i| i + 2);
        let crlf = rest.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4);
        let end = match (lf, crlf) {
            (Some(a), Some(b)) => {
                if a - 2 < b - 4 {
                    a
                } else {
                    b
                }
            }
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => break,
        };
        frames.push(rest[..end].to_vec());
        rest = &rest[end..];
    }
    if !rest.trim_ascii().is_empty() {
        let mut frame = rest.to_vec();
        frame.extend_from_slice(if rest.ends_with(b"\r\n") {
            b"\r\n"
        } else if rest.ends_with(b"\n") {
            b"\n"
        } else {
            b"\n\n"
        });
        frames.push(frame);
    }
    frames
}

struct Framed {
    client: Format,
    inner: Box<dyn GoStream>,
}

impl Framed {
    fn framed(&self, chunks: Vec<Vec<u8>>) -> Vec<Bytes> {
        if responses_client(self.client) {
            return join_responses_frames(chunks).into_iter().map(Bytes::from).collect();
        }
        chunks
            .iter()
            .filter_map(|c| frame(self.client, c))
            .map(Bytes::from)
            .collect()
    }
}

impl StreamTranslator for Framed {
    fn event(&mut self, event: &[u8]) -> Result<Vec<Bytes>, Error> {
        let mut chunks = vec![];
        for line in scan_lines(event) {
            chunks.extend(self.inner.line(line)?);
        }
        Ok(self.framed(chunks))
    }

    fn finish(&mut self) -> Result<Vec<Bytes>, Error> {
        Ok(vec![])
    }
}

/// Wraps a Go-shaped translator. Executor-side line handling stays with the executor:
/// Go's OpenAI-compatible executor joins a frame's `data:` lines into one line and feeds
/// `data: [DONE]` when a non-Responses stream ends without one; the Gemini executors feed
/// a final `[DONE]`. Executors that do so pass those lines as events.
pub(crate) fn framed(client: Format, _upstream: Format, inner: Box<dyn GoStream>) -> Box<dyn StreamTranslator> {
    Box::new(Framed { client, inner })
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
    fn responses_clients_get_whole_frames() {
        let chunks = |v: &[&[u8]]| v.iter().map(|c| c.to_vec()).collect::<Vec<_>>();
        assert_eq!(
            join_responses_frames(chunks(&[b"event: a", b"data: {}", b""])),
            [b"event: a\ndata: {}\n\n".to_vec()]
        );
        assert_eq!(
            join_responses_frames(chunks(&[b"event: a\ndata: 1\n\nevent: b\ndata: 2\n\n"])),
            [b"event: a\ndata: 1\n\n".to_vec(), b"event: b\ndata: 2\n\n".to_vec()]
        );
        assert_eq!(join_responses_frames(chunks(&[b"", b"  "])), Vec::<Vec<u8>>::new());
        assert_eq!(
            join_responses_frames(chunks(&[b"data: x\r\n"])),
            [b"data: x\r\n\r\n".to_vec()]
        );
    }

    #[test]
    fn events_become_scanner_lines_and_chunks_get_client_framing() {
        let mut s = framed(Format::OpenAI, Format::OpenAI, Box::new(Echo(vec![])));
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
        let mut s = framed(Format::Claude, Format::OpenAI, Box::new(Echo(vec![])));
        assert_eq!(
            s.event(b"event: e\ndata: 1\n\n").unwrap(),
            [Bytes::from_static(b"event: e"), Bytes::from_static(b"data: 1")]
        );
    }
}
