//! Response plumbing shared by the inference routes: JSON bodies, SSE headers and the
//! stream forwarder (sdk/api/handlers/stream_forwarder.go).

use std::collections::VecDeque;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use cpa_core::config::Config;
use cpa_core::exec::{ExecError, ExecStream};
use futures_util::StreamExt;

/// A JSON response with an explicit Content-Type.
pub fn json(status: u16, content_type: &'static str, body: impl Into<Body>) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        [(header::CONTENT_TYPE, HeaderValue::from_static(content_type))],
        body.into(),
    )
        .into_response()
}

/// gin `c.JSON`: compact JSON with the charset parameter.
pub fn gin_json(status: u16, body: String) -> Response {
    json(status, "application/json; charset=utf-8", body)
}

/// gin `c.JSON(status, ErrorResponse{Error: ErrorDetail{Message, Type}})`.
pub fn error_detail(status: u16, message: &str, kind: &str) -> Response {
    let detail = crate::gojson::Obj::new()
        .str("message", message)
        .str("type", kind)
        .finish();
    gin_json(status, crate::gojson::Obj::new().raw("error", &detail).finish())
}

/// The headers every Go SSE handler sets before the first event.
pub fn sse(body: Body) -> Response {
    let mut res = body.into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    res
}

/// `requests.streaming.keepalive-seconds` (0 or absent disables heartbeats).
pub fn keepalive(cfg: &Config) -> Option<Duration> {
    let seconds = cfg
        .document
        .get("requests")
        .and_then(|r| r.get("streaming"))
        .and_then(|s| s.get("keepalive-seconds"))
        .and_then(serde_yaml_ng::Value::as_i64)
        .unwrap_or(0);
    (seconds > 0).then(|| Duration::from_secs(seconds as u64))
}

/// How one route frames a stream.
pub trait Writer: Send + 'static {
    /// One upstream event in, the bytes to write out.
    fn chunk(&mut self, event: Bytes) -> Vec<Bytes>;
    /// Set after a chunk carried a terminal error: stop forwarding.
    fn stopped(&self) -> bool {
        false
    }
    /// Bytes for a terminal error after the stream committed.
    fn error(&mut self, error: &ExecError) -> Vec<Bytes>;
    /// Bytes for a clean end of stream.
    fn end(&mut self) -> Vec<Bytes>;
    /// A heartbeat while idle.
    fn heartbeat(&mut self) -> Bytes {
        Bytes::from_static(b": keep-alive\n\n")
    }
}

/// Forwards `first` and `rest` through `writer` as a response body.
pub fn stream<W: Writer>(first: Option<Bytes>, rest: ExecStream, mut writer: W, keepalive: Option<Duration>) -> Body {
    let (ready, finished) = match first {
        Some(event) => {
            let ready = writer.chunk(event);
            (ready, writer.stopped())
        }
        None => (writer.end(), true),
    };
    resume(ready, finished, rest, writer, keepalive)
}

/// Continues a stream whose first bytes the writer already produced.
pub fn resume<W: Writer>(
    ready: Vec<Bytes>,
    finished: bool,
    rest: ExecStream,
    writer: W,
    keepalive: Option<Duration>,
) -> Body {
    let ready: VecDeque<Bytes> = ready.into();
    struct State<W> {
        rest: ExecStream,
        writer: W,
        ready: VecDeque<Bytes>,
        finished: bool,
        ticker: Option<tokio::time::Interval>,
    }
    let ticker = keepalive.map(|period| {
        let mut t = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        t
    });
    let state = State {
        rest,
        writer,
        ready,
        finished,
        ticker,
    };
    let body = futures_util::stream::unfold(state, |mut s| async move {
        loop {
            if let Some(bytes) = s.ready.pop_front() {
                return Some((Ok::<_, std::convert::Infallible>(bytes), s));
            }
            if s.finished {
                return None;
            }
            let next = match s.ticker.as_mut() {
                Some(ticker) => tokio::select! {
                    item = s.rest.next() => Some(item),
                    _ = ticker.tick() => None,
                },
                None => Some(s.rest.next().await),
            };
            match next {
                None => {
                    let beat = s.writer.heartbeat();
                    s.ready.push_back(beat);
                }
                Some(Some(Ok(event))) => {
                    s.ready.extend(s.writer.chunk(event));
                    s.finished = s.writer.stopped();
                }
                Some(Some(Err(error))) => {
                    s.ready.extend(s.writer.error(&error));
                    s.finished = true;
                }
                Some(None) => {
                    s.ready.extend(s.writer.end());
                    s.finished = true;
                }
            }
        }
    });
    Body::from_stream(body)
}

