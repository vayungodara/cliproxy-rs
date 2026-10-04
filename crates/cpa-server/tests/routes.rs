//! Route surface through the real runtime, Claude executor and a local mock upstream.
//! Expected shapes come from the Go handlers they port (cited per assertion); the Go
//! binary comparison for the same routes lives in harness/fixtures.py.

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use cpa_core::config::Config;
use cpa_core::credential::{Credential, Source};
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::{Runtime, router};
use futures_util::StreamExt;
use serde_json::Value;

const MODEL: &str = "claude-sonnet-4-6";

fn event(kind: &str, data: Value) -> String {
    format!("event: {kind}\ndata: {data}\n\n")
}

fn sse() -> String {
    let message = serde_json::json!({"id":"msg_1","type":"message","role":"assistant","model":MODEL,"content":[],
        "stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":0}});
    [
        event(
            "message_start",
            serde_json::json!({"type":"message_start","message":message}),
        ),
        event(
            "content_block_start",
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        ),
        event(
            "content_block_delta",
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}),
        ),
        event(
            "content_block_stop",
            serde_json::json!({"type":"content_block_stop","index":0}),
        ),
        event(
            "message_delta",
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
        ),
        event("message_stop", serde_json::json!({"type":"message_stop"})),
    ]
    .concat()
}

#[derive(Default)]
struct Seen {
    requests: Mutex<Vec<(String, Value)>>,
    /// Signalled when a `fake-held-<token>` request reached the mock.
    arrived: tokio::sync::Notify,
    /// The held request answers like `fake-<token>` once this is signalled.
    release: tokio::sync::Notify,
}

