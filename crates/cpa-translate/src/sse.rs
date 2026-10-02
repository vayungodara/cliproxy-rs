//! Server-sent event framing.
//!
//! Splits a byte stream into complete events without changing a byte: each frame is
//! everything up to and including the blank line that ends the event. Line endings may
//! be `\n`, `\r\n` or `\r`, mixed. Concatenating all frames reproduces the input.
//! Provider-specific normalization (line endings, stopping at `message_stop`) belongs in
//! the executor, not here.

use bytes::{Bytes, BytesMut};

/// Largest single event accepted by default. Claude events are far smaller; this only
/// stops a broken or hostile upstream from growing the buffer without bound.
pub const DEFAULT_MAX_EVENT: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventTooLarge {
    pub limit: usize,
}

impl std::fmt::Display for EventTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SSE event exceeds {} bytes", self.limit)
    }
}

impl std::error::Error for EventTooLarge {}

pub struct Framer {
    buf: BytesMut,
    /// Where scanning resumes, so each byte is examined once.
    scan: usize,
    /// Start of the line containing `scan`.
    line_start: usize,
    max_event: usize,
}

impl Default for Framer {
    fn default() -> Self {
        Self::with_limit(DEFAULT_MAX_EVENT)
    }
}

impl Framer {
    pub fn with_limit(max_event: usize) -> Self {
        Self {
            buf: BytesMut::new(),
            scan: 0,
            line_start: 0,
            max_event,
        }
    }

    /// Feeds a chunk and returns every event it completed. After an error the framer
    /// must not be used again.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Bytes>, EventTooLarge> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(end) = self.next_event_end() {
            out.push(self.buf.split_to(end).freeze());
            self.scan = 0;
            self.line_start = 0;
        }
        if self.buf.len() > self.max_event {
            return Err(EventTooLarge { limit: self.max_event });
        }
        Ok(out)
    }

    /// Returns whatever is left once the stream ends (an unterminated final event).
    pub fn finish(&mut self) -> Option<Bytes> {
        (!self.buf.is_empty()).then(|| self.buf.split().freeze())
    }

    /// Index just past the blank line that ends the first buffered event.
    fn next_event_end(&mut self) -> Option<usize> {
        let b = &self.buf[..];
        while self.scan < b.len() {
            let i = self.scan;
            let eol = match b[i] {
                b'\n' => 1,
                // A trailing `\r` may be the first half of `\r\n`; wait for more input.
                b'\r' if i + 1 == b.len() => return None,
                b'\r' if b[i + 1] == b'\n' => 2,
                b'\r' => 1,
                _ => {
                    self.scan += 1;
                    continue;
                }
            };
            let blank = i == self.line_start;
            self.scan += eol;
            self.line_start = self.scan;
            if blank {
                return Some(self.scan);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(chunks: &[&[u8]]) -> Vec<Vec<u8>> {
        let mut f = Framer::default();
        let mut out: Vec<Vec<u8>> = chunks
            .iter()
            .flat_map(|c| f.push(c).unwrap())
            .map(|b| b.to_vec())
            .collect();
        out.extend(f.finish().map(|b| b.to_vec()));
        out
    }

    #[test]
    fn splits_on_blank_lines_byte_for_byte() {
        let input = b"event: a\ndata: 1\n\nevent: b\r\ndata: 2\r\n\r\ndata: 3\r\rtail";
        let got = frames(&[input]);
        assert_eq!(
            got,
            [
                &b"event: a\ndata: 1\n\n"[..],
                b"event: b\r\ndata: 2\r\n\r\n",
                b"data: 3\r\r",
                b"tail"
            ]
        );
        assert_eq!(got.concat(), input);
    }

    #[test]
    fn handles_any_chunk_boundary() {
        let input: &[u8] = b"data: x\r\n\r\ndata: y\n\n";
        for cut in 0..=input.len() {
            let (a, b) = input.split_at(cut);
            assert_eq!(
                frames(&[a, b]),
                [&b"data: x\r\n\r\n"[..], b"data: y\n\n"],
                "cut at {cut}"
            );
        }
        // Byte-at-a-time also exercises the resumable scan.
        let singles: Vec<&[u8]> = input.chunks(1).collect();
        assert_eq!(frames(&singles), [&b"data: x\r\n\r\n"[..], b"data: y\n\n"]);
    }

    #[test]
    fn oversized_event_is_an_error_not_unbounded_growth() {
        let mut f = Framer::with_limit(8);
        assert_eq!(
            f.push(b"data: 1\n\n").unwrap().len(),
            1,
            "a 9-byte event completes before the check"
        );
        assert_eq!(f.push(b"data: 12345").unwrap_err(), EventTooLarge { limit: 8 });
    }
}
