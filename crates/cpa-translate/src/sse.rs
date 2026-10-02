//! Server-sent event framing.
//!
//! Splits a byte stream into complete events without changing a byte: each frame is
//! everything up to and including the blank line that ends the event. Line endings may
//! be `\n`, `\r\n` or `\r`, mixed. Concatenating all frames reproduces the input.

use bytes::{Bytes, BytesMut};

#[derive(Default)]
pub struct Framer {
    buf: BytesMut,
}

impl Framer {
    /// Feeds a chunk and returns every event it completed.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Bytes> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(end) = self.event_end() {
            out.push(self.buf.split_to(end).freeze());
        }
        out
    }

    /// Returns whatever is left once the stream ends (an unterminated final event).
    pub fn finish(&mut self) -> Option<Bytes> {
        (!self.buf.is_empty()).then(|| self.buf.split().freeze())
    }

    /// Index just past the blank line that ends the first buffered event.
    fn event_end(&self) -> Option<usize> {
        let b = &self.buf[..];
        let (mut i, mut line_start) = (0, 0);
        while i < b.len() {
            let eol = match b[i] {
                b'\n' => 1,
                // A trailing `\r` may be the first half of `\r\n`; wait for more input.
                b'\r' if i + 1 == b.len() => return None,
                b'\r' if b[i + 1] == b'\n' => 2,
                b'\r' => 1,
                _ => {
                    i += 1;
                    continue;
                }
            };
            let blank = i == line_start;
            i += eol;
            if blank {
                return Some(i);
            }
            line_start = i;
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
            .flat_map(|c| f.push(c))
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
    }
}
