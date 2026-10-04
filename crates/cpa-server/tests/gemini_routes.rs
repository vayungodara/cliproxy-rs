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
use cpa_server::router;

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
    let credentials = cpa_core::config::credentials::from_config(&config);
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
    let credentials = cpa_core::config::credentials::from_config(&config);
    let executors = Executors {
        claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(config, credentials, executors));
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

/// Replays one scripted upstream answer (status, headers, body) to every request.
async fn scripted(reply: &serde_json::Value) -> String {
    let status = reply["status"].as_u64().unwrap() as u16;
    let headers: Vec<(String, String)> = reply["headers"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|h| (h[0].as_str().unwrap().to_owned(), h[1].as_str().unwrap().to_owned()))
        .collect();
    let body = reply["body"].as_str().unwrap().to_owned();
    let app = axum::Router::new().fallback(move || {
        let (headers, body) = (headers.clone(), body.clone());
        async move {
            let mut response = Response::new(axum::body::Body::from(body));
            *response.status_mut() = axum::http::StatusCode::from_u16(status).unwrap();
            for (name, value) in headers {
                response.headers_mut().append(
                    axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    value.parse().unwrap(),
                );
            }
            response
        }
    });
    serve(app).await
}

/// The `usage_*` scenarios of the Go executor fixtures through the real router: the
/// record the usage queue stores must carry the tokens, response model and translated
/// reasoning effort Go's `UsageReporter` published for the same upstream answer. The
/// executors report upstream payloads (`ExecRequest::usage`); without them the server
/// would parse the translated client response instead.
#[tokio::test]
async fn usage_records_match_go() {
    let fixtures = [
        include_str!("../../cpa-exec/tests/fixtures/gemini_go.json"),
        include_str!("../../cpa-exec/tests/fixtures/vertex_go.json"),
    ];
    let mut ran = 0;
    for fixture in fixtures {
        let fixture: serde_json::Value = serde_json::from_str(fixture).unwrap();
        for s in fixture["scenarios"].as_array().unwrap() {
            let name = s["name"].as_str().unwrap();
            if !name.starts_with("usage_") {
                continue;
            }
            let reply = s.get("upstream").unwrap_or_else(|| &s["replies"][0]);
            let up = scripted(reply).await;
            let config = format!(
                "access:\n  api-keys: [client-key]\n{}",
                s["config"]
                    .as_str()
                    .unwrap()
                    .replace("UPSTREAM", up.trim_start_matches("http://"))
            );
            let config = Config::parse(&config).unwrap();
            let credentials = cpa_core::config::credentials::from_config(&config);
            let executors = Executors {
                claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            };
            let rt = Arc::new(cpa_server::testing::runtime(config, credentials, executors));
            let queue = rt.usage_queue();
            queue.configure(
                true,
                &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
            );
            let url = serve(router(rt.clone())).await;
            let path = match s["source"].as_str().unwrap() {
                "openai" => "/v1/chat/completions",
                "claude" => "/v1/messages",
                "openai-response" => "/v1/responses",
                other => panic!("{name}: no route for {other}"),
            };
            let (status, _, text) = post(&url, path, s["payload"].as_str().unwrap()).await;
            assert_eq!(status, 200, "{name}: {text}");
            let mut records = Vec::new();
            for _ in 0..50 {
                records = queue.pop_oldest(10);
                if !records.is_empty() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(records.len(), 1, "{name}: one attempt");
            let got: serde_json::Value = serde_json::from_slice(&records[0]).unwrap();
            let want = &s["usage"];
            for field in [
                "input_tokens",
                "output_tokens",
                "reasoning_tokens",
                "cached_tokens",
                "cache_read_tokens",
                "cache_creation_tokens",
                "total_tokens",
            ] {
                assert_eq!(got["tokens"][field], want[field], "{name}: {field}");
            }
            let text = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_owned();
            assert_eq!(
                text(&got["response_model"]),
                text(&want["response_model"]),
                "{name}: response_model"
            );
            assert_eq!(got["failed"], want["failed"], "{name}: failed");
            // Go's Interactions paths never set a translated effort: the record keeps the
            // conductor's context value (the client's effort), which the executor-level
            // generator does not bind. The Gemini and Vertex paths replace it.
            if s["provider"].as_str() != Some("gemini-interactions") {
                assert_eq!(
                    text(&got["reasoning_effort"]),
                    text(&want["reasoning_effort"]),
                    "{name}: reasoning_effort"
                );
            }
            ran += 1;
        }
    }
    assert_eq!(ran, 7, "usage scenarios in the Go fixtures");
}

/// With session affinity, a Gemini body nested far deeper than any conversation is
/// served without its message prefix (the server does not overflow its stack), while a
/// plain one gets an LCP session.
#[tokio::test]
async fn deeply_nested_body_skips_prefix_matching() {
    let seen = Arc::new(Seen::default());
    let up = serve(axum::Router::new().fallback(upstream).with_state(seen.clone())).await;
    // A private auth-dir: without one, credential loading reads the real ~/.cli-proxy-api.
    let auth_dir = std::env::temp_dir().join(format!("cpa-gemini-deep-auths-{}", std::process::id()));
    std::fs::create_dir_all(&auth_dir).unwrap();
    let config = Config::parse(&format!(
        "auth-dir: {}\naccess:\n  api-keys: [client-key]\nrouting:\n  session-affinity: true\napi-keys:\n  gemini:\n    - base-url: {up}/\n      headers:\n        X-Session: \"s-$CPA-SESSION-ID\"\n      models:\n        - name: gemini-2.5-flash\n          alias: flash\n      keys:\n        - api-key: AIza-fake-upstream\n",
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
    let url = serve(router(rt)).await;
    // Deep enough to overflow unbounded extraction, shallow enough for the derived ID.
    let levels = 1000;
    let deep = format!(
        r#"{{"contents":[{{"role":"user","parts":{}{{"text":"hi"}}{}}}]}}"#,
        "[".repeat(levels),
        "]".repeat(levels)
    );
    let (status, _, text) = post(&url, "/v1beta/models/flash:generateContent", &deep).await;
    assert_eq!(status, 200, "{text}");
    let (status, _, text) = post(&url, "/v1beta/models/flash:generateContent", HI).await;
    assert_eq!(status, 200, "{text}");
    let sessions: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.4.clone().unwrap_or_default())
        .collect();
    assert_eq!(sessions.len(), 2);
    assert!(!sessions[0].starts_with("s-lcp:"), "{sessions:?}");
    assert!(sessions[1].starts_with("s-lcp:v1:"), "{sessions:?}");
}
