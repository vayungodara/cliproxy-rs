//! xAI usage records when the client goes away, through the real router, dispatch and
//! usage tracker, against a loopback mock upstream. Go cancels the request context: the
//! pending HTTP send or body read fails with `context canceled` and the executor
//! publishes that failure (`TrackFailure`, the scanner's `PublishFailure`). An attempt
//! that ended before the client left keeps the record it already settled.

use std::sync::Arc;
use std::time::Duration;

use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

const CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_1","object":"response","model":"grok-4.7","status":"in_progress","output":[]}}"#;
const DELTA: &str =
    r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"hi"}"#;
const COMPLETED_USAGE: &str = r#"{"type":"response.completed","response":{"id":"resp_1","object":"response","model":"grok-4.7","status":"completed","output":[],"usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}}"#;
const COMPLETED_BARE: &str = r#"{"type":"response.completed","response":{"id":"resp_1","object":"response","model":"grok-4.7","status":"completed","output":[]}}"#;

/// What the mock upstream does after reading the request.
#[derive(Clone, Copy)]
enum Upstream {
    /// Never answers.
    Hang,
    /// Sends the SSE headers, `response.created` and a text delta, then waits forever.
    /// (The Responses framer hands a frame to the client once the next line arrives.)
    Partial,
    /// Sends what `Partial` sends, then drops the connection mid-body.
    Reset,
    /// A complete SSE body ending in `response.completed` with this payload.
    Complete(&'static str),
}

fn chunk(data: &str) -> String {
    format!("{:x}\r\n{data}\r\n", data.len())
}

fn sse(event: &str) -> String {
    chunk(&format!("data: {event}\n\n"))
}

const SSE_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";

/// Reads one HTTP/1.1 request (headers and Content-Length body).
async fn read_request(conn: &mut TcpStream) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = conn.read(&mut tmp).await.unwrap();
        assert!(n > 0, "upstream connection closed before the request was read");
        buf.extend_from_slice(&tmp[..n]);
        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .map_or(0, |v| v.trim().parse().unwrap());
        if buf.len() >= end + 4 + length {
            return;
        }
    }
}

/// One mock upstream connection per call; `ready` fires once the mock has answered as
/// far as it is going to before the client leaves.
async fn mock(behaviour: Upstream) -> (String, mpsc::UnboundedReceiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (ready, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let (mut conn, _) = listener.accept().await.unwrap();
            let ready = ready.clone();
            tokio::spawn(async move {
                read_request(&mut conn).await;
                match behaviour {
                    Upstream::Hang => {}
                    Upstream::Partial | Upstream::Reset => {
                        let head = format!("{SSE_HEAD}{}{}", sse(CREATED), sse(DELTA));
                        conn.write_all(head.as_bytes()).await.unwrap();
                        conn.flush().await.unwrap();
                    }
                    Upstream::Complete(completed) => {
                        let body = format!("{SSE_HEAD}{}{}0\r\n\r\n", sse(CREATED), sse(completed));
                        conn.write_all(body.as_bytes()).await.unwrap();
                    }
                }
                let _ = ready.send(());
                if let Upstream::Reset = behaviour {
                    drop(conn);
                    return;
                }
                // Keep the connection open until the test runtime ends.
                std::future::pending::<()>().await;
                drop(conn);
            });
        }
    });
    (addr, rx)
}

struct Proxy {
    addr: String,
    rt: Arc<cpa_server::Runtime>,
    ready: mpsc::UnboundedReceiver<()>,
}

