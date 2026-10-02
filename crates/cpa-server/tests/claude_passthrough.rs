//! End-to-end: client -> cliproxy-rs -> mock Anthropic upstream.

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use cpa_core::config::Config;
use cpa_server::{AppState, router};

const SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n\
                   event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

#[derive(Clone, Debug)]
struct Seen {
    uri: String,
    authorization: String,
    beta: String,
    client_key_leaked: bool,
    body: Bytes,
}

type Log = Arc<Mutex<Vec<Seen>>>;

async fn upstream(State(log): State<Log>, req: Request) -> axum::response::Response {
    let (parts, body) = req.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let header = |n: &str| parts.headers.get(n).map(|v| v.to_str().unwrap().to_owned()).unwrap_or_default();
    log.lock().unwrap().push(Seen {
        uri: parts.uri.to_string(),
        authorization: header("authorization"),
        beta: header("anthropic-beta"),
        client_key_leaked: parts.headers.values().any(|v| v.as_bytes() == b"client-key-1"),
        body: body.clone(),
    });
    if body.windows(10).any(|w| w == b"RATE_LIMIT") {
        return (StatusCode::TOO_MANY_REQUESTS, [("content-type", "application/json")], r#"{"type":"error"}"#)
            .into_response();
    }
    ([("content-type", "text/event-stream"), ("request-id", "req_123"), ("x-secret", "nope")], SSE).into_response()
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn auth_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("cpa-test-{}-{:?}", std::process::id(), std::thread::current().id()));
    std::fs::create_dir_all(&dir).unwrap();
    let write = |name: &str, json: &str| std::fs::write(dir.join(name), json).unwrap();
    write("claude-a@example.com.json", r#"{"type":"claude","access_token":"tok-A","email":"a@example.com"}"#);
    write("claude-b@example.com.json", r#"{"type":"claude","access_token":"tok-B","email":"b@example.com"}"#);
    write("codex-c.json", r#"{"type":"codex","access_token":"tok-CODEX"}"#);
    write("broken.json", "{not json");
    write("notes.txt", "ignored");
    dir
}

#[tokio::test]
async fn claude_messages_passthrough() {
    let log: Log = Arc::default();
    let upstream_url = serve(axum::Router::new().fallback(upstream).with_state(log.clone())).await;
    let config = Config {
        host: String::new(),
        port: 0,
        api_keys: vec!["client-key-1".into()],
        auth_dir: auth_dir(),
    };
    let state = AppState::load(config, &upstream_url).unwrap();
    assert_eq!(state.claude_credentials().len(), 2, "codex, broken and non-json files must be skipped");
    let proxy = serve(router(Arc::new(state))).await;
    let client = wreq::Client::new();
    let url = format!("{proxy}/v1/messages");
    // Non-canonical JSON (spacing, key order) proves the body is forwarded untouched.
    let body = r#"{ "model":"claude-opus-5-5","stream":true,  "messages":[{"role":"user","content":"hi"}] }"#;

    let missing = client.post(&url).body(body).send().await.unwrap();
    assert_eq!(missing.status().as_u16(), 401);
    assert_eq!(missing.text().await.unwrap(), r#"{"error":"Missing API key"}"#);

    let wrong = client.post(&url).header("x-api-key", "nope").body(body).send().await.unwrap();
    assert_eq!(wrong.status().as_u16(), 401);
    assert_eq!(wrong.text().await.unwrap(), r#"{"error":"Invalid API key"}"#);
    assert!(log.lock().unwrap().is_empty(), "rejected requests must not reach upstream");

    for auth in ["x-api-key", "authorization"] {
        let value = if auth == "authorization" { "Bearer client-key-1" } else { "client-key-1" };
        let res = client.post(&url).header(auth, value).body(body).send().await.unwrap();
        assert_eq!(res.status().as_u16(), 200);
        assert_eq!(res.headers()["content-type"], "text/event-stream");
        assert_eq!(res.headers()["request-id"], "req_123");
        assert!(res.headers().get("x-secret").is_none());
        assert_eq!(res.text().await.unwrap(), SSE);
    }

    let seen = log.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    let mut tokens: Vec<_> = seen.iter().map(|s| s.authorization.as_str()).collect();
    tokens.sort();
    assert_eq!(tokens, ["Bearer tok-A", "Bearer tok-B"], "round-robin over Claude credentials only");
    for s in &seen {
        assert_eq!(s.uri, "/v1/messages?beta=true");
        assert_eq!(s.body, body.as_bytes());
        assert!(s.beta.split(',').any(|b| b == "oauth-2025-04-20"));
        assert!(!s.client_key_leaked, "client key must never reach upstream");
    }

    let limited = client
        .post(format!("{proxy}/v1/messages/count_tokens?key=client-key-1"))
        .body(r#"{"RATE_LIMIT":1}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(limited.status().as_u16(), 429);
    assert_eq!(limited.text().await.unwrap(), r#"{"type":"error"}"#);
    assert_eq!(log.lock().unwrap().last().unwrap().uri, "/v1/messages/count_tokens?beta=true");

    let models: serde_json::Value = client
        .get(format!("{proxy}/v1/models"))
        .header("x-api-key", "client-key-1")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(models["data"].as_array().unwrap().iter().any(|m| m["id"] == "claude-sonnet-5-5"));

    let health = client.get(format!("{proxy}/healthz")).send().await.unwrap();
    assert_eq!(health.text().await.unwrap(), r#"{"status":"ok"}"#);
}
