//! End-to-end: client -> cliproxy-rs -> mock Anthropic upstream.
//!
//! Expectations come from CLIProxyAPI's handlers (sdk/api/handlers/claude/code_handlers.go),
//! not from this implementation.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::{Runtime, router};

const SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n\
                   event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
const JSON_REPLY: &str = r#"{"id":"msg_1","type":"message","content":[]}"#;

#[derive(Clone, Debug)]
struct Seen {
    uri: String,
    authorization: String,
    beta: String,
    accept_encoding: String,
    client_key_leaked: bool,
    body: Bytes,
}

type Log = Arc<Mutex<Vec<Seen>>>;

async fn upstream(State(log): State<Log>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let header = |n: &str| {
        parts
            .headers
            .get(n)
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default()
    };
    log.lock().unwrap().push(Seen {
        uri: parts.uri.to_string(),
        authorization: header("authorization"),
        beta: header("anthropic-beta"),
        accept_encoding: header("accept-encoding"),
        client_key_leaked: parts.headers.values().any(|v| v.as_bytes() == b"client-key-1"),
        body: body.clone(),
    });
    let has = |marker: &[u8]| body.windows(marker.len()).any(|w| w == marker);
    if has(b"MODE_429") {
        let err = r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#;
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("content-type", "application/json"), ("retry-after", "7")],
            err,
        )
            .into_response();
    }
    if has(b"MODE_BREAK") {
        use futures_util::StreamExt;
        // The pause lets hyper flush the first event before the connection is cut.
        let chunks = futures_util::stream::once(async {
            Ok::<_, std::io::Error>(Bytes::from_static(b"event: message_start\ndata: {}\n\n"))
        })
        .chain(futures_util::stream::once(async {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            Err(std::io::Error::other("connection reset"))
        }));
        return ([("content-type", "text/event-stream")], Body::from_stream(chunks)).into_response();
    }
    let extra = [("request-id", "req_123"), ("x-secret", "nope")];
    if has(br#""stream":true"#) {
        ([("content-type", "text/event-stream")], extra, SSE).into_response()
    } else {
        ([("content-type", "application/json")], extra, JSON_REPLY).into_response()
    }
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn auth_dir(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cpa-it-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (file, json) in files {
        std::fs::write(dir.join(file), json).unwrap();
    }
    dir
}

async fn proxy(dir: &Path, upstream_url: &str) -> String {
    let mut config = Config::parse("").unwrap();
    config.api_keys = vec!["client-key-1".into()];
    config.auth_dir = dir.into();
    let creds = cpa_core::credential::load_dir(dir).unwrap();
    let executors = Executors {
        claude: ClaudeExecutor::new(upstream_url).unwrap(),
        codex: Default::default(),
        devices: Default::default(),
        openai: Default::default(),
    };
    let rt = Arc::new(Runtime::new(config, creds, executors));
    // This suite checks wire passthrough, one upstream attempt per request. Scheduler
    // failover/rounds have their own local-upstream integration suite.
    rt.publish_policy(cpa_server::scheduler::Policy {
        max_retry_credentials: 1,
        ..Default::default()
    });
    serve(router(rt)).await
}

