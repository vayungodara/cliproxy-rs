//! permessage-deflate on the upstream Codex socket (RFC 7692), as gorilla/websocket's
//! dialer does it with `EnableCompression`: the client offers
//! `permessage-deflate; server_no_context_takeover; client_no_context_takeover`, reads
//! compressed messages, and never compresses what it writes (Go calls
//! `EnableWriteCompression(false)`).
//!
//! tungstenite has no extension support and rejects frames with RSV1 set, so [`Inflate`]
//! sits between the upgraded connection and tungstenite. When the server accepted the
//! offer, each compressed data message is inflated and handed on as one uncompressed
//! frame; control frames and uncompressed frames pass through unchanged. Writes always
//! pass through.

use std::io::{self, Read};
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::{Buf, BytesMut};
use http::HeaderMap;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// gorilla's offer (`Dialer.EnableCompression`).
pub(super) const OFFER: &str = "permessage-deflate; server_no_context_takeover; client_no_context_takeover";

/// The handshake response's `permessage-deflate` agreement (gorilla `Dial`): the first
/// such extension decides; it must carry both no-context-takeover parameters.
/// `Err` is gorilla's `errInvalidCompression`.
pub(super) fn negotiated(headers: &HeaderMap) -> Result<bool, ()> {
    for extension in parse_extensions(headers) {
        if extension.first().map(|(name, _)| name.as_str()) != Some("permessage-deflate") {
            continue;
        }
        let has = |key: &str| extension.iter().skip(1).any(|(k, _)| k == key);
        if !has("server_no_context_takeover") || !has("client_no_context_takeover") {
            return Err(());
        }
        return Ok(true);
    }
    Ok(false)
}

/// gorilla `parseExtensions`: each extension is its token followed by `key[=value]`
/// parameters; a malformed header value is skipped from that point on.
fn parse_extensions(headers: &HeaderMap) -> Vec<Vec<(String, String)>> {
    fn is_token(c: u8) -> bool {
        c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c)
    }
    fn skip_space(s: &[u8]) -> &[u8] {
        let n = s.iter().take_while(|c| matches!(c, b' ' | b'\t')).count();
        &s[n..]
    }
    fn token(s: &[u8]) -> (String, &[u8]) {
        let n = s.iter().take_while(|c| is_token(**c)).count();
        (String::from_utf8_lossy(&s[..n]).into_owned(), &s[n..])
    }
    /// `nextTokenOrQuoted`: a token, or a quoted string with `\` escapes.
    fn token_or_quoted(s: &[u8]) -> (String, &[u8]) {
        if s.first() != Some(&b'"') {
            return token(s);
        }
        let mut value = Vec::new();
        let mut i = 1;
        while i < s.len() {
            match s[i] {
                b'"' => return (String::from_utf8_lossy(&value).into_owned(), &s[i + 1..]),
                b'\\' if i + 1 < s.len() => {
                    value.push(s[i + 1]);
                    i += 2;
                }
                c => {
                    value.push(c);
                    i += 1;
                }
            }
        }
        (String::new(), b"")
    }
    let mut result = Vec::new();
    'headers: for value in headers.get_all(http::header::SEC_WEBSOCKET_EXTENSIONS) {
        let mut s = value.as_bytes();
        loop {
            let (name, rest) = token(skip_space(s));
            s = rest;
            if name.is_empty() {
                continue 'headers;
            }
            let mut extension = vec![(name, String::new())];
            loop {
                s = skip_space(s);
                let Some(rest) = s.strip_prefix(b";") else {
                    break;
                };
                let (key, rest) = token(skip_space(rest));
                if key.is_empty() {
                    continue 'headers;
                }
                s = skip_space(rest);
                let mut param = String::new();
                if let Some(rest) = s.strip_prefix(b"=") {
                    let (v, rest) = token_or_quoted(skip_space(rest));
                    param = v;
                    s = skip_space(rest);
                }
                if !s.is_empty() && s[0] != b',' && s[0] != b';' {
                    continue 'headers;
                }
                extension.push((key, param));
            }
            if !s.is_empty() && s[0] != b',' {
                continue 'headers;
            }
            result.push(extension);
            if s.is_empty() {
                continue 'headers;
            }
            s = &s[1..];
        }
    }
    result
}

/// gorilla `decompressNoContextTakeover`: the RFC's `00 00 ff ff` tail plus a final
/// empty block so the inflater ends cleanly.
fn inflate(payload: &[u8], limit: usize) -> io::Result<Vec<u8>> {
    const TAIL: &[u8] = b"\x00\x00\xff\xff\x01\x00\x00\xff\xff";
    let mut out = Vec::new();
    flate2::read::DeflateDecoder::new(Read::chain(payload, TAIL))
        .take(limit as u64 + 1)
        .read_to_end(&mut out)?;
    if out.len() > limit {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "inflated message too large"));
    }
    Ok(out)
}

/// One server frame's header.
struct Header {
    fin: bool,
    rsv1: bool,
    /// RSV2 or RSV3: never valid here; gorilla rejects the frame.
    rsv23: bool,
    opcode: u8,
    masked: bool,
    /// Header length, extended length and mask key included.
    len: usize,
    payload: usize,
}

