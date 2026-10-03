//! Turning a wreq response into the execution envelope. Shared by provider executors.

use std::collections::VecDeque;
use std::pin::Pin;
use std::time::Duration;

use async_compression::tokio::bufread::{BrotliDecoder, DeflateDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder};
use bytes::{Bytes, BytesMut};
use cpa_core::exec::{ExecError, ExecResponse, ExecStream, FailureScope, ResponseBody};
use cpa_translate::sse::Framer;
use futures_util::{Stream, StreamExt};
use http::HeaderMap;
use tokio::io::{AsyncBufRead, AsyncReadExt, BufReader};
use tokio_util::io::{ReaderStream, StreamReader};

/// Largest upstream error body read; the rest is never pulled off the socket.
pub(crate) const MAX_ERROR_BODY: usize = 64 * 1024;

pub(crate) fn transport_error(e: wreq::Error) -> ExecError {
    // wreq errors may include a URL with credential-bearing query parameters.
    let _ = e;
    ExecError::local(502, FailureScope::Transport, "upstream request failed")
}

/// A response whose body went through Go's Claude `decodeResponseBody`. `body` is
/// the decoded stream, or the decoder-construction error text Go reports.
pub(crate) struct Decoded {
    pub status: u16,
    /// Without Content-Encoding and Content-Length, which described the raw bytes.
    pub headers: HeaderMap,
    pub body: Result<ExecStream, String>,
}

/// Decodes a Go standard-transport or native exchange ([`crate::proxy::send_routed`]).
pub(crate) async fn decode_upstream(upstream: crate::proxy::Upstream) -> Decoded {
    let mut headers = upstream.headers;
    let encoding = content_encoding(&headers);
    headers.remove(http::header::CONTENT_ENCODING);
    headers.remove(http::header::CONTENT_LENGTH);
    let body = decode_body(upstream.body.map(|r| r.map_err(std::io::Error::other)), &encoding).await;
    Decoded {
        status: upstream.status,
        headers,
        body,
    }
}

/// `claudeResponseContentEncoding`: every Content-Encoding value, joined by commas.
fn content_encoding(headers: &HeaderMap) -> String {
    headers
        .get_all(http::header::CONTENT_ENCODING)
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .collect::<Vec<_>>()
        .join(",")
}

/// Non-2xx responses become [`ExecError`]; event streams are framed; anything else is
/// buffered. Decompression runs before framing and applies to errors too.
pub(crate) async fn into_response(res: wreq::Response) -> Result<ExecResponse, ExecError> {
    let status = res.status().as_u16();
    let mut headers = res.headers().clone();
    let encoding = content_encoding(&headers);
    let body = decode_body(res.bytes_stream().map(|r| r.map_err(std::io::Error::other)), &encoding)
        .await
        .map_err(|_| decode_error())?;
    headers.remove(http::header::CONTENT_ENCODING);
    headers.remove(http::header::CONTENT_LENGTH);
    if !(200..300).contains(&status) {
        let body = read_bounded(body, MAX_ERROR_BODY).await?;
        return Err(ExecError {
            status,
            scope: scope_for(status),
            retry_after: retry_after(&headers),
            headers: Box::new(headers),
            body,
            direct: false,
        });
    }
    let is_sse = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    let body = if is_sse {
        ResponseBody::Stream(framed(body))
    } else {
        let bytes = read_bounded(body, usize::MAX).await?;
        ResponseBody::Buffered(bytes)
    };
    Ok(ExecResponse { status, headers, body })
}

pub(crate) async fn read_bounded<S>(mut body: S, limit: usize) -> Result<Bytes, ExecError>
where
    S: Stream<Item = Result<Bytes, ExecError>> + Unpin,
{
    let mut out = BytesMut::new();
    while out.len() < limit {
        match body.next().await {
            Some(Ok(chunk)) => out.extend_from_slice(&chunk[..chunk.len().min(limit - out.len())]),
            Some(Err(error)) => return Err(error),
            None => break,
        }
    }
    Ok(out.freeze())
}

type Reader = Pin<Box<dyn AsyncBufRead + Send>>;

/// Reads up to `n` bytes without losing them: the bytes, the reader positioned before
/// them again, and the read error that stopped short of `n` (EOF is not an error).
async fn peek(mut reader: Reader, n: usize) -> (Vec<u8>, Reader, Option<std::io::Error>) {
    let mut prefix = Vec::with_capacity(n);
    let mut error = None;
    while prefix.len() < n {
        let mut byte = [0];
        match reader.read(&mut byte).await {
            Ok(0) => break,
            Ok(_) => prefix.push(byte[0]),
            Err(e) => {
                error = Some(e);
                break;
            }
        }
    }
    let rest: Reader = Box::pin(BufReader::new(std::io::Cursor::new(prefix.clone()).chain(reader)));
    (prefix, rest, error)
}

