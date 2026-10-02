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
use tokio::io::{AsyncRead, AsyncReadExt, BufReader};
use tokio_util::io::{ReaderStream, StreamReader};

/// Largest upstream error body read; the rest is never pulled off the socket.
pub(crate) const MAX_ERROR_BODY: usize = 64 * 1024;

pub(crate) fn transport_error(e: wreq::Error) -> ExecError {
    // wreq errors may include a URL with credential-bearing query parameters.
    let _ = e;
    ExecError::local(502, FailureScope::Transport, "upstream request failed")
}

/// Status, headers (encoding/length removed) and the decoded body as raw chunks.
/// The Claude executor reads lines itself, so nothing is framed or buffered here.
pub(crate) async fn decoded_response(res: wreq::Response) -> Result<(u16, HeaderMap, ExecStream), ExecError> {
    let status = res.status().as_u16();
    let mut headers = res.headers().clone();
    let body = decoded(res.bytes_stream(), &headers).await?;
    headers.remove(http::header::CONTENT_ENCODING);
    headers.remove(http::header::CONTENT_LENGTH);
    Ok((status, headers, body))
}

/// Non-2xx responses become [`ExecError`]; event streams are framed; anything else is
/// buffered. Decompression runs before framing and applies to errors too.
pub(crate) async fn into_response(res: wreq::Response) -> Result<ExecResponse, ExecError> {
    let status = res.status().as_u16();
    let mut headers = res.headers().clone();
    let body = decoded(res.bytes_stream(), &headers).await?;
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

async fn decoded<S>(body: S, headers: &HeaderMap) -> Result<ExecStream, ExecError>
where
    S: Stream<Item = wreq::Result<Bytes>> + Send + Unpin + 'static,
{
    let reader = StreamReader::new(body.map(|r| r.map_err(std::io::Error::other)));
    let mut reader = BufReader::new(reader);
    // Sniff across arbitrary TCP fragmentation, not only the first chunk.
    let mut prefix = Vec::new();
    for _ in 0..4 {
        let mut byte = [0];
        if reader.read(&mut byte).await.map_err(read_error)? == 0 {
            break;
        }
        prefix.push(byte[0]);
    }
    let encoding = headers
        .get_all(http::header::CONTENT_ENCODING)
        .iter()
        .map(|value| value.to_str().map(str::trim))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| decode_error())?
        .join(",")
        .to_ascii_lowercase();
    // ponytail: stacked encodings are rejected, not partly decoded. The upgrade
    // path is Go's reverse-order decoder chain (claude_executor_request.go).
    let encoding = if encoding.is_empty() || encoding == "identity" {
        if prefix.starts_with(&[0x1f, 0x8b]) {
            "gzip"
        } else if prefix.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
            "zstd"
        } else {
            "identity"
        }
    } else {
        &encoding
    };
    let reader = BufReader::new(std::io::Cursor::new(prefix).chain(reader));
    let decoder: Pin<Box<dyn AsyncRead + Send>> = match encoding {
        "identity" => Box::pin(reader),
        "gzip" | "x-gzip" => {
            let mut decoder = GzipDecoder::new(reader);
            decoder.multiple_members(true);
            Box::pin(decoder)
        }
        "zstd" => {
            let mut decoder = ZstdDecoder::new(reader);
            decoder.multiple_members(true);
            Box::pin(decoder)
        }
        "br" => Box::pin(BrotliDecoder::new(reader)),
        "deflate" => {
            // Go accepts both zlib-wrapped and raw deflate.
            use tokio::io::AsyncBufReadExt;
            let mut reader = reader;
            let head = reader.fill_buf().await.map_err(read_error)?;
            let zlib = head.len() >= 2
                && head[0] & 15 == 8
                && head[0] >> 4 <= 7
                && (u16::from(head[0]) * 256 + u16::from(head[1])) % 31 == 0;
            if zlib {
                Box::pin(ZlibDecoder::new(reader))
            } else {
                Box::pin(DeflateDecoder::new(reader))
            }
        }
        _ => return Err(decode_error()),
    };
    Ok(ReaderStream::new(decoder).map(|r| r.map_err(read_error)).boxed())
}

fn read_error(error: std::io::Error) -> ExecError {
    if error.get_ref().is_some_and(|inner| inner.is::<wreq::Error>()) {
        ExecError::local(502, FailureScope::Transport, "upstream request failed")
    } else {
        decode_error()
    }
}

fn decode_error() -> ExecError {
    ExecError::local(
        502,
        FailureScope::Request,
        "upstream request failed: invalid or unsupported response compression",
    )
}

/// Frames an SSE body into whole events. The first `Err` is the last item.
fn framed<S>(body: S) -> ExecStream
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

    fn chunks(items: Vec<wreq::Result<Bytes>>) -> impl Stream<Item = wreq::Result<Bytes>> + Send + Unpin + 'static {
        futures_util::stream::iter(items)
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
    async fn unknown_declared_encoding_is_rejected() {
        let mut h = HeaderMap::new();
        h.insert(http::header::CONTENT_ENCODING, "compress".parse().unwrap());
        assert!(decoded(chunks(vec![Ok(Bytes::from_static(b"bad"))]), &h).await.is_err());
        h.insert(http::header::CONTENT_ENCODING, "identity".parse().unwrap());
        h.append(http::header::CONTENT_ENCODING, "br".parse().unwrap());
        assert!(decoded(chunks(vec![Ok(Bytes::from_static(b"bad"))]), &h).await.is_err());
    }

    #[tokio::test]
    async fn compression_decodes_fragmented_gzip_zstd_brotli_and_deflate() {
        use async_compression::tokio::bufread::{BrotliEncoder, DeflateEncoder, GzipEncoder, ZlibEncoder, ZstdEncoder};
        const PLAIN: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        for name in ["gzip", "zstd", "br", "deflate", "raw-deflate"] {
            let reader = BufReader::new(std::io::Cursor::new(PLAIN));
            let mut encoder: Pin<Box<dyn AsyncRead + Send>> = match name {
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
        let body = chunks(vec![Ok(Bytes::from_static(b"data: ok\n\n")), Err(error)]);
        let stream = decoded(body, &HeaderMap::new()).await.unwrap();
        let items: Vec<_> = framed(stream).collect().await;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].as_ref().unwrap(), b"data: ok\n\n".as_slice());
        assert_eq!(items[1].as_ref().unwrap_err().scope, FailureScope::Transport);
    }
}
