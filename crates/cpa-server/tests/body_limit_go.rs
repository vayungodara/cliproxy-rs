//! `server.max-request-body-bytes` against answers recorded from Go v8.0.35
//! (tests/reference/body_limit/main.go -> tests/fixtures/body_limit_go.json). Each
//! scenario runs through the binary's middleware stack in-process, with no credentials,
//! so accepted bodies stop at the model lookup.
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use axum::body::{Body, Bytes, HttpBody};
use base64::Engine as _;
use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::management::{self, Management};
use http_body::{Frame, SizeHint};
use serde_json::Value;
use tower::ServiceExt as _;

/// The client sends this many bytes per frame.
const FRAME: usize = 16;

/// A request body that counts what the server pulled from it.
struct Counted {
    rest: Bytes,
    read: Arc<AtomicU64>,
    declared: bool,
}

impl HttpBody for Counted {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.rest.is_empty() {
            return Poll::Ready(None);
        }
        let n = self.rest.len().min(FRAME);
        let chunk = self.rest.split_to(n);
        self.read.fetch_add(n as u64, Ordering::Relaxed);
        Poll::Ready(Some(Ok(Frame::data(chunk))))
    }

    fn is_end_stream(&self) -> bool {
        self.rest.is_empty()
    }

    fn size_hint(&self) -> SizeHint {
        if self.declared {
            SizeHint::with_exact(self.rest.len() as u64)
        } else {
            SizeHint::default()
        }
    }
}

fn app(dir: &std::path::Path, yaml: &str, plugin_auth: bool) -> (axum::Router, Arc<cpa_server::Runtime>) {
    std::fs::create_dir_all(dir.join("auth")).unwrap();
    let path = dir.join("config.yaml");
    std::fs::write(&path, format!("{yaml}auth-dir: {}\n", dir.join("auth").display())).unwrap();
    let rt = Arc::new(cpa_server::testing::runtime(
        Config::load(&path).unwrap(),
        vec![],
        Executors {
            claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        },
    ));
    if plugin_auth {
        cpa_server::testing::unloaded_frontend_auth(&rt);
    }
    let state = Management::with_options(rt.clone(), path, management::Options::default());
    // crates/cliproxy/src/main.rs, without Home, observability and the heap trimmer.
    let app = cpa_server::app(rt.clone(), management::router(state.clone()));
    let app = cpa_server::safe_mode::router(&rt, app);
    let app = app.layer(axum::middleware::from_fn_with_state(rt.clone(), management::cors));
    let app = cpa_server::request_logging::router(&state, app);
    (cpa_server::body_limit::layer(&rt, app), rt)
}

/// Go answers a failed `/openai/v1/videos` request with a random video ID.
fn normalize(body: &str) -> String {
    match serde_json::from_str::<Value>(body) {
        Ok(mut v) if v["object"] == "video" => {
            v["id"] = "video_<id>".into();
            v.to_string()
        }
        Ok(v) => v.to_string(),
        Err(_) => body.to_owned(),
    }
}

const STATUS_ONLY: &[&str] = &[
    "management_chunked PUT /v0/management/debug",
    "management_chunked PUT /v0/management/config.yaml",
    "management_chunked PUT /v0/management/api-keys",
    // The answer carries a per-run bcrypt hash and temporary path; Go reads no body.
    "management_chunked GET /v8/management/config",
];

