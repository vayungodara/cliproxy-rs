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
use cpa_server::{Runtime, router};

#[derive(Default)]
struct Seen(Mutex<Vec<(String, Option<String>, String)>>);

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
    seen.0.lock().unwrap().push((path, auth, body));
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
    let seen = Arc::new(Seen::default());
    let upstream_url = serve(axum::Router::new().fallback(upstream).with_state(seen.clone())).await;
    let config = Config::parse(&format!(
        "access:\n  api-keys: [client-key]\napi-keys:\n  openai-compatibility:\n    - name: Acme\n      base-url: {upstream_url}/v1\n      models:\n        - name: up-model\n          alias: fast\n      keys:\n        - api-key: sk-fake-upstream\n"
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::load(&config);
    let executors = Executors {
        claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        devices: Default::default(),
        openai: Default::default(),
    };
    let rt = Arc::new(Runtime::new(config, credentials, executors));
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
