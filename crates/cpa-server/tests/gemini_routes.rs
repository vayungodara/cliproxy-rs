//! Gemini API keys and native Interactions keys through the real router, runtime,
//! registry and executor, against a local mock upstream. Byte-level parity with Go is
//! checked in cpa-exec (gemini_tests.rs); this proves the wiring: config credentials,
//! alias routing, the `/v1beta` handlers writing the executor's stream bytes verbatim,
//! and the Interactions route reaching the Interactions credential.

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::{Runtime, router};

/// Path with query, the upstream API key header, the Api-Revision header and the body.
type Seen = Mutex<Vec<(String, Option<String>, Option<String>, String, Option<String>)>>;

const ANSWER: &str = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usageMetadata":{"totalTokenCount":4}}"#;
const CHUNK: &str =
    r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"he"}]}}],"usageMetadata":{"totalTokenCount":2}}"#;
const INTERACTION: &str = r#"{"id":"int_1","status":"completed","outputs":[{"type":"text","text":"ok"}]}"#;

async fn upstream(State(seen): State<Arc<Seen>>, req: Request) -> Response {
    let path = req.uri().path_and_query().map(|p| p.to_string()).unwrap_or_default();
    let (key, revision, session) = {
        let header = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
        (header("x-goog-api-key"), header("api-revision"), header("x-session"))
    };
    let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    seen.lock()
        .unwrap()
        .push((path.clone(), key, revision, body.clone(), session));
    if path.contains(":streamGenerateContent") {
        let sse = format!("data: {CHUNK}\n\ndata: {ANSWER}\n\n");
        return ([("content-type", "text/event-stream")], Bytes::from(sse)).into_response();
    }
    if path.contains(":countTokens") {
        return (
            [("content-type", "application/json")],
            r#"{"totalTokens":7,"promptTokensDetails":[{"modality":"TEXT","tokenCount":7}]}"#,
        )
            .into_response();
    }
    if path.ends_with("/interactions") {
        if body.contains(r#""stream":true"#) {
            let sse = "event: interaction.created\ndata: {\"event_type\":\"interaction.created\"}\n\n: ping\n\nevent: done\ndata: [DONE]\n\n";
            return ([("content-type", "text/event-stream")], sse).into_response();
        }
        return ([("content-type", "application/json")], INTERACTION).into_response();
    }
    ([("content-type", "application/json")], ANSWER).into_response()
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

async fn proxy() -> (String, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let up = serve(axum::Router::new().fallback(upstream).with_state(seen.clone())).await;
    let config = Config::parse(&format!(
        "access:\n  api-keys: [client-key]\napi-keys:\n  gemini:\n    - base-url: {up}/\n      headers:\n        X-Session: \"s-$CPA-SESSION-ID\"\n      models:\n        - name: gemini-2.5-flash\n          alias: flash\n      keys:\n        - api-key: AIza-fake-upstream\n  interactions:\n    - base-url: {up}\n      models:\n        - name: gemini-3-pro-preview\n          alias: native-pro\n      keys:\n        - api-key: AIza-fake-interactions\n"
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
    let rt = Arc::new(Runtime::new(config, credentials, executors));
    (serve(router(rt)).await, seen)
}

async fn post(url: &str, path: &str, body: &str) -> (u16, String, String) {
    let res = wreq::Client::new()
        .post(format!("{url}{path}"))
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

const HI: &str = r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#;

#[tokio::test]
async fn generate_content_routes_alias_to_gemini_key() {
    let (url, seen) = proxy().await;
    let (status, content_type, text) = post(&url, "/v1beta/models/flash:generateContent", HI).await;
    assert_eq!(status, 200, "{text}");
    assert!(content_type.starts_with("application/json"), "{content_type}");
    assert_eq!(text, ANSWER, "Gemini responses pass through");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    let (path, key, _, body, session) = &seen[0];
    assert_eq!(path, "/v1beta/models/gemini-2.5-flash:generateContent");
    // Without an explicit client session, $CPA-SESSION-ID is the derived canonical
    // session the conductor binds (Go syncMetadataSessionToContext).
    assert!(
        session.as_deref().is_some_and(|s| s.starts_with("s-derived:ctx:v1:")),
        "{session:?}"
    );
    assert_eq!(
        key.as_deref(),
        Some("AIza-fake-upstream"),
        "the client key never reaches the upstream"
    );
    assert!(body.contains(r#""model":"gemini-2.5-flash""#), "{body}");
}

#[tokio::test]
async fn stream_generate_content_writes_executor_bytes() {
    let (url, seen) = proxy().await;
    let (status, content_type, text) = post(&url, "/v1beta/models/flash:streamGenerateContent?alt=sse", HI).await;
    assert_eq!(status, 200, "{text}");
    assert!(content_type.starts_with("text/event-stream"), "{content_type}");
    // Non-terminal usage moves to cpaUsageMetadata; the terminal chunk keeps it.
    let first = CHUNK.replace(
        r#","usageMetadata":{"totalTokenCount":2}"#,
        r#","cpaUsageMetadata":{"totalTokenCount":2}"#,
    );
    assert_eq!(text, format!("data: {first}\n\ndata: {ANSWER}\n\n"));
    assert_eq!(
        seen.lock().unwrap()[0].0,
        "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
    );

    // With another `alt`, chunks are written bare and the upstream gets `$alt`.
    let (status, _, text) = post(&url, "/v1beta/models/flash:streamGenerateContent?alt=json", HI).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(text, format!("{first}{ANSWER}"));
    assert_eq!(
        seen.lock().unwrap()[1].0,
        "/v1beta/models/gemini-2.5-flash:streamGenerateContent?$alt=json"
    );
}

#[tokio::test]
async fn count_tokens_reaches_gemini_key() {
    let (url, seen) = proxy().await;
    let (status, _, text) = post(&url, "/v1beta/models/flash:countTokens", HI).await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains(r#""totalTokens":7"#), "{text}");
    assert_eq!(seen.lock().unwrap()[0].0, "/v1beta/models/gemini-2.5-flash:countTokens");
}

#[tokio::test]
async fn interactions_route_uses_interactions_key() {
    let (url, seen) = proxy().await;
    let (status, _, text) = post(&url, "/v1beta/interactions", r#"{"model":"native-pro","input":"hi"}"#).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(text, INTERACTION);
    let (path, key, revision, body, _) = seen.lock().unwrap()[0].clone();
    assert_eq!(path, "/v1beta/interactions");
    assert_eq!(key.as_deref(), Some("AIza-fake-interactions"));
    assert_eq!(revision.as_deref(), Some("2026-05-20"));
    assert_eq!(body, r#"{"model":"gemini-3-pro-preview","input":"hi"}"#);

    let (status, content_type, text) = post(
        &url,
        "/v1beta/interactions",
        r#"{"model":"native-pro","input":"hi","stream":true}"#,
    )
    .await;
    assert_eq!(status, 200, "{text}");
    assert!(content_type.starts_with("text/event-stream"), "{content_type}");
    assert_eq!(
        text,
        "event: interaction.created\ndata: {\"event_type\":\"interaction.created\"}\n\ndata: : ping\n\nevent: done\ndata: [DONE]\n\n"
    );
}

/// A Vertex API key through the router: alias routing to the key's models and the
/// `/v1/publishers/google/models` path under its base URL.
#[tokio::test]
async fn vertex_api_key_routes_alias() {
    let seen = Arc::new(Seen::default());
    let up = serve(axum::Router::new().fallback(upstream).with_state(seen.clone())).await;
    let config = Config::parse(&format!(
        "access:\n  api-keys: [client-key]\napi-keys:\n  vertex:\n    - base-url: {up}/api\n      models:\n        - name: gemini-2.5-flash\n          alias: vflash\n      keys:\n        - api-key: vk-fake\n"
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
    let rt = Arc::new(Runtime::new(config, credentials, executors));
    let url = serve(router(rt)).await;
    let (status, _, text) = post(&url, "/v1beta/models/vflash:generateContent", HI).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(text, ANSWER);
    let (path, key, _, body, _) = seen.lock().unwrap()[0].clone();
    assert_eq!(
        path,
        "/api/v1/publishers/google/models/gemini-2.5-flash:generateContent"
    );
    assert_eq!(key.as_deref(), Some("vk-fake"));
    assert!(body.contains(r#""model":"gemini-2.5-flash""#), "{body}");
}