#[tokio::test]
async fn claude_messages_end_to_end() {
    let log: Log = Arc::default();
    let upstream_url = serve(axum::Router::new().fallback(upstream).with_state(log.clone())).await;
    let dir = auth_dir(
        "main",
        &[
            (
                "claude-a.json",
                r#"{"type":"claude","access_token":"tok-A","email":"a@example.com"}"#,
            ),
            (
                "claude-b.json",
                r#"{"type":"claude","access_token":"tok-B","email":"b@example.com"}"#,
            ),
            (
                "claude-c.json",
                r#"{"type":"claude","access_token":"tok-DISABLED","disabled":true}"#,
            ),
            ("codex-d.json", r#"{"type":"codex","access_token":"tok-CODEX"}"#),
            ("broken.json", "{not json"),
            ("notes.txt", "ignored"),
        ],
    );
    let proxy = proxy(&dir, &upstream_url).await;
    let client = wreq::Client::new();
    let url = format!("{proxy}/v1/messages");
    let post = |body: &'static str| client.post(&url).header("x-api-key", "client-key-1").body(body).send();

    // Client auth (sdk/access errors).
    let missing = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(missing.status().as_u16(), 401);
    assert_eq!(missing.text().await.unwrap(), r#"{"error":"Missing API key"}"#);
    let wrong = client
        .post(format!("{url}?key=nope&key=client-key-1"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(
        wrong.text().await.unwrap(),
        r#"{"error":"Invalid API key"}"#,
        "first query value wins"
    );
    assert!(
        log.lock().unwrap().is_empty(),
        "rejected requests must not reach upstream"
    );

    // Non-streaming: JSON path, body forwarded byte for byte, upstream headers dropped.
    let json_body = r#"{ "model":"claude-opus-5-5",  "messages":[{"role":"user","content":"hi"}] }"#;
    let res = post(json_body).await.unwrap();
    assert_eq!(res.status().as_u16(), 200);
    assert_eq!(res.headers()["content-type"], "application/json");
    assert!(res.headers().get("request-id").is_none() && res.headers().get("x-secret").is_none());
    assert_eq!(res.text().await.unwrap(), JSON_REPLY);

    // Streaming: SSE headers set by the proxy, events intact.
    let stream_body = r#"{"model":"claude-opus-5-5","stream":true,"messages":[]}"#;
    let res = client
        .post(&url)
        .header("authorization", "Bearer client-key-1")
        .body(stream_body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 200);
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    assert_eq!(res.headers()["cache-control"], "no-cache");
    assert!(res.headers().get("request-id").is_none());
    assert_eq!(res.text().await.unwrap(), SSE);

    // Upstream error before streaming: Claude error JSON and status. Go sends Retry-After
    // only for its own scheduler/cooldown errors, never a raw upstream one.
    let res = post(r#"{"model":"claude-opus-5-5","stream":true,"x":"MODE_429"}"#)
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 429);
    assert!(res.headers().get("retry-after").is_none());
    assert_eq!(
        res.text().await.unwrap(),
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
    );

    // Upstream dies mid-stream: events so far, then a terminal error event.
    let res = post(r#"{"model":"claude-opus-5-5","stream":true,"x":"MODE_BREAK"}"#)
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 200);
    let text = res.text().await.unwrap();
    let (first, rest) = text.split_at("event: message_start\ndata: {}\n\n".len());
    assert_eq!(first, "event: message_start\ndata: {}\n\n");
    assert!(
        rest.starts_with(
            r#"event: error
data: {"type":"error","error":{"type":"api_error","message":"unexpected EOF"#
        ),
        "got {rest:?}"
    );
    assert!(rest.ends_with("\n\n"));

    let seen = log.lock().unwrap().clone();
    assert_eq!(seen.len(), 4);
    let mut tokens: Vec<_> = seen.iter().map(|s| s.authorization.as_str()).collect();
    tokens.sort();
    tokens.dedup();
    assert_eq!(
        tokens,
        ["Bearer tok-A", "Bearer tok-B"],
        "disabled and non-Claude credentials are never used"
    );
    // These tokens are not Claude OAuth tokens (no sk-ant-oat) and have no fingerprint
    // profile, so Go keeps the caller-owned shape: no CLI betas, and the body gets only
    // Go's normalization (max_tokens from the catalog, a default cache breakpoint on the
    // last turn, an explicit stream flag). Streams to custom gateways ask for identity.
    for s in &seen {
        assert_eq!(s.uri, "/v1/messages?beta=true");
        assert!(
            s.beta.is_empty(),
            "caller-owned shape carries no managed betas: {}",
            s.beta
        );
        let streaming = s.body.windows(13).any(|w| w == br#""stream":true"#);
        let expected = if streaming {
            "identity"
        } else {
            "gzip, deflate, br, zstd"
        };
        assert_eq!(s.accept_encoding, expected);
        assert!(!s.client_key_leaked, "client key must never reach upstream");
    }
    let max_tokens = cpa_core::registry::pinned()
        .channel("claude")
        .iter()
        .find(|m| m.id == "claude-opus-5-5")
        .and_then(|m| m.raw.get("max_completion_tokens").and_then(serde_json::Value::as_i64))
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&seen[0].body),
        format!(
            r#"{{ "model":"claude-opus-5-5",  "messages":[{{"role":"user","content":[{{"type":"text","text":"hi","cache_control":{{"type":"ephemeral"}}}}]}}] ,"max_tokens":{max_tokens},"stream":false}}"#
        ),
        "untouched members keep their bytes; Go's normalizations are appended in place"
    );

    let models: serde_json::Value = client
        .get(format!("{proxy}/v1/models"))
        .header("x-api-key", "client-key-1")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        models["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "claude-sonnet-5-5")
    );
    let health = client.get(format!("{proxy}/healthz")).send().await.unwrap();
    assert_eq!(health.text().await.unwrap(), r#"{"status":"ok"}"#);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn unregistered_and_unserved_models_follow_go_error_contracts() {
    let dir = auth_dir(
        "empty",
        &[
            (
                "claude-off.json",
                r#"{"type":"claude","access_token":"t","disabled":true}"#,
            ),
            ("xai-a.json", r#"{"type":"xai","access_token":"fake-xai"}"#),
        ],
    );
    let proxy = proxy(&dir, "http://127.0.0.1:9").await;
    let post = |body: &'static str| {
        wreq::Client::new()
            .post(format!("{proxy}/v1/messages"))
            .header("x-api-key", "client-key-1")
            .body(body)
            .send()
    };
    // Go (differential case disabled-credential): a disabled credential registers no
    // models, so the model has no provider: 400, not auth_not_found.
    let res = post(r#"{"model":"claude-opus-5"}"#).await.unwrap();
    assert_eq!(res.status().as_u16(), 400);
    assert_eq!(
        res.text().await.unwrap(),
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"unknown provider for model claude-opus-5"}}"#
    );
    // A registered model whose provider has no executor yet is auth_not_found (Go skips
    // auths whose executor is not registered).
    let res = post(r#"{"model":"grok-4.7"}"#).await.unwrap();
    assert_eq!(res.status().as_u16(), 503);
    assert_eq!(
        res.text().await.unwrap(),
        r#"{"type":"error","error":{"type":"api_error","message":"auth_not_found: no auth available (providers=xai, model=grok-4.7)"}}"#
    );
    std::fs::remove_dir_all(dir).unwrap();
}
