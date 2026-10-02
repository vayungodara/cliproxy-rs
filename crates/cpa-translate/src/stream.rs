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

/// How the client's Go handler writes one translator chunk.
pub fn frame(client: Format, chunk: &[u8]) -> Option<Vec<u8>> {
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
            if chunk.is_empty() {
                return None;
            }
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
        // Claude, Responses and Codex translators emit complete SSE events.
        // ponytail: the Responses handler also runs responsesSSEFramer (output-item
        // bookkeeping for terminal events); that belongs to the route in cpa-server.
        _ => (!chunk.is_empty()).then(|| chunk.to_vec()),
    }
}

struct Framed {
    client: Format,
    upstream: Format,
    inner: Box<dyn GoStream>,
    done: bool,
}

impl Framed {
    fn framed(&self, chunks: Vec<Vec<u8>>) -> Vec<Bytes> {
        chunks
            .iter()
            .filter_map(|c| frame(self.client, c))
            .map(Bytes::from)
            .collect()
    }
}

impl StreamTranslator for Framed {
    fn event(&mut self, event: &[u8]) -> Result<Vec<Bytes>, Error> {
        if self.done {
            return Ok(vec![]);
        }
        let mut chunks = vec![];
        if self.upstream == Format::OpenAI {
            // openai_compat_executor.go: the trimmed `data:` lines of one frame, joined by
            // `\n`, become one `data: <payload>` line; reading stops after `[DONE]`.
            let data: Vec<&[u8]> = scan_lines(event)
                .map(crate::common::trim_space)
                .filter_map(|l| l.strip_prefix(b"data:").map(crate::common::trim_space))
                .collect();
            if data.is_empty() {
                return Ok(vec![]);
            }
            let payload = crate::common::trim_space(&data.join(&b'\n')).to_vec();
            self.done = payload == b"[DONE]";
            let mut line = b"data: ".to_vec();
            line.extend_from_slice(&payload);
            chunks.extend(self.inner.line(&line)?);
        } else {
            for line in scan_lines(event) {
                chunks.extend(self.inner.line(line)?);
            }
        }
        Ok(self.framed(chunks))
    }

    fn finish(&mut self) -> Result<Vec<Bytes>, Error> {
        // openai_compat_executor.go feeds `data: [DONE]` when a non-Responses stream ends
        // cleanly without one.
        if self.upstream == Format::OpenAI && !self.done && self.client != Format::OpenAIResponse {
            self.done = true;
            let chunks = self.inner.line(b"data: [DONE]")?;
            return Ok(self.framed(chunks));
        }
        Ok(vec![])
    }
}

pub(crate) fn framed(client: Format, upstream: Format, inner: Box<dyn GoStream>) -> Box<dyn StreamTranslator> {
    Box::new(Framed {
        client,
        upstream,
        inner,
        done: false,
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
    fn openai_frames_join_data_lines_and_stop_after_done() {
        let mut s = framed(Format::Claude, Format::OpenAI, Box::new(Echo(vec![])));
        let out = s.event(b"event: x\ndata: {\"a\":\n data: 1}\n\n").unwrap();
        assert_eq!(out, [Bytes::from_static(b"data: {\"a\":\n1}")]);
        assert!(s.event(b": comment\n\n").unwrap().is_empty());
        assert_eq!(
            s.event(b"data: [DONE]\n\n").unwrap(),
            [Bytes::from_static(b"data: [DONE]")]
        );
        assert!(s.event(b"data: {}\n\n").unwrap().is_empty());
        assert!(s.finish().unwrap().is_empty());
        // Without [DONE], a clean end synthesizes one, except for Responses clients.
        let mut s = framed(Format::Claude, Format::OpenAI, Box::new(Echo(vec![])));
        assert_eq!(s.finish().unwrap(), [Bytes::from_static(b"data: [DONE]")]);
        let mut s = framed(Format::OpenAIResponse, Format::OpenAI, Box::new(Echo(vec![])));
        assert!(s.finish().unwrap().is_empty());
    }
}
