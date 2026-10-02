//! Turning a wreq response into the execution envelope. Shared by provider executors.

use std::collections::VecDeque;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use cpa_core::exec::{ExecError, ExecResponse, ExecStream, FailureScope, ResponseBody};
use cpa_translate::sse::Framer;
use futures_util::{Stream, StreamExt};
use http::HeaderMap;

/// Largest upstream error body read; the rest is never pulled off the socket.
pub(crate) const MAX_ERROR_BODY: usize = 64 * 1024;

pub(crate) fn transport_error(e: wreq::Error) -> ExecError {
    ExecError::local(502, FailureScope::Credential, format!("upstream request failed: {e}"))
}

/// Non-2xx responses become [`ExecError`]; event streams are framed; anything else is
/// buffered. Compressed bodies are rejected until decoding lands.
pub(crate) async fn into_response(res: wreq::Response) -> Result<ExecResponse, ExecError> {
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    if !(200..300).contains(&status) {
        let body = read_bounded(res.bytes_stream(), MAX_ERROR_BODY).await;
        return Err(ExecError {
            status,
            scope: scope_for(status),
            retry_after: retry_after(&headers),
            headers: Box::new(headers),
            body,
            direct: false,
        });
    }
    reject_declared_encoding(&headers)?;
    let is_sse = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    let body = if is_sse {
        ResponseBody::Stream(framed(res.bytes_stream()))
    } else {
        let bytes = res.bytes().await.map_err(transport_error)?;
        reject_sniffed_encoding(&bytes)?;
        ResponseBody::Buffered(bytes)
    };
    Ok(ExecResponse { status, headers, body })
}

async fn read_bounded<S>(mut body: S, limit: usize) -> Bytes
where
    S: Stream<Item = wreq::Result<Bytes>> + Unpin,
{
    let mut out = BytesMut::new();
    while out.len() < limit {
        match body.next().await {
            Some(Ok(chunk)) => out.extend_from_slice(&chunk[..chunk.len().min(limit - out.len())]),
            _ => break,
        }
    }
    out.freeze()
}

// ponytail: compression is refused, not decoded. Executors send `accept-encoding:
// identity`; the Claude wire-profile port sends Claude Code's real accept-encoding and
// decodes gzip/br/zstd/deflate, including unlabelled bodies (claude_executor_request.go).
fn reject_declared_encoding(headers: &HeaderMap) -> Result<(), ExecError> {
    let encoding = headers
        .get(http::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if encoding.is_empty() || encoding.eq_ignore_ascii_case("identity") {
        return Ok(());
    }
    Err(compressed(encoding))
}

fn reject_sniffed_encoding(body: &[u8]) -> Result<(), ExecError> {
    if body.starts_with(&[0x1f, 0x8b]) {
        return Err(compressed("gzip (unlabelled)"));
    }
    if body.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return Err(compressed("zstd (unlabelled)"));
    }
    Ok(())
}

fn compressed(encoding: &str) -> ExecError {
    ExecError::local(
        502,
        FailureScope::Request,
        format!("upstream sent a {encoding} response, which is not supported yet"),
    )
}

/// Frames an SSE body into whole events. The first `Err` is the last item.
fn framed<S>(body: S) -> ExecStream
where
    S: Stream<Item = wreq::Result<Bytes>> + Send + Unpin + 'static,
{
    struct State<S> {
        body: S,
        framer: Framer,
        ready: VecDeque<Bytes>,
        first: bool,
        done: bool,
    }
    let state = State {
        body,
        framer: Framer::default(),
        ready: VecDeque::new(),
        first: true,
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
                    let sniff = if std::mem::take(&mut st.first) {
                        reject_sniffed_encoding(&chunk)
                    } else {
                        Ok(())
                    };
                    match sniff.and_then(|()| {
                        st.framer
                            .push(&chunk)
                            .map_err(|e| ExecError::local(502, FailureScope::Request, e.to_string()))
                    }) {
                        Ok(events) => {
                            st.ready.extend(events);
                            continue;
                        }
                        Err(e) => e,
                    }
                }
                Some(Err(e)) => transport_error(e),
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
fn scope_for(status: u16) -> FailureScope {
    match status {
        401 | 402 | 403 | 408 | 429 | 500.. => FailureScope::Credential,
        _ => FailureScope::Request,
    }
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let secs = headers
        .get(http::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(Duration::from_secs(secs))
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
        let body = read_bounded(endless, 2500).await;
        assert_eq!(body.len(), 2500, "must stop reading at the limit");
    }

    #[tokio::test]
    async fn framed_stream_stops_after_first_error() {
        let gz = chunks(vec![
            Ok(Bytes::from_static(&[0x1f, 0x8b, 8, 0])),
            Ok(Bytes::from_static(b"data: x\n\n")),
        ]);
        let items: Vec<_> = framed(gz).collect().await;
        assert_eq!(items.len(), 1);
        assert!(items[0].as_ref().unwrap_err().to_string().contains("gzip"));

        let ok = chunks(vec![
            Ok(Bytes::from_static(b"data: a\n\ndata: b")),
            Ok(Bytes::from_static(b"\n\n")),
        ]);
        let items: Vec<Bytes> = framed(ok).map(Result::unwrap).collect().await;
        assert_eq!(
            items,
            [Bytes::from_static(b"data: a\n\n"), Bytes::from_static(b"data: b\n\n")]
        );
    }

    #[test]
    fn declared_encoding_is_rejected() {
        let mut h = HeaderMap::new();
        assert!(reject_declared_encoding(&h).is_ok());
        h.insert(http::header::CONTENT_ENCODING, "identity".parse().unwrap());
        assert!(reject_declared_encoding(&h).is_ok());
        h.insert(http::header::CONTENT_ENCODING, "br".parse().unwrap());
        assert_eq!(reject_declared_encoding(&h).unwrap_err().status, 502);
    }
}