#[tokio::test]
async fn body_limit_answers_match_go() {
    let fixture: Vec<Value> = serde_json::from_str(include_str!("fixtures/body_limit_go.json")).unwrap();
    let mut failures = Vec::new();
    for (i, case) in fixture.iter().enumerate() {
        let name = case["name"].as_str().unwrap();
        let dir = std::env::temp_dir().join(format!("cpa-body-limit-{}-{i}", std::process::id()));
        let (app, _) = app(&dir, case["yaml"].as_str().unwrap(), case["plugin_auth"] == true);
        let mut raw = base64::engine::general_purpose::STANDARD
            .decode(case["body_b64"].as_str().unwrap())
            .unwrap();
        if let Some(pad) = case["pad_to"].as_u64() {
            raw.resize(pad as usize, b'p');
        }
        let read = Arc::new(AtomicU64::new(0));
        let chunked = case["chunked"].as_bool().unwrap();
        let mut request = axum::http::Request::builder()
            .method(case["method"].as_str().unwrap())
            .uri(case["path"].as_str().unwrap());
        if !chunked {
            request = request.header("content-length", raw.len());
        }
        for (k, v) in case["headers"].as_object().into_iter().flatten() {
            request = request.header(k, v.as_str().unwrap());
        }
        let len = raw.len() as u64;
        let mut request = request
            .body(Body::new(Counted {
                rest: Bytes::from(raw),
                read: read.clone(),
                declared: !chunked,
            }))
            .unwrap();
        let peer: SocketAddr = "192.0.2.1:1234".parse().unwrap();
        request.extensions_mut().insert(axum::extract::ConnectInfo(peer));
        let response = app.oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8_lossy(&body).into_owned();
        let got = (status, content_type, normalize(&body));
        let want = (
            case["status"].as_u64().unwrap() as u16,
            case["content_type"].as_str().unwrap().to_owned(),
            normalize(case["response"].as_str().unwrap()),
        );
        // Handlers dd7b4334 left alone read their body through axum's extractor, whose
        // failure is a plain-text 400 where gin's handlers write their own JSON 400
        // (docs/DIFFERENCES-FROM-GO.md).
        let status_only = STATUS_ONLY.contains(&name);
        if status_only && got.0 != want.0 || !status_only && got != want {
            failures.push(format!("{name}: got {got:?}, want {want:?}"));
        }
        // Go reads at most the limit plus one byte; frames arrive whole here.
        let (go_read, rs_read) = (case["read"].as_u64().unwrap(), read.load(Ordering::Relaxed));
        let bounded = if go_read == 0 {
            rs_read == 0
        } else if go_read < len {
            rs_read < go_read + FRAME as u64
        } else {
            true
        };
        if !bounded {
            failures.push(format!("{name}: read {rs_read} of {len} bytes, Go read {go_read}"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        fixture.len(),
        failures.join("\n")
    );
}

/// Go reads the limit from the current config on every request.
#[tokio::test]
async fn a_published_limit_applies_to_the_next_request() {
    let dir = std::env::temp_dir().join(format!("cpa-body-limit-reload-{}", std::process::id()));
    let yaml = "config-version: 8\naccess:\n  api-keys:\n    - test-key\n";
    let (app, rt) = app(&dir, yaml, false);
    let send = |app: axum::Router| async move {
        let request = axum::http::Request::post("/v1/responses")
            .header("authorization", "Bearer test-key")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"model":"gpt-5","input":"ok"}"#))
            .unwrap();
        app.oneshot(request).await.unwrap().status().as_u16()
    };
    assert_eq!(send(app.clone()).await, 400, "no limit: the model lookup answers");
    let path = dir.join("config.yaml");
    // Keeps the private auth-dir app() wrote.
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{text}server:\n  max-request-body-bytes: 29\n")).unwrap();
    rt.publish_config(Config::load(&path).unwrap());
    assert_eq!(send(app).await, 413);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Go stores the middleware's limit on the request: a reload while the body is still
/// arriving does not change the limit its decoding applies.
#[tokio::test]
async fn a_reload_during_a_request_keeps_its_limit() {
    use futures_util::StreamExt as _;
    let dir = std::env::temp_dir().join(format!("cpa-body-limit-mid-{}", std::process::id()));
    let yaml = "config-version: 8\nserver:\n  max-request-body-bytes: 128\naccess:\n  api-keys:\n    - test-key\n";
    let (app, rt) = app(&dir, yaml, false);
    let decoded = format!(r#"{{"model":"gpt-5","input":"{}"}}"#, "z".repeat(512));
    let compressed = zstd::encode_all(decoded.as_bytes(), 3).unwrap();
    assert!(compressed.len() < 128);
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(1);
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|chunk| (Ok::<_, std::io::Error>(chunk), rx))
    });
    let request = axum::http::Request::post("/v1/responses")
        .header("authorization", "Bearer test-key")
        .header("content-type", "application/json")
        .header("content-encoding", "zstd")
        .body(Body::from_stream(stream.boxed()))
        .unwrap();
    let response = tokio::spawn(app.oneshot(request));
    let (head, tail) = compressed.split_at(compressed.len() / 2);
    tx.send(Bytes::copy_from_slice(head)).await.unwrap();
    // The handler has taken the first half, so the middleware has run.
    while tx.capacity() < tx.max_capacity() {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let path = dir.join("config.yaml");
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("max-request-body-bytes: 128", "max-request-body-bytes: 0");
    std::fs::write(&path, text).unwrap();
    rt.publish_config(Config::load(&path).unwrap());
    assert_eq!(rt.config().max_request_body_bytes(), None);
    tx.send(Bytes::copy_from_slice(tail)).await.unwrap();
    drop(tx);
    let response = response.await.unwrap().unwrap();
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
        (status, String::from_utf8_lossy(&body).as_ref()),
        (
            413,
            r#"{"error":{"message":"Invalid request: http: request body too large","type":"invalid_request_error"}}"#
        )
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Go checks a v8 config write's `If-Match` before it reads the body: a stale write
/// fails at once, even while the body is still arriving.
#[tokio::test]
async fn a_stale_config_write_fails_before_its_body_is_read() {
    struct Open(Arc<AtomicU64>);
    impl HttpBody for Open {
        type Data = Bytes;
        type Error = std::io::Error;
        fn poll_frame(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
            // One byte, then the stream stays open.
            if self.0.fetch_add(1, Ordering::Relaxed) == 0 {
                Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"{")))))
            } else {
                Poll::Pending
            }
        }
    }
    // Go's ConfigV8 order: lock, read the file, If-Match, parse the stored config, then
    // read the body. Each case answers before the pending body is polled.
    let non_utf8: &[u8] = b"config-version: 8\nserver: \xff\n";
    // Name, the stored file written after the runtime loads, the condition header, status.
    type Case = (
        &'static str,
        Option<&'static [u8]>,
        Option<(&'static str, &'static str)>,
        u16,
    );
    let cases: [Case; 4] = [
        ("stale", None, Some(("if-match", "\"definitely-stale\"")), 412),
        ("malformed", Some(b"server: ["), None, 500),
        (
            "malformed, If-None-Match",
            Some(b"server: ["),
            Some(("if-none-match", "*")),
            500,
        ),
        (
            "non-UTF-8, stale",
            Some(non_utf8),
            Some(("if-match", "\"definitely-stale\"")),
            412,
        ),
    ];
    for limit in ["32", "0"] {
        for (name, stored, condition, status) in cases {
            let dir = std::env::temp_dir().join(format!("cpa-body-limit-stale-{limit}-{}", uuid::Uuid::new_v4()));
            let yaml = format!(
                "config-version: 8\nserver:\n  max-request-body-bytes: {limit}\nmanagement:\n  allow-remote: true\n  secret-key: mgmt-secret\n"
            );
            let (app, _) = app(&dir, &yaml, false);
            // Corrupted after the runtime loaded it.
            if let Some(bytes) = stored {
                std::fs::write(dir.join("config.yaml"), bytes).unwrap();
            }
            let file = std::fs::read(dir.join("config.yaml")).unwrap();
            let polls = Arc::new(AtomicU64::new(0));
            let mut request = axum::http::Request::put("/v8/management/config")
                .header("authorization", "Bearer mgmt-secret")
                .header("content-type", "application/json");
            if let Some((key, value)) = condition {
                request = request.header(key, value);
            }
            let mut request = request.body(Body::new(Open(polls.clone()))).unwrap();
            let peer: SocketAddr = "192.0.2.1:1234".parse().unwrap();
            request.extensions_mut().insert(axum::extract::ConnectInfo(peer));
            let response = tokio::time::timeout(std::time::Duration::from_secs(5), app.oneshot(request))
                .await
                .unwrap_or_else(|_| panic!("limit {limit}, {name}: waited for the body"))
                .unwrap();
            assert_eq!(response.status().as_u16(), status, "limit {limit}, {name}");
            assert_eq!(
                polls.load(Ordering::Relaxed),
                0,
                "limit {limit}, {name}: the body was polled"
            );
            if status == 412 {
                // The ETag hashes the raw file bytes, UTF-8 or not.
                use sha2::Digest as _;
                let hex: String = sha2::Sha256::digest(&file).iter().map(|b| format!("{b:02x}")).collect();
                assert_eq!(response.headers()["etag"], format!("\"{hex}\"").as_str(), "{name}");
            } else {
                let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["error"], "invalid_config", "limit {limit}, {name}");
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
