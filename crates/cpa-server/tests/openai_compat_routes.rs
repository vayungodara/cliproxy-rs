//! OpenAI-compatible providers through the real router, runtime, registry overlay and
//! executor, against a local mock upstream. The executor's byte-level behaviour is
//! checked against Go in cpa-exec (openai_compat_tests.rs); this proves the wiring:
//! config credentials, alias routing, client-key isolation and both response modes.

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::router;

/// Requests seen, and the compact failure to answer with (`None` answers `{"ok":true}`).
#[derive(Default)]
struct Seen(
    Mutex<Vec<(String, Option<String>, String)>>,
    Mutex<Option<(u16, &'static str)>>,
);

const CHAT: &str = r#"{"id":"c1","object":"chat.completion","created":1,"model":"up-model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;
const CHUNK: &str = r#"{"id":"c1","object":"chat.completion.chunk","created":1,"model":"up-model","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}"#;

async fn upstream(State(seen): State<Arc<Seen>>, req: Request) -> Response {
    let path = req.uri().path().to_owned();
    let auth = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    let stream = body.contains(r#""include_usage":true"#);
    seen.0.lock().unwrap().push((path.clone(), auth, body));
    if path.ends_with("/responses/compact") {
        return match *seen.1.lock().unwrap() {
            Some((status, body)) => (
                axum::http::StatusCode::from_u16(status).unwrap(),
                [("content-type", "application/json")],
                body,
            )
                .into_response(),
            None => ([("content-type", "application/json")], r#"{"ok":true}"#).into_response(),
        };
    }
    if stream {
        let sse = format!("data: {CHUNK}\n\ndata: [DONE]\n\n");
        ([("content-type", "text/event-stream")], Bytes::from(sse)).into_response()
    } else {
        ([("content-type", "application/json")], CHAT).into_response()
    }
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

async fn proxy() -> (String, Arc<Seen>) {
    proxy_keys(&["sk-fake-upstream"]).await
}

async fn proxy_keys(keys: &[&str]) -> (String, Arc<Seen>) {
    let keys: String = keys.iter().map(|k| format!("        - api-key: {k}\n")).collect();
    let seen = Arc::new(Seen::default());
    let upstream_url = serve(axum::Router::new().fallback(upstream).with_state(seen.clone())).await;
    let config = Config::parse(&format!(
        "access:\n  api-keys: [client-key]\napi-keys:\n  openai-compatibility:\n    - name: Acme\n      base-url: {upstream_url}/v1\n      models:\n        - name: up-model\n          alias: fast\n      keys:\n{keys}"
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
    (serve(router(rt)).await, seen)
}

async fn post(url: &str, body: &str) -> (u16, String, String) {
    let res = wreq::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header("authorization", "Bearer client-key")
        .header("content-type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .unwrap();
    let status = res.status().as_u16();
    let content_type = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    (status, content_type, res.text().await.unwrap())
}

#[tokio::test]
async fn alias_routes_to_configured_upstream_with_its_key() {
    let (url, seen) = proxy().await;
    let (status, content_type, text) =
        post(&url, r#"{"model":"fast","messages":[{"role":"user","content":"hi"}]}"#).await;
    assert_eq!(status, 200, "{text}");
    assert!(content_type.starts_with("application/json"), "{content_type}");
    assert_eq!(text, CHAT, "OpenAI-to-OpenAI non-stream responses pass through");
    let seen = seen.0.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    let (path, auth, body) = &seen[0];
    assert_eq!(path, "/v1/chat/completions");
    assert_eq!(
        auth.as_deref(),
        Some("Bearer sk-fake-upstream"),
        "the client key never reaches the upstream"
    );
    assert_eq!(
        body,
        r#"{"model":"up-model","messages":[{"role":"user","content":"hi"}]}"#
    );
}

#[tokio::test]
async fn streaming_requests_usage_and_ends_with_done() {
    let (url, seen) = proxy().await;
    let (status, content_type, text) = post(
        &url,
        r#"{"model":"fast","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(status, 200, "{text}");
    assert!(content_type.starts_with("text/event-stream"), "{content_type}");
    assert!(text.contains(&format!("data: {CHUNK}\n\n")), "{text}");
    assert!(text.trim_end().ends_with("data: [DONE]"), "{text}");
    let body = seen.0.lock().unwrap()[0].2.clone();
    assert_eq!(
        body,
        r#"{"model":"up-model","stream":true,"messages":[{"role":"user","content":"hi"}],"stream_options":{"include_usage":true}}"#
    );
}

async fn send(url: &str, path: &str, headers: &[(&str, &str)], body: Vec<u8>) -> (u16, String) {
    let mut req = wreq::Client::new()
        .post(format!("{url}{path}"))
        .header("authorization", "Bearer client-key")
        .header("content-type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = req.body(body).send().await.unwrap();
    (res.status().as_u16(), res.text().await.unwrap())
}

fn compact_calls(seen: &Seen) -> usize {
    seen.0
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.0.ends_with("/responses/compact"))
        .count()
}

/// Go `TestOpenAIResponsesCompactDecodesZstdRequestBody` (openai_responses_compact_test.go)
/// and `handlers.ReadRequestBody`.
#[tokio::test]
async fn compact_decodes_zstd_request_body() {
    let (url, seen) = proxy().await;
    let compressed = zstd::stream::encode_all(&br#"{"model":"fast","input":"hello"}"#[..], 0).unwrap();
    let (status, text) = send(
        &url,
        "/v1/responses/compact",
        &[("content-encoding", "zstd")],
        compressed,
    )
    .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(text, r#"{"ok":true}"#);
    assert_eq!(compact_calls(&seen), 1);
    let body = seen.0.lock().unwrap()[0].2.clone();
    assert!(
        body.contains(r#""input":"hello""#) && body.contains(r#""model":"up-model""#),
        "{body}"
    );

    // ReadRequestBody: an undecodable body that is valid JSON is used as sent; one
    // that is not answers 400 with the decoder's reason.
    let (status, text) = send(
        &url,
        "/v1/chat/completions",
        &[("content-encoding", "gzip")],
        br#"{"model":"fast","messages":[{"role":"user","content":"hi"}]}"#.to_vec(),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let (status, text) = send(
        &url,
        "/v1/chat/completions",
        &[("content-encoding", "br")],
        b"\x1f\x8b".to_vec(),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(
        text,
        r#"{"error":{"message":"Invalid request: unsupported request content encoding: br","type":"invalid_request_error"}}"#
    );
}

/// Go `TestOpenAIResponsesCompactTransientFailureDoesNotCooldownAuthAndPreservesError`:
/// a 500 on every credential keeps the upstream status and message, and cools nothing.
#[tokio::test]
async fn compact_transient_failure_keeps_error_and_cools_nothing() {
    let (url, seen) = proxy_keys(&["sk-fake-a", "sk-fake-b"]).await;
    *seen.1.lock().unwrap() = Some((
        500,
        r#"{"error":{"message":"compact upstream temporary error","type":"api_error"}}"#,
    ));
    let body = br#"{"model":"fast","input":"hello"}"#.to_vec();
    let (status, text) = send(&url, "/v1/responses/compact", &[], body.clone()).await;
    assert_eq!(status, 500, "{text}");
    assert!(text.contains("compact upstream temporary error"), "{text}");
    assert_eq!(compact_calls(&seen), 2, "a transient fault fails over");
    let (status, text) = send(&url, "/v1/responses", &[], body).await;
    assert_eq!(status, 200, "normal traffic is not cooled: {text}");
}

/// Go `TestOpenAIResponsesCompactRequestFaultStopsFallbackAndPreservesError`.
#[tokio::test]
async fn compact_request_fault_stops_fallback() {
    let (url, seen) = proxy_keys(&["sk-fake-a", "sk-fake-b"]).await;
    *seen.1.lock().unwrap() = Some((404, "404 page not found"));
    let body = br#"{"model":"fast","input":"hello"}"#.to_vec();
    let (status, text) = send(&url, "/v1/responses/compact", &[], body.clone()).await;
    assert_eq!(status, 404, "{text}");
    assert!(text.contains("404 page not found"), "{text}");
    assert_eq!(compact_calls(&seen), 1, "a request fault stops failover");
    let (status, text) = send(&url, "/v1/responses", &[], body).await;
    assert_eq!(status, 200, "normal traffic is not cooled: {text}");
}