/// `gzip.NewReader`'s eager header read: `EOF`, `unexpected EOF` or the bad header.
async fn gzip_reader(reader: Reader) -> Result<Reader, String> {
    let (head, reader, error) = peek(reader, 10).await;
    if let Some(error) = error {
        return Err(io_text(&error));
    }
    match head.len() {
        0 => Err("EOF".into()),
        1..10 => Err("unexpected EOF".into()),
        _ if head[..3] != [0x1f, 0x8b, 8] => Err("gzip: invalid header".into()),
        _ => {
            let mut decoder = GzipDecoder::new(reader);
            decoder.multiple_members(true);
            Ok(Box::pin(BufReader::new(decoder)))
        }
    }
}

fn zstd_reader(reader: Reader) -> Reader {
    let mut decoder = ZstdDecoder::new(reader);
    decoder.multiple_members(true);
    Box::pin(BufReader::new(decoder))
}

/// `decodeResponseBody`. Without any Content-Encoding header, gzip and zstd are
/// recognised by their magic bytes; otherwise each listed coding is undone in reverse
/// order, `identity` and empty entries are skipped, and anything else is refused.
pub(crate) async fn decode_body<S>(body: S, encoding: &str) -> Result<ExecStream, String>
where
    S: Stream<Item = std::io::Result<Bytes>> + Send + Unpin + 'static,
{
    let mut reader: Reader = Box::pin(BufReader::new(StreamReader::new(body)));
    if encoding.is_empty() {
        // Sniff across arbitrary TCP fragmentation, not only the first chunk.
        let (magic, rest, error) = peek(reader, 4).await;
        reader = rest;
        // bufio.Peek: a short read only counts at EOF with two bytes; a failed read
        // leaves the body as is, so the error surfaces to the caller's read.
        if error.is_none() && magic.starts_with(&[0x1f, 0x8b]) {
            reader = gzip_reader(reader)
                .await
                .map_err(|e| format!("magic-byte gzip: failed to create reader: {e}"))?;
        } else if error.is_none() && magic.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
            reader = zstd_reader(reader);
        } else if let Some(error) = error {
            let failed = futures_util::stream::iter([Err(read_error(error))]);
            return Ok(ReaderStream::new(reader)
                .map(|r| r.map_err(read_error))
                .chain(failed)
                .boxed());
        }
        return Ok(ReaderStream::new(reader).map(|r| r.map_err(read_error)).boxed());
    }
    for coding in encoding.split(',').rev() {
        reader = match coding.trim().to_lowercase().as_str() {
            "" | "identity" => continue,
            "gzip" => gzip_reader(reader)
                .await
                .map_err(|e| format!("failed to create gzip reader: {e}"))?,
            "deflate" => {
                // newClaudeDeflateReader: zlib when the first two bytes say so, else raw.
                let (head, rest, _) = peek(reader, 2).await;
                let zlib = head.len() == 2
                    && head[0] & 15 == 8
                    && head[0] >> 4 <= 7
                    && (u16::from(head[0]) * 256 + u16::from(head[1])) % 31 == 0;
                if zlib {
                    Box::pin(BufReader::new(ZlibDecoder::new(rest)))
                } else {
                    Box::pin(BufReader::new(DeflateDecoder::new(rest)))
                }
            }
            "br" => Box::pin(BufReader::new(BrotliDecoder::new(reader))),
            "zstd" => zstd_reader(reader),
            other => return Err(format!("unsupported content encoding {other:?}")),
        };
    }
    Ok(ReaderStream::new(reader).map(|r| r.map_err(read_error)).boxed())
}

/// Go's text for a body read failure: a truncated or reset body is `unexpected EOF`.
// ponytail: decoder failures keep the Rust decoder's wording, not compress/*'s.
pub(crate) fn io_text(error: &std::io::Error) -> String {
    match error.get_ref() {
        Some(inner) if inner.is::<wreq::Error>() || inner.is::<ExecError>() => "unexpected EOF".into(),
        _ if error.kind() == std::io::ErrorKind::UnexpectedEof => "unexpected EOF".into(),
        _ => error.to_string(),
    }
}

fn read_error(error: std::io::Error) -> ExecError {
    match error.get_ref() {
        Some(inner) if inner.is::<wreq::Error>() => {
            ExecError::local(502, FailureScope::Transport, "upstream request failed")
        }
        Some(inner) => match inner.downcast_ref::<ExecError>() {
            Some(error) => error.clone(),
            None => read_decode_error(&error),
        },
        None => read_decode_error(&error),
    }
}