async fn upstream(State(seen): State<Arc<Seen>>, req: Request) -> Response {
    let mut token = req.headers()["authorization"].to_str().unwrap().to_owned();
    let bytes = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    seen.requests.lock().unwrap().push((token.clone(), body.clone()));
    if let Some(rest) = token.strip_prefix("Bearer fake-held-") {
        seen.arrived.notify_one();
        seen.release.notified().await;
        token = format!("Bearer fake-{rest}");
    }
    match token.as_str() {
        "Bearer fake-fail" => (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response(),
        "Bearer fake-hdr" => (
            [
                ("content-type", "application/json; charset=utf-8"),
                ("x-upstream-id", "u1"),
                ("x-litellm-model", "m"),
                ("set-cookie", "s=1"),
            ],
            serde_json::json!({"id":"msg_1","type":"message","role":"assistant","model":body["model"],
                "content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","stop_sequence":null,
                "usage":{"input_tokens":5,"output_tokens":1}})
            .to_string(),
        )
            .into_response(),
        "Bearer fake-hdr-bad" => (
            StatusCode::BAD_REQUEST,
            [("content-type", "application/json"), ("x-upstream-id", "e1")],
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad input"}}"#,
        )
            .into_response(),
        "Bearer fake-bad" => (
            StatusCode::BAD_REQUEST,
            [("content-type", "application/json")],
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad input"}}"#,
        )
            .into_response(),
        "Bearer fake-quota" => (
            StatusCode::TOO_MANY_REQUESTS,
            [("content-type", "application/json"), ("retry-after", "30")],
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
        )
            .into_response(),
        "Bearer fake-break" => {
            use futures_util::StreamExt;
            let start: String = sse().split_inclusive("\n\n").take(3).collect();
            // The delay lets the first events reach the proxy before the reset.
            let broken = futures_util::stream::iter([Ok(Bytes::from(start))])
                .chain(futures_util::stream::once(async {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    Err(std::io::Error::other("reset"))
                }))
                .boxed();
            (
                [("content-type", "text/event-stream")],
                axum::body::Body::from_stream(broken),
            )
                .into_response()
        }
        _ if body["stream"] == Value::Bool(true) => ([("content-type", "text/event-stream")], sse()).into_response(),
        _ => (
            [("content-type", "application/json")],
            serde_json::json!({"id":"msg_1","type":"message","role":"assistant","model":body["model"],
                "content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","stop_sequence":null,
                "usage":{"input_tokens":5,"output_tokens":1}})
            .to_string(),
        )
            .into_response(),
    }
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

fn oauth(id: &str, token: &str, extra: Value) -> Credential {
    let mut metadata = extra.as_object().unwrap().clone();
    metadata.insert("type".into(), "claude".into());
    metadata.insert("access_token".into(), token.into());
    metadata.insert("expired".into(), "2099-01-01T00:00:00Z".into());
    Credential::from_file(Path::new("/fake"), &Path::new("/fake").join(id), metadata).unwrap()
}

struct Proxy {
    url: String,
    seen: Arc<Seen>,
    rt: Arc<Runtime>,
}

async fn proxy(config: &str, credentials: Vec<Credential>) -> Proxy {
    let seen = Arc::new(Seen::default());
    let upstream_url = serve(axum::Router::new().fallback(upstream).with_state(seen.clone())).await;
    let mut credentials = credentials;
    for c in &mut credentials {
        c.attributes.insert("base_url".into(), upstream_url.clone());
    }
    let config = Config::parse(&format!("access:\n  api-keys: [client-key]\n{config}")).unwrap();
    let executors = Executors {
        claude: ClaudeExecutor::new(&upstream_url).unwrap(),
        codex: Default::default(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(config, credentials, executors));
    let url = serve(router(rt.clone())).await;
    Proxy { url, seen, rt }
}

async fn post(url: &str, path: &str, body: &str) -> (u16, wreq::header::HeaderMap, String) {
    let res = wreq::Client::new()
        .post(format!("{url}{path}"))
        .header("authorization", "Bearer client-key")
        .body(body.to_owned())
        .send()
        .await
        .unwrap();
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    (status, headers, res.text().await.unwrap())
}

#[tokio::test]
async fn chat_completions_translate_and_stream_upstream_for_non_stream_clients() {
    let p = proxy("", vec![oauth("a.json", "fake-ok", serde_json::json!({}))]).await;
    let body = format!(r#"{{"model":"{MODEL}","messages":[{{"role":"user","content":"hello"}}]}}"#);
    let (status, headers, text) = post(&p.url, "/v1/chat/completions", &body).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(headers["content-type"], "application/json");
    let reply: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(reply["object"], "chat.completion");
    assert_eq!(reply["choices"][0]["message"]["content"], "hi");
    // Go claude_executor_execute.go: a translated non-stream request streams upstream.
    let seen = p.seen.requests.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].1["stream"], true);
    assert_eq!(seen[0].1["messages"][0]["role"], "user");
    // Go cpa_trace.go: selection time, auth index and a UUIDv7 request ID.
    let trace = headers["x-cpa-trace-id"].to_str().unwrap().to_owned();
    let parts: Vec<&str> = trace.splitn(3, '-').collect();
    assert_eq!(parts[0].len(), 14);
    assert_eq!(parts[1].len(), 16);
    assert_eq!(parts[2].as_bytes()[14], b'7', "UUIDv7: {trace}");

    // Streaming: OpenAI frames and a closing [DONE] (openai_handlers.go WriteDone).
    let stream_body =
        format!(r#"{{"model":"{MODEL}","stream":true,"messages":[{{"role":"user","content":"hello"}}]}}"#);
    let (status, headers, text) = post(&p.url, "/v1/chat/completions", &stream_body).await;
    assert_eq!(status, 200);
    assert_eq!(headers["content-type"], "text/event-stream");
    assert_eq!(headers["connection"], "keep-alive");
    assert!(text.starts_with("data: {"), "{text}");
    assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
    assert!(text.contains(r#""content":"hi""#));
}

/// Go `StartNonStreamingKeepAlive` (M2-0036), expectations from the Go golden
/// `nonstream_keepalive` (tests/reference/server/main.go): a result within one interval
/// is untouched; a slower one commits 200 `application/json` with a `\n` per interval,
/// then the rendered body, even when it is an error.
///
/// Synchronisation decides every outcome, never a wall-clock margin: the fast cases use
/// an interval that cannot elapse during the test, and a held mock answers only after
/// the test has read the first keep-alive newline. Which calls run the keep-alive at all
/// is `dispatch::tests::nonstream_keepalive_only_for_nonstream_generate`.
#[tokio::test]
async fn nonstream_keepalive_commits_like_go() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/server_go.json")).unwrap();
    let golden = |name: &str| {
        fixture["nonstream_keepalive"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .unwrap()
            .clone()
    };
    // Splits the keep-alive newlines from the rendered body.
    let split = |text: &str| {
        let rest = text.trim_start_matches('\n');
        (text.len() - rest.len(), rest.to_owned())
    };
    let body = format!(r#"{{"model":"{MODEL}","max_tokens":5,"messages":[{{"role":"user","content":"hi"}}]}}"#);

    // A result before the first tick is answered as without a keep-alive.
    let never = "requests:\n  nonstream-keepalive-interval: 86400\n";
    for (token, case) in [("fake-ok", "fast_ok"), ("fake-bad", "fast_error")] {
        let want = golden(case);
        let p = proxy(never, vec![oauth("a.json", token, serde_json::json!({}))]).await;
        let (status, headers, text) = post(&p.url, "/v1/messages", &body).await;
        assert_eq!(u64::from(status), want["status"].as_u64().unwrap(), "{text}");
        assert_eq!(headers["content-type"], want["content_type"].as_str().unwrap());
        assert!(headers.get("content-length").is_some(), "{headers:?}");
        assert_eq!(split(&text).0, 0, "{text:?}");
        if case == "fast_error" {
            assert_eq!(text, want["body"].as_str().unwrap());
        }
    }

    // A result after the first tick: the mock holds its reply until the client has read
    // a keep-alive newline, so the commit always precedes the result.
    let every_second = "requests:\n  nonstream-keepalive-interval: 1\n";
    for (token, path, case) in [
        ("fake-held-ok", "/v1/chat/completions", "slow_ok"),
        ("fake-held-bad", "/v1/messages", "slow_error"),
    ] {
        let want = golden(case);
        let p = proxy(every_second, vec![oauth("a.json", token, serde_json::json!({}))]).await;
        let res = wreq::Client::new()
            .post(format!("{}{path}", p.url))
            .header("authorization", "Bearer client-key")
            .body(body.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(u64::from(res.status().as_u16()), want["status"].as_u64().unwrap());
        let headers = res.headers().clone();
        assert_eq!(headers["content-type"], want["content_type"].as_str().unwrap());
        assert!(headers.get("content-length").is_none(), "{headers:?}");
        let mut chunks = res.bytes_stream();
        let mut text = Vec::new();
        while text.is_empty() {
            text.extend_from_slice(&chunks.next().await.unwrap().unwrap());
        }
        assert!(text.iter().all(|&b| b == b'\n'), "{:?}", String::from_utf8_lossy(&text));
        p.seen.release.notify_one();
        while let Some(chunk) = chunks.next().await {
            text.extend_from_slice(&chunk.unwrap());
        }
        let text = String::from_utf8(text).unwrap();
        let (newlines, rest) = split(&text);
        assert!(newlines >= 1, "{text:?}");
        let (want_newlines, want_rest) = split(want["body"].as_str().unwrap());
        assert_eq!(want_newlines, 1);
        if case == "slow_error" {
            assert_eq!(rest, want_rest);
        } else {
            let reply: Value = serde_json::from_str(&rest).unwrap();
            assert_eq!(reply["choices"][0]["message"]["content"], "hi");
        }
    }
}

/// gin without `HandleMethodNotAllowed` (Go `engine.handleHTTPRequest`): a known
/// path's unregistered method and an unregistered HEAD run NoRoute, whose handler
/// aborts with a bare 404; only `/healthz` registers HEAD. The Go binary comparison is
/// harness/fixtures.py `wrong-method-chat`, `models-head`, `healthz-post`.
#[tokio::test]
async fn wrong_methods_are_gin_no_route() {
    let p = proxy("", vec![oauth("a.json", "fake-ok", serde_json::json!({}))]).await;
    let client = wreq::Client::new();
    for (method, path) in [
        (wreq::Method::GET, "/v1/chat/completions"),
        (wreq::Method::PUT, "/v1/models"),
        (wreq::Method::DELETE, "/v1/messages"),
        (wreq::Method::HEAD, "/v1/models"),
        (wreq::Method::HEAD, "/"),
        (wreq::Method::POST, "/healthz"),
        (wreq::Method::GET, "/v2/nothing"),
    ] {
        let res = client
            .request(method.clone(), format!("{}{path}", p.url))
            .header("authorization", "Bearer client-key")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status().as_u16(), 404, "{method} {path}");
        assert!(
            res.headers().get("allow").is_none(),
            "{method} {path}: {:?}",
            res.headers()
        );
        assert!(res.headers().get("content-type").is_none(), "{method} {path}");
        assert_eq!(res.text().await.unwrap(), "", "{method} {path}");
    }
    // NoRoute runs no group middleware: no client key is asked for.
    for (method, path) in [
        (wreq::Method::GET, "/v2/nothing"),
        (wreq::Method::GET, "/v1/chat/completions"),
    ] {
        let res = client
            .request(method.clone(), format!("{}{path}", p.url))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status().as_u16(), 404, "{method} {path} without a key");
    }
    let res = client.get(format!("{}/v1/models", p.url)).send().await.unwrap();
    assert_eq!(res.status().as_u16(), 401, "matched routes still require the key");
    for (method, path) in [(wreq::Method::HEAD, "/healthz"), (wreq::Method::GET, "/v1/models")] {
        let res = client
            .request(method.clone(), format!("{}{path}", p.url))
            .header("authorization", "Bearer client-key")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status().as_u16(), 200, "{method} {path}");
    }
}

/// `requests.passthrough-headers` (Go `downstreamHeadersFromExecutor`,
/// `WriteUpstreamHeaders`, `WriteErrorResponse` addon): off by default; on, filtered
/// upstream headers fill what the handler did not set, and an error's upstream
/// headers come along. Filter expectations are the Go golden `upstream_headers`.
#[tokio::test]
async fn passthrough_headers_follow_go() {
    let body = format!(r#"{{"model":"{MODEL}","max_tokens":5,"messages":[{{"role":"user","content":"hi"}}]}}"#);
    for enabled in [false, true] {
        let config = format!("requests:\n  passthrough-headers: {enabled}\n");
        let ok = proxy(&config, vec![oauth("a.json", "fake-hdr", serde_json::json!({}))]).await;
        let (status, headers, text) = post(&ok.url, "/v1/messages", &body).await;
        assert_eq!(status, 200, "{text}");
        assert_eq!(
            headers["content-type"], "application/json",
            "the handler's own header wins"
        );
        assert_eq!(headers.get("x-upstream-id").is_some(), enabled, "{headers:?}");
        if enabled {
            assert_eq!(headers["x-upstream-id"], "u1");
        }
        assert!(headers.get("x-litellm-model").is_none() && headers.get("set-cookie").is_none());

        let bad = proxy(&config, vec![oauth("a.json", "fake-hdr-bad", serde_json::json!({}))]).await;
        let (status, headers, _) = post(&bad.url, "/v1/messages", &body).await;
        assert_eq!(status, 400);
        assert_eq!(headers.get("x-upstream-id").is_some(), enabled, "{headers:?}");
    }
}

#[tokio::test]
async fn legacy_completions_convert_both_ways() {
    let p = proxy("", vec![oauth("a.json", "fake-ok", serde_json::json!({}))]).await;
    let body = format!(r#"{{"model":"{MODEL}","prompt":"say hi","max_tokens":9}}"#);
    let (status, _, text) = post(&p.url, "/v1/completions", &body).await;
    assert_eq!(status, 200, "{text}");
    let reply: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(reply["object"], "text_completion");
    assert_eq!(reply["choices"][0]["text"], "hi");
    let seen = p.seen.requests.lock().unwrap().clone();
    assert_eq!(seen[0].1["messages"][0]["content"][0]["text"], "say hi");
    assert_eq!(seen[0].1["max_tokens"], 9);
    let (_, _, text) = post(
        &p.url,
        "/v1/completions",
        &body.replace("\"max_tokens\"", "\"stream\":true,\"max_tokens\""),
    )
    .await;
    assert!(
        text.contains(r#""object":"text_completion""#) && text.ends_with("data: [DONE]\n\n"),
        "{text}"
    );
}

#[tokio::test]
async fn failover_stop_rules_and_cooldown_contracts() {
    // A 500 fails over to the next credential (round-robin starts at a.json).
    let p = proxy(
        "",
        vec![
            oauth("a.json", "fake-fail", serde_json::json!({})),
            oauth("b.json", "fake-ok", serde_json::json!({})),
        ],
    )
    .await;
    let body = format!(r#"{{"model":"{MODEL}","max_tokens":5,"messages":[]}}"#);
    let (status, _, text) = post(&p.url, "/v1/messages", &body).await;
    assert_eq!(status, 200, "{text}");
    let tokens: Vec<String> = p.seen.requests.lock().unwrap().iter().map(|r| r.0.clone()).collect();
    assert_eq!(tokens, ["Bearer fake-fail", "Bearer fake-ok"]);

    // A request fault (Go IsRequestFault: invalid_request_error) never fails over.
    let p = proxy(
        "",
        vec![
            oauth("a.json", "fake-bad", serde_json::json!({})),
            oauth("b.json", "fake-ok", serde_json::json!({})),
        ],
    )
    .await;
    let (status, _, text) = post(
        &p.url,
        "/v1/chat/completions",
        &format!(r#"{{"model":"{MODEL}","messages":[]}}"#),
    )
    .await;
    assert_eq!(status, 400);
    // BuildErrorResponseBody passes the upstream JSON through.
    assert_eq!(
        text,
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad input"}}"#
    );
    assert_eq!(p.seen.requests.lock().unwrap().len(), 1);

    // Quota: the 429 cools the only credential; the next request is a model cooldown
    // with Retry-After (selector.go modelCooldownError, SafeResponseHeaders).
    let p = proxy("", vec![oauth("a.json", "fake-quota", serde_json::json!({}))]).await;
    let (status, headers, text) = post(
        &p.url,
        "/v1/chat/completions",
        &format!(r#"{{"model":"{MODEL}","messages":[]}}"#),
    )
    .await;
    assert_eq!(status, 429);
    assert!(
        headers.get("retry-after").is_none(),
        "raw upstream 429s carry no Retry-After"
    );
    assert!(text.contains("slow down"), "{text}");
    let (status, headers, text) = post(
        &p.url,
        "/v1/chat/completions",
        &format!(r#"{{"model":"{MODEL}","messages":[]}}"#),
    )
    .await;
    assert_eq!(status, 429);
    let retry: u64 = headers["retry-after"].to_str().unwrap().parse().unwrap();
    // The Claude executor adds its quota fuzz on top of the upstream 30s.
    assert!(retry >= 30, "{retry}");
    let cooldown: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(cooldown["error"]["code"], "model_cooldown");
    assert_eq!(cooldown["error"]["provider"], "claude");
    assert_eq!(cooldown["error"]["last_upstream_error"], "rate_limit_error: slow down");
    assert!(
        text.starts_with(r#"{"error":{"code":"model_cooldown","last_upstream_error""#),
        "sorted keys: {text}"
    );
    let (status, _, text) = post(
        &p.url,
        "/v1/messages",
        &format!(r#"{{"model":"{MODEL}","messages":[]}}"#),
    )
    .await;
    assert_eq!(status, 429);
    assert!(text.starts_with(r#"{"type":"error","error":{"type":"rate_limit_error","message":"All credentials for model claude-sonnet-4-6 are cooling down via provider claude"#), "{text}");
    assert_eq!(
        p.seen.requests.lock().unwrap().len(),
        1,
        "cooling credentials are not retried"
    );
}

#[tokio::test]
async fn mid_stream_failure_ends_with_route_error_frame() {
    let p = proxy("", vec![oauth("a.json", "fake-break", serde_json::json!({}))]).await;
    let body = format!(r#"{{"model":"{MODEL}","stream":true,"messages":[{{"role":"user","content":"x"}}]}}"#);
    let (status, _, text) = post(&p.url, "/v1/chat/completions", &body).await;
    assert_eq!(status, 200, "{text}");
    let last = text.trim_end().rsplit("\n\n").next().unwrap();
    assert!(last.starts_with(r#"data: {"error":{"#), "{text}");
    assert!(!text.contains("[DONE]"), "terminal errors replace [DONE]");
}

#[tokio::test]
async fn config_models_alias_and_force_mapping_reach_upstream_and_client() {
    let mut key = Credential {
        id: "claude:apikey:1".into(),
        provider: "claude".into(),
        source: Source::Config {
            section: "claude-api-key".into(),
            index: 0,
        },
        disabled: false,
        label: "claude-apikey".into(),
        attributes: [("api_key", "fake-ok"), ("auth_kind", "apikey")]
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect(),
        metadata: serde_json::json!({"access_token":"fake-ok","models":[{"name":MODEL,"alias":"friendly","force-mapping":true}]})
            .as_object()
            .unwrap()
            .clone(),
        revision: 0,
    };
    key.attributes.insert("prefix".into(), "team".into());
    let p = proxy("", vec![key]).await;
    // Chat Completions so the request translator writes the resolved model; the Claude
    // identity path does not rewrite `model` yet (Go's registry fallback does).
    let (status, _, text) = post(
        &p.url,
        "/v1/chat/completions",
        r#"{"model":"team/friendly","messages":[{"role":"user","content":"x"}]}"#,
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let seen = p.seen.requests.lock().unwrap().clone();
    assert_eq!(seen[0].1["model"], MODEL, "alias resolved to the upstream name");
    let reply: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(reply["model"], "friendly", "force-mapping rewrites the response model");
    // The registry lists the alias and its prefixed copy, not the upstream name.
    let models: Value = wreq::Client::new()
        .get(format!("{}/v1/models", p.url))
        .header("authorization", "Bearer client-key")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["friendly", "team/friendly"]);
    // Unregistered names are a 400 before any upstream call.
    let (status, _, _) = post(
        &p.url,
        "/v1/messages",
        &format!(r#"{{"model":"{MODEL}","messages":[]}}"#),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(p.rt.store().stats().success, 1);
}

#[tokio::test]
async fn misc_routes_match_go() {
    let p = proxy("", vec![oauth("a.json", "fake-ok", serde_json::json!({}))]).await;
    let client = wreq::Client::new();
    let head = client.head(format!("{}/healthz", p.url)).send().await.unwrap();
    assert_eq!(head.status().as_u16(), 200);
    assert_eq!(head.text().await.unwrap(), "");
    let root = client
        .get(format!("{}/", p.url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        root,
        r#"{"endpoints":["POST /v1/chat/completions","POST /v1/completions","GET /v1/models"],"message":"CLI Proxy API Server"}"#
    );
    let cb = client
        .get(format!("{}/anthropic/callback?code=c&state=s", p.url))
        .send()
        .await
        .unwrap();
    assert_eq!(cb.headers()["content-type"], "text/html; charset=utf-8");
    let devin = client.get(format!("{}/devin/callback", p.url)).send().await.unwrap();
    assert_eq!(devin.status().as_u16(), 400);
    assert_eq!(devin.headers()["cache-control"], "no-store");
    assert_eq!(devin.text().await.unwrap(), r#"{"error":"code or error is required"}"#);
    // A pending login receives the callback.
    let got = Arc::new(Mutex::new(None));
    let sink = got.clone();
    p.rt.set_oauth_callback_sink(Some(Arc::new(move |cb: &cpa_server::runtime::OAuthCallback| {
        *sink.lock().unwrap() = Some((cb.provider, cb.state.clone(), cb.code.clone(), cb.error.clone()));
        true
    })));
    let callback = |path: &'static str| client.get(format!("{}{path}", p.url)).send();
    let ok = callback("/callback?code=%20c1%20&state=s1").await.unwrap();
    assert_eq!(ok.status().as_u16(), 200);
    assert_eq!(
        *got.lock().unwrap(),
        Some(("devin", "s1".to_owned(), "c1".to_owned(), String::new()))
    );
    // Go's Devin handler trims `error` before falling back to `error_description`, so a
    // blank error still reports the denial (server_routes.go devinCallbackHandler).
    let denied = callback("/callback?state=s2&error=%20&error_description=access_denied")
        .await
        .unwrap();
    assert_eq!(denied.status().as_u16(), 200);
    assert_eq!(
        *got.lock().unwrap(),
        Some(("devin", "s2".to_owned(), String::new(), "access_denied".to_owned()))
    );
    // The other providers' handlers read `error` untrimmed: a blank one wins.
    let blank = callback("/anthropic/callback?state=s3&error=%20&error_description=access_denied")
        .await
        .unwrap();
    assert_eq!(blank.status().as_u16(), 200);
    assert_eq!(
        *got.lock().unwrap(),
        Some(("anthropic", "s3".to_owned(), String::new(), " ".to_owned()))
    );
    // Interactions validation (interactions_handlers.go).
    let (status, _, text) = post(&p.url, "/v1beta/interactions", r#"{"model":"a","agent":"b"}"#).await;
    assert_eq!(
        (status, text.as_str()),
        (
            400,
            r#"{"error":{"message":"request requires exactly one of model or agent","type":"invalid_request_error"}}"#
        )
    );
    // Gemini action parsing (gemini_handlers.go).
    let (status, _, text) = post(&p.url, "/v1beta/models/x", "{}").await;
    assert_eq!(
        (status, text.as_str()),
        (
            404,
            r#"{"error":{"message":"/v1beta/models/x not found.","type":"invalid_request_error"}}"#
        )
    );
    let (status, _, text) = post(&p.url, &format!("/v1beta/models/{MODEL}:embed"), "{}").await;
    assert_eq!((status, text.as_str()), (200, ""));
    // Compact through Claude: the executor's 501 is a request fault, no failover.
    let (status, _, text) = post(
        &p.url,
        "/v1/responses/compact",
        &format!(r#"{{"model":"{MODEL}","input":"x","stream":false}}"#),
    )
    .await;
    assert_eq!(status, 501);
    assert_eq!(
        text,
        r#"{"error":{"message":"/responses/compact not supported","type":"server_error","code":"internal_server_error"}}"#
    );
    assert!(p.seen.requests.lock().unwrap().is_empty());
}

#[derive(Default)]
struct OAuthMock {
    calls: Mutex<Vec<String>>,
}

async fn oauth_upstream(State(mock): State<Arc<OAuthMock>>, req: Request) -> Response {
    let path = req.uri().path().to_owned();
    let auth = req
        .headers()
        .get("authorization")
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    mock.calls.lock().unwrap().push(format!("{path} {auth}"));
    match (path.as_str(), auth.as_str()) {
        ("/token", _) => axum::Json(serde_json::json!({
            "access_token": "sk-ant-oat-new-fake", "refresh_token": "fake-rotated", "expires_in": 3600,
            "account": {"uuid": "acct-fake", "email_address": "a@example.invalid"}}))
        .into_response(),
        ("/v1/messages", "Bearer sk-ant-oat-old-fake") => (
            StatusCode::UNAUTHORIZED,
            [("content-type", "application/json")],
            r#"{"type":"error","error":{"type":"authentication_error","message":"token expired"}}"#,
        )
            .into_response(),
        _ => (
            [("content-type", "application/json")],
            r#"{"id":"msg_1","type":"message","role":"assistant","model":"m","content":[],"stop_reason":"end_turn"}"#,
        )
            .into_response(),
    }
}

/// Go refreshes OAuth tokens in the background (conductor_refresh.go) and recovers a
/// rejected token with one refresh-and-retry (tryRefreshAfterUnauthorized): an expired
/// token is sent first, never awaited.
#[tokio::test]
async fn expired_token_is_used_then_refreshed_once_after_401() {
    let mock = Arc::new(OAuthMock::default());
    let upstream_url = serve(axum::Router::new().fallback(oauth_upstream).with_state(mock.clone())).await;
    let dir = std::env::temp_dir().join(format!("cpa-routes-401-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("claude.json");
    let metadata = serde_json::json!({"type":"claude","access_token":"sk-ant-oat-old-fake","refresh_token":"fake-refresh",
        "expired":"2000-01-01T00:00:00Z","account_uuid":"acct-fake","email":"a@example.invalid",
        "claude_device_ids":["b".repeat(64)]});
    std::fs::write(&path, metadata.to_string()).unwrap();
    let mut credential = Credential::from_file(&dir, &path, metadata.as_object().unwrap().clone()).unwrap();
    credential.attributes.insert("base_url".into(), upstream_url.clone());
    let oauth = cpa_exec::oauth::OAuth::with_endpoints(
        wreq::Client::new(),
        &format!("{upstream_url}/token"),
        &format!("{upstream_url}/profile"),
        &format!("{upstream_url}/roles"),
    );
    let executors = Executors {
        claude: ClaudeExecutor::new(&upstream_url).unwrap().with_oauth(oauth),
        codex: Default::default(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let config = Config::parse("access:\n  api-keys: [client-key]\n").unwrap();
    let rt = Arc::new(cpa_server::testing::runtime(config, vec![credential], executors));
    let url = serve(router(rt.clone())).await;
    let (status, _, text) = post(
        &url,
        "/v1/messages",
        &format!(r#"{{"model":"{MODEL}","max_tokens":5,"messages":[]}}"#),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let calls = mock.calls.lock().unwrap().clone();
    assert_eq!(
        calls.iter().map(|c| c.split(' ').next().unwrap()).collect::<Vec<_>>(),
        // Claude's refresh also reads the profile (the Go differential run does too).
        ["/v1/messages", "/token", "/profile", "/v1/messages"],
        "{calls:?}"
    );
    assert!(calls[0].ends_with("sk-ant-oat-old-fake") && calls[3].ends_with("sk-ant-oat-new-fake"));
    let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        saved["access_token"], "sk-ant-oat-new-fake",
        "the rotated token is persisted"
    );
    assert_eq!(rt.store().stats().success, 1);
    std::fs::remove_dir_all(&dir).unwrap();
}

type Reply = Box<dyn Fn() -> Response + Send + Sync>;

/// Replies in order, one per upstream call; the last repeats.
struct Script {
    replies: Vec<Reply>,
    calls: Mutex<Vec<String>>,
}

async fn scripted(State(script): State<Arc<Script>>, req: Request) -> Response {
    let auth = req
        .headers()
        .get("authorization")
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let n = {
        let mut calls = script.calls.lock().unwrap();
        calls.push(auth);
        calls.len() - 1
    };
    (script.replies[n.min(script.replies.len() - 1)])()
}

fn status_reply(status: u16) -> Reply {
    Box::new(move || (StatusCode::from_u16(status).unwrap(), "scripted failure").into_response())
}

fn sse_reply() -> Reply {
    Box::new(|| ([("content-type", "text/event-stream")], sse()).into_response())
}

fn json_reply() -> Reply {
    Box::new(|| {
        (
            [("content-type", "application/json")],
            r#"{"id":"msg_1","type":"message","role":"assistant","model":"m","content":[],"stop_reason":"end_turn"}"#,
        )
            .into_response()
    })
}

async fn scripted_proxy(config: &str, tokens: &[&str], replies: Vec<Reply>) -> (String, Arc<Script>, Arc<Runtime>) {
    let script = Arc::new(Script {
        replies,
        calls: Mutex::default(),
    });
    let upstream_url = serve(axum::Router::new().fallback(scripted).with_state(script.clone())).await;
    let credentials = tokens
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mut c = oauth(&format!("{i}.json"), t, serde_json::json!({}));
            c.attributes.insert("base_url".into(), upstream_url.clone());
            c
        })
        .collect();
    let config = Config::parse(&format!("access:\n  api-keys: [client-key]\n{config}")).unwrap();
    let executors = Executors {
        claude: ClaudeExecutor::new(&upstream_url).unwrap(),
        codex: Default::default(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(config, credentials, executors));
    (serve(router(rt.clone())).await, script, rt)
}

/// Go conductor_execution.go: a selection failure (every credential cooling) takes part
/// in retry rounds, so the request waits for the cooldown instead of failing.
#[tokio::test]
async fn cooling_selection_waits_for_the_next_retry_round() {
    let config = "routing:\n  retry:\n    request-retry: 1\n    max-retry-interval: 3\n  cooldown:\n    transient-error-cooldown-seconds: 1\n";
    let (url, script, _) = scripted_proxy(
        config,
        &["fake-a"],
        vec![status_reply(500), status_reply(500), json_reply()],
    )
    .await;
    let body = format!(r#"{{"model":"{MODEL}","max_tokens":5,"messages":[]}}"#);
    // Two rounds of 500: the credential ends cooling for one second.
    let (status, _, _) = post(&url, "/v1/messages", &body).await;
    assert_eq!(status, 500);
    assert_eq!(script.calls.lock().unwrap().len(), 2);
    let started = std::time::Instant::now();
    let (status, _, text) = post(&url, "/v1/messages", &body).await;
    assert_eq!(status, 200, "{text}");
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(500),
        "waited for the cooldown"
    );
    assert_eq!(script.calls.lock().unwrap().len(), 3);
}

/// Go conductor_stream.go: a stream that closes before its first payload is a failed
/// attempt (`empty_stream`) and fails over.
#[tokio::test]
async fn empty_stream_fails_over_and_reports_empty_stream() {
    let empty: Reply = Box::new(|| ([("content-type", "text/event-stream")], "").into_response());
    let (url, script, rt) = scripted_proxy("", &["fake-a", "fake-b"], vec![empty, sse_reply()]).await;
    let body = format!(r#"{{"model":"{MODEL}","stream":true,"max_tokens":5,"messages":[]}}"#);
    let (status, _, text) = post(&url, "/v1/messages", &body).await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("message_stop"));
    assert_eq!(script.calls.lock().unwrap().len(), 2);
    let stats = rt.store().stats();
    assert_eq!((stats.failure, stats.success), (1, 1));

    let empty: Reply = Box::new(|| ([("content-type", "text/event-stream")], "").into_response());
    let (url, _, _) = scripted_proxy("", &["fake-a"], vec![empty]).await;
    let (status, _, text) = post(&url, "/v1/messages", &body).await;
    assert_eq!(status, 500);
    assert_eq!(
        text,
        r#"{"type":"error","error":{"type":"api_error","message":"empty_stream: upstream stream closed before first payload"}}"#
    );
}

/// Go handlers_stream.go: `requests.streaming.bootstrap-retries` re-runs a stream that
/// failed before its first payload, independently of request-retry.
#[tokio::test]
async fn bootstrap_retries_rerun_a_stream_that_broke_before_its_first_payload() {
    let broken: Reply = Box::new(|| {
        // A reset before any body byte: the executor's first item is the error (Go: a
        // channel error before the first payload). Go's Claude stream flushes any bytes
        // that did arrive, even a partial line, as a payload before the error.
        let body =
            futures_util::stream::iter([Err::<Bytes, _>(std::io::Error::other("reset"))]).then(|item| async move {
                // Let hyper send the response head before the reset.
                if item.is_err() {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                item
            });
        (
            [("content-type", "text/event-stream")],
            axum::body::Body::from_stream(body),
        )
            .into_response()
    });
    let body = format!(r#"{{"model":"{MODEL}","stream":true,"max_tokens":5,"messages":[]}}"#);
    let config = "requests:\n  streaming:\n    bootstrap-retries: 1\n";
    let (url, script, _) = scripted_proxy(config, &["fake-a"], vec![broken, sse_reply()]).await;
    let (status, _, text) = post(&url, "/v1/messages", &body).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(script.calls.lock().unwrap().len(), 2);

    let broken: Reply = Box::new(|| {
        // A reset before any body byte: the executor's first item is the error (Go: a
        // channel error before the first payload). Go's Claude stream flushes any bytes
        // that did arrive, even a partial line, as a payload before the error.
        let body =
            futures_util::stream::iter([Err::<Bytes, _>(std::io::Error::other("reset"))]).then(|item| async move {
                // Let hyper send the response head before the reset.
                if item.is_err() {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                item
            });
        (
            [("content-type", "text/event-stream")],
            axum::body::Body::from_stream(body),
        )
            .into_response()
    });
    let (url, script, _) = scripted_proxy("", &["fake-a"], vec![broken, sse_reply()]).await;
    let (status, _, _) = post(&url, "/v1/messages", &body).await;
    assert_eq!(status, 500, "without bootstrap retries the transport fault is final");
    assert_eq!(script.calls.lock().unwrap().len(), 1);
}

/// Go net/http: no Content-Length set by the handler means chunked above 2048 bytes and
/// Content-Length at or below it; a bodiless HEAD carries neither.
#[tokio::test]
async fn response_framing_follows_go_net_http() {
    let (url, _, _) = scripted_proxy("", &["fake-a"], vec![json_reply()]).await;
    let client = wreq::Client::new();
    let res = client
        .get(format!("{url}/v1/models"))
        .header("authorization", "Bearer client-key")
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .unwrap();
    let headers = res.headers().clone();
    let body = res.bytes().await.unwrap();
    assert!(body.len() > 2048, "the Anthropic catalog is large: {}", body.len());
    assert_eq!(
        headers.get("transfer-encoding").map(|v| v.to_str().unwrap()),
        Some("chunked")
    );
    assert!(headers.get("content-length").is_none());

    let res = client.get(format!("{url}/")).send().await.unwrap();
    let length = res
        .headers()
        .get("content-length")
        .map(|v| v.to_str().unwrap().to_owned());
    assert_eq!(length, Some(res.bytes().await.unwrap().len().to_string()));

    let res = client.head(format!("{url}/healthz")).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert!(res.headers().get("content-length").is_none(), "{:?}", res.headers());
    assert!(res.headers().get("transfer-encoding").is_none(), "{:?}", res.headers());

    // Client-auth failures use gin's JSON content type.
    let res = client.get(format!("{url}/v1/models")).send().await.unwrap();
    assert_eq!(res.status(), 401);
    assert_eq!(
        res.headers().get("content-type").unwrap(),
        "application/json; charset=utf-8"
    );
    assert_eq!(res.text().await.unwrap(), r#"{"error":"Missing API key"}"#);
}

/// Go `RequestPathMetadataKey` is gin's `FullPath()`: route templates in gin syntax.
#[tokio::test]
async fn route_path_uses_gin_templates() {
    use axum::extract::{MatchedPath, OriginalUri};
    let echo = |m: Option<MatchedPath>, OriginalUri(uri): OriginalUri| async move {
        cpa_server::dispatch::route_path(m.as_ref(), &uri)
    };
    let app = axum::Router::new()
        .route("/v1beta/models/{*action}", axum::routing::post(echo))
        .route("/v1/videos/{request_id}", axum::routing::post(echo))
        .route("/v1/images/generations", axum::routing::post(echo));
    let url = serve(app).await;
    let client = wreq::Client::new();
    for (path, expected) in [
        (
            "/v1beta/models/gemini-2.5-pro:generateContent",
            "/v1beta/models/*action",
        ),
        ("/v1/videos/abc", "/v1/videos/:request_id"),
        ("/v1/images/generations?x=1", "/v1/images/generations"),
    ] {
        let got = client
            .post(format!("{url}{path}"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(got, expected, "{path}");
    }
}

/// Go's usage queue gets one `queuedUsageDetail` per upstream attempt (the shape is
/// pinned against Go in `usage_record::tests`): a failed attempt records its status
/// and body, the failover attempt its parsed usage; streams publish when they end.
#[tokio::test]
async fn usage_queue_records_every_attempt() {
    let p = proxy(
        "",
        vec![
            oauth("a.json", "fake-fail", serde_json::json!({})),
            oauth("b.json", "fake-ok", serde_json::json!({"email": "b@example.com"})),
        ],
    )
    .await;
    let queue = p.rt.usage_queue();
    let pop = || -> Vec<Value> {
        queue
            .pop_oldest(10)
            .iter()
            .map(|r| serde_json::from_slice(r).unwrap())
            .collect()
    };
    let body = format!(r#"{{"model":"{MODEL}","max_tokens":5,"messages":[]}}"#);
    let cfg = Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap();
    queue.configure(true, &cfg);

    let (status, headers, text) = post(&p.url, "/v1/messages", &body).await;
    assert_eq!(status, 200, "{text}");
    let trace = headers["x-cpa-trace-id"].to_str().unwrap();
    let request_id = trace.splitn(3, '-').nth(2).unwrap();
    let records = pop();
    assert_eq!(records.len(), 2, "{records:?}");
    let (failed, ok) = (&records[0], &records[1]);
    assert_eq!(failed["failed"], true);
    assert_eq!(failed["fail"], serde_json::json!({"status_code": 500, "body": "boom"}));
    // No email and no API key: Go's source falls back to the client key.
    assert_eq!(failed["source"], "client-key");
    assert_eq!(ok["failed"], false);
    assert_eq!(ok["fail"], serde_json::json!({"status_code": 200, "body": ""}));
    assert_eq!(ok["source"], "b@example.com");
    assert_eq!(ok["tokens"]["input_tokens"], 5);
    assert_eq!(ok["tokens"]["output_tokens"], 1);
    assert_eq!(ok["tokens"]["total_tokens"], 6);
    assert_eq!(ok["token_breakdown"]["quality"], "complete");
    for (key, want) in [
        ("provider", "claude"),
        ("executor_type", "ClaudeExecutor"),
        ("model", MODEL),
        ("alias", MODEL),
        ("endpoint", "POST /v1/messages"),
        ("auth_type", "oauth"),
        ("api_key", "client-key"),
        ("service_tier", "auto"),
        ("response_model", MODEL),
    ] {
        assert_eq!(ok[key], want, "{key}");
    }
    assert_eq!(ok["request_id"], request_id);
    assert_eq!(ok["trace_id"], request_id);
    assert_eq!(ok["stream"], false);
    assert_eq!(ok["generate"], true);
    assert_ne!(failed["execution_id"], ok["execution_id"]);
    assert_eq!(ok["access_token_sha256"].as_str().unwrap().len(), 64);

    // A streamed attempt publishes once the stream ends, with the merged usage.
    let body = format!(r#"{{"model":"{MODEL}","stream":true,"messages":[]}}"#);
    let (status, _, _) = post(&p.url, "/v1/messages", &body).await;
    assert_eq!(status, 200);
    let records = pop();
    let streamed = records.last().unwrap();
    assert_eq!(streamed["stream"], true);
    assert_eq!(streamed["tokens"]["input_tokens"], 5);
    assert_eq!(streamed["tokens"]["output_tokens"], 1);
    assert_eq!(streamed["response_model"], MODEL);

    // Without management (or usage statistics) nothing is queued.
    queue.configure(false, &cfg);
    let (status, _, _) = post(&p.url, "/v1/messages", &body).await;
    assert_eq!(status, 200);
    queue.configure(true, &cfg);
    assert!(pop().is_empty());
}

/// Go's Responses handler applies `prepareCodexOrphanDelegation` before dispatch, so the
/// rewrite reaches every provider (here Claude). Input and rewritten text come from
/// Go's `RewriteCodexOrphanDelegationInput` golden (cpa-common codex_client_go.json).
#[tokio::test]
async fn responses_route_prepares_codex_orphan_delegation() {
    let input = r#"[{"type":"function_call","call_id":"c1","name":"create_thread","namespace":"codex_app","arguments":"{}"},{"type":"function_call_output","call_id":"c1","name":"create_thread","namespace":"codex_app","output":"paired"},{"type":"function_call_output","call_id":"c1","name":"create_thread","namespace":"codex_app","output":"second <one>"}]"#;
    let body = format!(r#"{{"model":"{MODEL}","input":{input}}}"#);
    let send = |url: String, body: String| async move {
        let res = wreq::Client::new()
            .post(format!("{url}/v1/responses"))
            .header("authorization", "Bearer client-key")
            .header("x-openai-subagent", "collab_spawn")
            .body(body)
            .send()
            .await
            .unwrap();
        res.status().as_u16()
    };
    let rewritten = "Tool output from codex_app__create_thread:\\nsecond <one>";
    // Legacy `codex.orphan-delegation-compatibility` applies at the Responses boundary.
    let p = proxy(
        "codex:\n  orphan-delegation-compatibility: true\n",
        vec![oauth("a.json", "fake-ok", serde_json::json!({}))],
    )
    .await;
    assert_eq!(send(p.url.clone(), body.clone()).await, 200);
    let upstream = p.seen.requests.lock().unwrap()[0].1.to_string();
    assert!(upstream.contains(rewritten), "{upstream}");
    // Written in v8 form it is OAuth-only: the handler leaves the input alone, but an
    // OAuth credential's executor still sees it (Go's executor translation runs
    // RewriteCodexOrphanDelegationInputForConfig with the unscoped config).
    let v8 = "oauth:\n  providers:\n    codex:\n      orphan-delegation-compatibility: true\n";
    let p = proxy(v8, vec![oauth("a.json", "fake-ok", serde_json::json!({}))]).await;
    assert_eq!(send(p.url.clone(), body.clone()).await, 200);
    let upstream = p.seen.requests.lock().unwrap()[0].1.to_string();
    assert!(upstream.contains(rewritten), "{upstream}");
    // An API-key credential runs with cfg.ForAPIKey() (executorForAuth): nobody rewrites.
    let mut key = Credential::from_file(
        Path::new("/fake"),
        &Path::new("/fake").join("key.json"),
        serde_json::json!({"type": "claude"}).as_object().unwrap().clone(),
    )
    .unwrap();
    key.attributes.insert("api_key".into(), "fake-ok".into());
    let p = proxy(v8, vec![key]).await;
    assert_eq!(send(p.url.clone(), body).await, 200);
    let upstream = p.seen.requests.lock().unwrap()[0].1.to_string();
    assert!(!upstream.contains("Tool output from"), "{upstream}");
}

/// Session turns (the Responses WebSocket) run through `dispatch::run`: the pin
/// excludes every other credential in every round, the callback sees each attempted
/// credential, and attempts go through `execute_in_session`.
#[tokio::test]
async fn session_turns_honour_pin_callback_and_session() {
    use cpa_core::exec::{Caller, ExecSession, Operation};
    use cpa_core::format::Format;
    use cpa_server::dispatch::{self, Call, SessionTurn};
    let p = proxy(
        "",
        vec![
            oauth("a.json", "fake-ok", serde_json::json!({})),
            oauth("b.json", "fake-fail", serde_json::json!({})),
        ],
    )
    .await;
    let selected = Arc::new(Mutex::new(Vec::<String>::new()));
    let turn = |pinned: Option<&str>, continuation: bool| {
        let seen = selected.clone();
        Arc::new(SessionTurn {
            session: ExecSession {
                id: "ws-1".into(),
                continuation,
                lease: None,
            },
            pinned: pinned.map(str::to_owned),
            on_selected: Some(Box::new(move |c| seen.lock().unwrap().push(c.id.clone()))),
            home: None,
        })
    };
    let call = |turn| Call {
        entry: Format::Claude,
        response: Format::Claude,
        operation: Operation::Generate,
        model: MODEL.into(),
        body: Bytes::from(format!(r#"{{"model":"{MODEL}","stream":true,"messages":[]}}"#)),
        stream: true,
        alt: None,
        headers: Default::default(),
        caller: Caller {
            principal: "client-key".into(),
            source: "authorization",
        },
        forced_provider: None,
        selection_model: None,
        execution_session: Some("ws-1".into()),
        request_path: "/v1/responses".into(),
        peer: None,
        turn: Some(turn),
        media: None,
    };
    let tokens = || -> Vec<String> { p.seen.requests.lock().unwrap().drain(..).map(|r| r.0).collect() };

    // Pinned to the failing credential: no failover to a.json.
    let result = dispatch::run(&p.rt, call(turn(Some("b.json"), false)), &dispatch::Trace::default()).await;
    let error = result.err().expect("pinned credential fails");
    assert_eq!(error.failure.status(), 500);
    assert_eq!(tokens(), ["Bearer fake-fail"]);
    assert_eq!(*selected.lock().unwrap(), ["b.json"]);

    // Pinned to the healthy one: served there, and the callback names it.
    selected.lock().unwrap().clear();
    let done = dispatch::run(&p.rt, call(turn(Some("a.json"), false)), &dispatch::Trace::default()).await;
    assert!(matches!(done, Ok(dispatch::Done::Stream { .. })));
    drop(done);
    assert_eq!(tokens(), ["Bearer fake-ok"]);
    assert_eq!(*selected.lock().unwrap(), ["a.json"]);

    // A continuation turn needs the session's upstream socket; a Claude credential
    // cannot hold one, so execute_in_session refuses without calling upstream.
    selected.lock().unwrap().clear();
    let result = dispatch::run(&p.rt, call(turn(Some("a.json"), true)), &dispatch::Trace::default()).await;
    let error = result.err().expect("continuation needs an upstream session");
    assert!(
        error.failure.text().contains("upstream_http_replay_required"),
        "{}",
        error.failure.text()
    );
    assert!(tokens().is_empty());
}