async fn proxy(behaviour: Upstream) -> Proxy {
    let (upstream, ready) = mock(behaviour).await;
    // A private auth-dir: without one, credential loading reads the real ~/.cli-proxy-api.
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let auth_dir = std::env::temp_dir().join(format!("cpa-xai-cancel-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&auth_dir).unwrap();
    let config = Config::parse(&format!(
        "auth-dir: {}\naccess:\n  api-keys: [client-key]\napi-keys:\n  xai:\n    - name: xai-1\n      base-url: http://{upstream}/v1\n      keys:\n        - api-key: sk-fake-xai\n",
        auth_dir.display()
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::load(&config);
    let executors = Executors {
        claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(config, credentials, executors));
    cpa_server::install_registry(&rt);
    rt.usage_queue().configure(
        true,
        &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let app = cpa_server::router(rt.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Proxy { addr, rt, ready }
}

/// Opens a client connection and sends a `/v1/responses` request.
async fn request(addr: &str, stream: bool) -> TcpStream {
    let body = format!(r#"{{"model":"grok-4.7","input":"hi","stream":{stream}}}"#);
    let mut conn = TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "POST /v1/responses HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer client-key\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    conn.write_all(head.as_bytes()).await.unwrap();
    conn
}

/// Waits for usage records, then for any late extra record.
async fn records(rt: &cpa_server::Runtime, want: usize) -> Vec<Value> {
    let mut got = Vec::new();
    for _ in 0..300 {
        got.extend(rt.usage_queue().pop_oldest(10));
        if got.len() >= want && want > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    got.extend(rt.usage_queue().pop_oldest(10));
    got.iter().map(|r| serde_json::from_slice(r).unwrap()).collect()
}

fn outcome(record: &Value) -> (bool, i64, String, i64) {
    (
        record["failed"].as_bool().unwrap(),
        record["fail"]["status_code"].as_i64().unwrap(),
        record["fail"]["body"].as_str().unwrap().to_owned(),
        record["tokens"]["total_tokens"].as_i64().unwrap(),
    )
}

/// The client leaves while the executor waits for the upstream response headers.
#[tokio::test]
async fn cancel_while_awaiting_headers_records_the_failure() {
    let mut p = proxy(Upstream::Hang).await;
    let conn = request(&p.addr, true).await;
    p.ready.recv().await.unwrap();
    // The executor is now inside the upstream send.
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(conn);
    let got = records(&p.rt, 1).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(outcome(&got[0]), (true, 499, "context canceled".into(), 0));
}

/// The client leaves while the stream waits for the next upstream line, after the
/// first event reached it.
#[tokio::test]
async fn cancel_while_awaiting_stream_data_records_the_failure() {
    let mut p = proxy(Upstream::Partial).await;
    let mut conn = request(&p.addr, true).await;
    p.ready.recv().await.unwrap();
    let mut seen = Vec::new();
    let mut tmp = [0u8; 4096];
    while !String::from_utf8_lossy(&seen).contains("response.created") {
        let n = tokio::time::timeout(Duration::from_secs(5), conn.read(&mut tmp))
            .await
            .expect("first event reaches the client")
            .unwrap();
        assert!(n > 0, "stream ended early: {}", String::from_utf8_lossy(&seen));
        seen.extend_from_slice(&tmp[..n]);
    }
    drop(conn);
    let got = records(&p.rt, 1).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(outcome(&got[0]), (true, 499, "context canceled".into(), 0));
}

/// A non-streaming client leaves while the executor reads the upstream body.
#[tokio::test]
async fn cancel_while_reading_buffered_body_records_the_failure() {
    let mut p = proxy(Upstream::Partial).await;
    let conn = request(&p.addr, false).await;
    p.ready.recv().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(conn);
    let got = records(&p.rt, 1).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(outcome(&got[0]), (true, 499, "context canceled".into(), 0));
}

/// A stream that already failed keeps its own failure when the client then leaves.
#[tokio::test]
async fn error_then_drop_keeps_the_stream_failure() {
    let mut p = proxy(Upstream::Reset).await;
    let mut conn = request(&p.addr, true).await;
    p.ready.recv().await.unwrap();
    let mut out = Vec::new();
    conn.read_to_end(&mut out).await.unwrap();
    drop(conn);
    let got = records(&p.rt, 1).await;
    assert_eq!(got.len(), 1, "{got:?}");
    let (failed, _, body, _) = outcome(&got[0]);
    assert!(failed, "{got:?}");
    assert_ne!(body, "context canceled", "{got:?}");
}

/// A completed stream keeps its usage when the client then leaves.
#[tokio::test]
async fn completion_then_drop_keeps_the_usage() {
    let mut p = proxy(Upstream::Complete(COMPLETED_USAGE)).await;
    let mut conn = request(&p.addr, true).await;
    p.ready.recv().await.unwrap();
    let mut out = Vec::new();
    conn.read_to_end(&mut out).await.unwrap();
    assert!(String::from_utf8_lossy(&out).contains("response.completed"));
    drop(conn);
    let got = records(&p.rt, 1).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(outcome(&got[0]), (false, 200, String::new(), 10));
}

/// A completed stream without usage still publishes nothing (no `EnsurePublished`).
#[tokio::test]
async fn usage_less_completion_records_nothing() {
    let mut p = proxy(Upstream::Complete(COMPLETED_BARE)).await;
    let mut conn = request(&p.addr, true).await;
    p.ready.recv().await.unwrap();
    let mut out = Vec::new();
    conn.read_to_end(&mut out).await.unwrap();
    assert!(String::from_utf8_lossy(&out).contains("response.completed"));
    drop(conn);
    let got = records(&p.rt, 0).await;
    assert!(got.is_empty(), "{got:?}");
}