/// A decompressor failing mid-body: a plain Go error from `io.ReadAll`.
fn read_decode_error(error: &std::io::Error) -> ExecError {
    ExecError::local(500, FailureScope::Transport, io_text(error))
}

fn decode_error() -> ExecError {
    ExecError::local(
        502,
        FailureScope::Request,
        "upstream request failed: invalid or unsupported response compression",
    )
}

/// Frames an SSE body into whole events. The first `Err` is the last item.
pub(crate) fn framed<S>(body: S) -> ExecStream
where
    S: Stream<Item = Result<Bytes, ExecError>> + Send + Unpin + 'static,
{
    struct State<S> {
        body: S,
        framer: Framer,
        ready: VecDeque<Bytes>,
        done: bool,
    }
    let state = State {
        body,
        framer: Framer::default(),
        ready: VecDeque::new(),
        done: false,
    };
    futures_util::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(event) = st.ready.pop_front() {
                return Some((Ok(event), st));
            }
            if st.done {
                return None;
            }
            let fail = match st.body.next().await {
                Some(Ok(chunk)) => {
                    match st
                        .framer
                        .push(&chunk)
                        .map_err(|e| ExecError::local(502, FailureScope::Request, e.to_string()))
                    {
                        Ok(events) => {
                            st.ready.extend(events);
                            continue;
                        }
                        Err(e) => e,
                    }
                }
                Some(Err(e)) => e,
                None => {
                    st.done = true;
                    st.ready.extend(st.framer.finish());
                    continue;
                }
            };
            st.done = true;
            st.ready.clear();
            return Some((Err(fail), st));
        }
    })
    .boxed()
}