/// Normalizes an executor event to SSE payload lines: `data: x` frames pass through,
/// bare JSON becomes `data: x\n\n`.
pub fn ensure_frame(event: Bytes) -> Bytes {
    let trimmed = event.trim_ascii_start();
    if trimmed.starts_with(b"data:") || trimmed.starts_with(b"event:") || trimmed.starts_with(b":") {
        if event.ends_with(b"\n\n") {
            event
        } else {
            let mut out = event.to_vec();
            out.extend_from_slice(if event.ends_with(b"\n") { b"\n" } else { b"\n\n" });
            Bytes::from(out)
        }
    } else {
        Bytes::from([b"data: ".as_slice(), &event, b"\n\n"].concat())
    }
}

/// The `data:` payloads of one SSE frame, joined with newlines.
pub fn data_payload(frame: &[u8]) -> Option<Vec<u8>> {
    let mut out: Option<Vec<u8>> = None;
    for line in frame.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line).trim_ascii();
        let Some(data) = line.strip_prefix(b"data:") else {
            continue;
        };
        let data = data.trim_ascii();
        match &mut out {
            Some(buf) => {
                buf.push(b'\n');
                buf.extend_from_slice(data);
            }
            None => out = Some(data.to_vec()),
        }
    }
    out
}

/// The `event:` name of one SSE frame.
pub fn event_name(frame: &[u8]) -> String {
    frame
        .split(|&b| b == b'\n')
        .find_map(|line| line.trim_ascii().strip_prefix(b"event:"))
        .map(|name| String::from_utf8_lossy(name.trim_ascii()).into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Plain;
    impl Writer for Plain {
        fn chunk(&mut self, event: Bytes) -> Vec<Bytes> {
            vec![event]
        }
        fn error(&mut self, _: &ExecError) -> Vec<Bytes> {
            vec![Bytes::from_static(b"ERR")]
        }
        fn end(&mut self) -> Vec<Bytes> {
            vec![Bytes::from_static(b"END")]
        }
    }

    async fn collect(body: Body) -> String {
        String::from_utf8(axum::body::to_bytes(body, usize::MAX).await.unwrap().to_vec()).unwrap()
    }

    #[tokio::test]
    async fn forwards_then_ends_or_errors_and_beats_while_idle() {
        let rest = futures_util::stream::iter([Ok(Bytes::from_static(b"b"))]).boxed();
        assert_eq!(
            collect(stream(Some(Bytes::from_static(b"a")), rest, Plain, None)).await,
            "abEND"
        );
        let err = cpa_core::exec::ExecError::local(502, cpa_core::exec::FailureScope::Transport, "x");
        let rest = futures_util::stream::iter([Err(err)])
            .chain(futures_util::stream::pending())
            .boxed();
        assert_eq!(
            collect(stream(Some(Bytes::from_static(b"a")), rest, Plain, None)).await,
            "aERR"
        );
        assert_eq!(
            collect(stream(None, futures_util::stream::pending().boxed(), Plain, None)).await,
            "END"
        );
        tokio::time::pause();
        let slow = futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_millis(250)).await;
            Ok(Bytes::from_static(b"z"))
        })
        .boxed();
        let body = stream(
            Some(Bytes::from_static(b"a")),
            slow,
            Plain,
            Some(Duration::from_millis(100)),
        );
        assert_eq!(collect(body).await, "a: keep-alive\n\n: keep-alive\n\nzEND");
    }

    #[test]
    fn frames_and_payloads() {
        assert_eq!(ensure_frame(Bytes::from_static(b"{}")), "data: {}\n\n");
        assert_eq!(ensure_frame(Bytes::from_static(b"data: {}\n")), "data: {}\n\n");
        assert_eq!(
            data_payload(b"event: x\r\ndata: {\"a\":1}\r\n\r\n").unwrap(),
            b"{\"a\":1}"
        );
        assert_eq!(
            event_name(b"event: response.completed\ndata: {}\n\n"),
            "response.completed"
        );
        assert!(data_payload(b": ping\n\n").is_none());
    }
}