fn header(buf: &[u8]) -> Option<Header> {
    let (&b0, &b1) = (buf.first()?, buf.get(1)?);
    let (mut len, short) = (2usize, usize::from(b1 & 0x7f));
    let payload = match short {
        126 => {
            len += 2;
            usize::from(u16::from_be_bytes(buf.get(2..4)?.try_into().ok()?))
        }
        127 => {
            len += 8;
            usize::try_from(u64::from_be_bytes(buf.get(2..10)?.try_into().ok()?)).unwrap_or(usize::MAX)
        }
        n => n,
    };
    let masked = b1 & 0x80 != 0;
    if masked {
        len += 4;
    }
    (buf.len() >= len).then_some(Header {
        fin: b0 & 0x80 != 0,
        rsv1: b0 & 0x40 != 0,
        rsv23: b0 & 0x30 != 0,
        opcode: b0 & 0x0f,
        masked,
        len,
        payload,
    })
}

/// A final, unmasked frame (servers never mask).
fn encode(opcode: u8, payload: &[u8], out: &mut BytesMut) {
    out.extend_from_slice(&[0x80 | opcode]);
    match payload.len() {
        n if n < 126 => out.extend_from_slice(&[n as u8]),
        n if n <= usize::from(u16::MAX) => {
            out.extend_from_slice(&[126]);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            out.extend_from_slice(&[127]);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    out.extend_from_slice(payload);
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// The upgraded connection under tungstenite, inflating compressed messages when the
/// handshake negotiated permessage-deflate.
pub(crate) struct Inflate<S> {
    inner: S,
    enabled: bool,
    /// Largest frame or inflated message accepted.
    limit: usize,
    /// Bytes read from `inner` that do not form a whole frame yet.
    raw: BytesMut,
    /// Frames ready for tungstenite.
    ready: BytesMut,
    /// The compressed message being assembled: its opcode and payload so far.
    message: Option<(u8, Vec<u8>)>,
    eof: bool,
    /// A decoding failure, reported once the frames before it are delivered.
    failed: Option<io::Error>,
}

impl<S> Inflate<S> {
    pub(super) fn new(inner: S, enabled: bool, limit: usize) -> Self {
        Self {
            inner,
            enabled,
            limit,
            raw: BytesMut::new(),
            ready: BytesMut::new(),
            message: None,
            eof: false,
            failed: None,
        }
    }

    /// Moves every whole frame in `raw` to `ready`, inflating compressed messages.
    fn process(&mut self) -> io::Result<()> {
        while let Some(h) = header(&self.raw) {
            if h.payload > self.limit {
                return Err(invalid("websocket frame too large"));
            }
            if self.raw.len() < h.len + h.payload {
                return Ok(());
            }
            let mut frame = self.raw.split_to(h.len + h.payload);
            if h.masked || h.rsv23 {
                // Servers never mask and never set RSV2/RSV3: the frame goes on as it is,
                // so tungstenite rejects it as gorilla does (inflating would hide it).
                self.ready.extend_from_slice(&frame);
                continue;
            }
            let payload = &frame[h.len..];
            match (self.message.is_some(), h.opcode) {
                // Only a message's first frame decides compression.
                (false, 1 | 2) if h.rsv1 => {
                    if h.fin {
                        let inflated = inflate(payload, self.limit)?;
                        encode(h.opcode, &inflated, &mut self.ready);
                    } else {
                        self.message = Some((h.opcode, payload.to_vec()));
                    }
                }
                (true, 0) => {
                    let (opcode, mut compressed) = self.message.take().expect("message in progress");
                    if compressed.len() + payload.len() > self.limit {
                        return Err(invalid("websocket message too large"));
                    }
                    compressed.extend_from_slice(payload);
                    if h.fin {
                        let inflated = inflate(&compressed, self.limit)?;
                        encode(opcode, &inflated, &mut self.ready);
                    } else {
                        self.message = Some((opcode, compressed));
                    }
                }
                // Control frames may arrive between fragments.
                (true, op) if op < 8 => return Err(invalid("websocket: data before FIN")),
                // Everything else passes on uncompressed. Once negotiated, gorilla accepts
                // RSV1 on any frame, so it is cleared for tungstenite.
                _ => {
                    frame[0] &= !0x40;
                    self.ready.extend_from_slice(&frame);
                }
            }
        }
        Ok(())
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Inflate<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        if !this.enabled {
            return Pin::new(&mut this.inner).poll_read(cx, buf);
        }
        loop {
            if !this.ready.is_empty() {
                let n = this.ready.len().min(buf.remaining());
                buf.put_slice(&this.ready[..n]);
                this.ready.advance(n);
                return Poll::Ready(Ok(()));
            }
            // Messages decoded before a bad one reach the reader first, as gorilla
            // returns them before its error.
            if let Some(error) = this.failed.take() {
                return Poll::Ready(Err(error));
            }
            if this.eof {
                // A partial frame at EOF goes on as it is; tungstenite reports it.
                if !this.raw.is_empty() {
                    let rest = this.raw.split();
                    this.ready.extend_from_slice(&rest);
                    continue;
                }
                return Poll::Ready(Ok(()));
            }
            let mut chunk = [0u8; 16 * 1024];
            let mut read = ReadBuf::new(&mut chunk);
            ready!(Pin::new(&mut this.inner).poll_read(cx, &mut read))?;
            if read.filled().is_empty() {
                this.eof = true;
                continue;
            }
            this.raw.extend_from_slice(read.filled());
            if let Err(error) = this.process() {
                this.failed = Some(error);
                // Nothing after the failure is read again.
                this.raw.clear();
                this.eof = true;
            }
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Inflate<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
#[path = "codex_ws_deflate_tests.rs"]
mod tests;