// ponytail: status-only classification. The scheduler port (sdk/cliproxy/auth, custom
// request-error rules) replaces this with per-provider and per-credential rules.
pub(crate) fn scope_for(status: u16) -> FailureScope {
    match status {
        401 | 402 | 403 | 408 | 429 | 500.. => FailureScope::Credential,
        _ => FailureScope::Request,
    }
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let now = std::time::SystemTime::now();
    crate::quota::retry_after(headers.get(http::header::RETRY_AFTER)?.to_str().ok()?, now)?
        .duration_since(now)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(
        items: Vec<std::io::Result<Bytes>>,
    ) -> impl Stream<Item = std::io::Result<Bytes>> + Send + Unpin + 'static {
        futures_util::stream::iter(items)
    }

    async fn decoded<S>(body: S, headers: &HeaderMap) -> Result<ExecStream, String>
    where
        S: Stream<Item = std::io::Result<Bytes>> + Send + Unpin + 'static,
    {
        decode_body(body, &content_encoding(headers)).await
    }

    #[tokio::test]
    async fn error_body_read_is_bounded() {
        let endless = futures_util::stream::repeat_with(|| Ok(Bytes::from_static(&[b'x'; 1000])));
        let body = read_bounded(endless, 2500).await.unwrap();
        assert_eq!(body.len(), 2500, "must stop reading at the limit");
    }

    #[tokio::test]
    async fn framed_stream_stops_after_first_error() {
        let failed = futures_util::stream::iter(vec![Err(decode_error()), Ok(Bytes::from_static(b"data: x\n\n"))]);
        let items: Vec<_> = framed(failed).collect().await;
        assert_eq!(items.len(), 1);
        assert!(items[0].is_err());

        let ok = futures_util::stream::iter(vec![
            Ok(Bytes::from_static(b"data: a\n\ndata: b")),
            Ok(Bytes::from_static(b"\n\n")),
        ]);
        let items: Vec<Bytes> = framed(ok).map(Result::unwrap).collect().await;
        assert_eq!(
            items,
            [Bytes::from_static(b"data: a\n\n"), Bytes::from_static(b"data: b\n\n")]
        );
    }

    #[tokio::test]
    async fn declared_encodings_follow_go_decode_response_body() {
        let mut h = HeaderMap::new();
        h.insert(http::header::CONTENT_ENCODING, "compress".parse().unwrap());
        let error = decoded(chunks(vec![Ok(Bytes::from_static(b"bad"))]), &h).await.err();
        assert_eq!(error.as_deref(), Some(r#"unsupported content encoding "compress""#));
        // An explicit identity disables magic-byte sniffing: gzip bytes pass through.
        let gzip_magic = Bytes::from_static(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3]);
        h.insert(http::header::CONTENT_ENCODING, "identity".parse().unwrap());
        let stream = decoded(chunks(vec![Ok(gzip_magic.clone())]), &h).await.unwrap();
        assert_eq!(read_bounded(stream, usize::MAX).await.unwrap(), gzip_magic);
        // gzip.NewReader reads its header eagerly.
        h.insert(http::header::CONTENT_ENCODING, "gzip".parse().unwrap());
        let error = decoded(chunks(vec![Ok(Bytes::from_static(b"bad body"))]), &h)
            .await
            .err();
        assert_eq!(error.as_deref(), Some("failed to create gzip reader: unexpected EOF"));
        let error = decoded(chunks(vec![Ok(Bytes::from_static(b"not a gzip body"))]), &h)
            .await
            .err();
        assert_eq!(
            error.as_deref(),
            Some("failed to create gzip reader: gzip: invalid header")
        );
        let error = decoded(chunks(vec![]), &h).await.err();
        assert_eq!(error.as_deref(), Some("failed to create gzip reader: EOF"));
    }

    #[tokio::test]
    async fn compression_decodes_fragmented_gzip_zstd_brotli_and_deflate() {
        use async_compression::tokio::bufread::{BrotliEncoder, DeflateEncoder, GzipEncoder, ZlibEncoder, ZstdEncoder};
        const PLAIN: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        for name in ["gzip", "zstd", "br", "deflate", "raw-deflate"] {
            let reader = BufReader::new(std::io::Cursor::new(PLAIN));
            let mut encoder: Pin<Box<dyn tokio::io::AsyncRead + Send>> = match name {
                "gzip" => Box::pin(GzipEncoder::new(reader)),
                "zstd" => Box::pin(ZstdEncoder::new(reader)),
                "br" => Box::pin(BrotliEncoder::new(reader)),
                "deflate" => Box::pin(ZlibEncoder::new(reader)),
                _ => Box::pin(DeflateEncoder::new(reader)),
            };
            let mut encoded = Vec::new();
            encoder.read_to_end(&mut encoded).await.unwrap();
            for declared in [true, false] {
                if !declared && !matches!(name, "gzip" | "zstd") {
                    continue;
                }
                let mut headers = HeaderMap::new();
                if declared {
                    headers.insert("content-encoding", name.trim_start_matches("raw-").parse().unwrap());
                }
                let chunks = encoded
                    .iter()
                    .map(|byte| Ok(Bytes::copy_from_slice(&[*byte])))
                    .collect();
                let stream = decoded(self::chunks(chunks), &headers).await.unwrap();
                assert_eq!(
                    read_bounded(stream, usize::MAX).await.unwrap(),
                    PLAIN,
                    "{name}, declared={declared}"
                );
            }
            if matches!(name, "gzip" | "zstd") {
                let doubled = [encoded.as_slice(), encoded.as_slice()].concat();
                let stream = decoded(chunks(vec![Ok(Bytes::from(doubled))]), &HeaderMap::new())
                    .await
                    .unwrap();
                assert_eq!(read_bounded(stream, usize::MAX).await.unwrap(), [PLAIN, PLAIN].concat());
            }
            if name == "gzip" {
                *encoded.last_mut().unwrap() ^= 1;
                let stream = decoded(chunks(vec![Ok(Bytes::from(encoded))]), &HeaderMap::new())
                    .await
                    .unwrap();
                assert!(
                    read_bounded(stream, usize::MAX).await.is_err(),
                    "corrupt gzip trailer must not be buffered as success"
                );
            }
        }
    }

    #[tokio::test]
    async fn lifecycle_failure_is_transport_not_quota_or_request() {
        let error = wreq::Client::new().get("not a URL").send().await.unwrap_err();
        assert_eq!(transport_error(error).scope, FailureScope::Transport);
        let error = wreq::Client::new().get("not a URL").send().await.unwrap_err();
        let body = chunks(vec![
            Ok(Bytes::from_static(b"data: ok\n\n")),
            Err(std::io::Error::other(error)),
        ]);
        let stream = decoded(body, &HeaderMap::new()).await.unwrap();
        let items: Vec<_> = framed(stream).collect().await;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].as_ref().unwrap(), b"data: ok\n\n".as_slice());
        assert_eq!(items[1].as_ref().unwrap_err().scope, FailureScope::Transport);
        // A Go-transport body error keeps its own classification through decoding.
        let failed = ExecError::local(502, FailureScope::Transport, "upstream request failed");
        let body = chunks(vec![Err(std::io::Error::other(failed))]);
        let stream = decoded(body, &HeaderMap::new()).await.unwrap();
        let items: Vec<_> = stream.collect().await;
        assert_eq!(items[0].as_ref().unwrap_err().scope, FailureScope::Transport);
    }
}
