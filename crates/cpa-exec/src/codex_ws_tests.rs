//! Upstream WebSocket executor tests. Error-frame expectations come from Go
//! (`tests/fixtures/codex_ws_errors.json`, `tests/reference/codex/zz_rsfix_ws_errors_test.go`);
//! the lifecycle tests run the executor against a loopback axum WebSocket upstream with
//! fake credentials. Whole-handler behaviour is covered against Go fixtures in
//! cpa-server's `tests/ws_e2e.rs`.

use super::*;
use std::collections::VecDeque;
use std::path::Path;

use axum::extract::State;
use axum::extract::ws::{Message as AxMessage, WebSocket as AxSocket, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::{Caller, Operation};
use cpa_core::format::Format;
use serde_json::Value;

#[test]
fn error_frames_match_go() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("../tests/fixtures/codex_ws_errors.json")).expect("fixture");
    assert_eq!(cases.len(), 15);
    for case in &cases {
        let payload = case["payload"].as_str().unwrap();
        let cooling = case["model_level_cooling"].as_bool().unwrap_or(false);
        let got = ws_error(payload, cooling);
        if !case["matched"].as_bool().unwrap() {
            assert!(got.is_none(), "{payload}");
            continue;
        }
        let got = got.unwrap_or_else(|| panic!("{payload}: not classified"));
        assert_eq!(u64::from(got.status), case["status"].as_u64().unwrap(), "{payload}");
        assert_eq!(
            String::from_utf8_lossy(&got.body),
            case["message"].as_str().unwrap(),
            "{payload}"
        );
        assert_eq!(
            got.retry_after.map(|d| d.as_millis() as i64),
            case["retry_after_ms"].as_i64(),
            "{payload}"
        );
        let credential_scoped = case["credential_scoped"].as_bool().unwrap_or(false);
        assert_eq!(
            got.scope == FailureScope::Credential && response::is_usage_limit(&String::from_utf8_lossy(&got.body)),
            credential_scoped,
            "{payload}: credential-wide usage limit"
        );
        let mut headers: Vec<(String, String)> = got
            .headers
            .iter()
            .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap().to_owned()))
            .collect();
        headers.sort();
        let mut want: Vec<(String, String)> = case["headers"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.as_str().unwrap().to_owned()))
            .collect();
        want.sort();
        assert_eq!(headers, want, "{payload}");
    }
}

/// What the scripted upstream does with each received frame.
#[derive(Clone)]
enum Act {
    Send(Vec<&'static str>),
    /// Send, then close the socket with this code.
    SendClose(Vec<&'static str>, u16),
    /// Send one owned text frame (large payloads).
    SendOwned(String),
    /// Ping `count` times, `every` apart, without any application message.
    Pings(Duration, usize),
}

#[derive(Default)]
struct Upstream {
    script: Mutex<VecDeque<Act>>,
    frames: Mutex<Vec<String>>,
    dials: Mutex<Vec<HeaderMap>>,
    /// Handshake rejection (status, body) for the next dial.
    reject: Mutex<Option<(u16, &'static str)>>,
    /// Sockets that ended (closed by either side).
    ended: Mutex<usize>,
    /// Send a binary message right after the handshake, before any request.
    binary_first: Mutex<bool>,
}

async fn handler(State(up): State<Arc<Upstream>>, ws: WebSocketUpgrade, headers: axum::http::HeaderMap) -> Response {
    let mut converted = HeaderMap::new();
    for (k, v) in &headers {
        converted.insert(
            http::HeaderName::from_bytes(k.as_str().as_bytes()).unwrap(),
            http::HeaderValue::from_bytes(v.as_bytes()).unwrap(),
        );
    }
    up.dials.lock().unwrap().push(converted);
    if let Some((status, body)) = up.reject.lock().unwrap().take() {
        return (
            axum::http::StatusCode::from_u16(status).unwrap(),
            [
                ("content-type", "application/json"),
                ("x-codex-primary-used-percent", "97"),
            ],
            body,
        )
            .into_response();
    }
    ws.on_upgrade(move |socket| serve_socket(up, socket))
}

async fn serve_socket(up: Arc<Upstream>, mut socket: AxSocket) {
    if *up.binary_first.lock().unwrap() {
        let _ = socket.send(AxMessage::Binary(vec![1u8, 2, 3].into())).await;
    }
    while let Some(Ok(message)) = socket.recv().await {
        let AxMessage::Text(text) = message else {
            continue;
        };
        up.frames.lock().unwrap().push(text.as_str().to_owned());
        let act = up.script.lock().unwrap().pop_front();
        let (events, close) = match act {
            Some(Act::Send(events)) => (events, None),
            Some(Act::SendClose(events, code)) => (events, Some(code)),
            Some(Act::SendOwned(text)) => {
                let _ = socket.send(AxMessage::Text(text.into())).await;
                continue;
            }
            Some(Act::Pings(every, count)) => {
                for _ in 0..count {
                    tokio::time::sleep(every).await;
                    if socket.send(AxMessage::Ping(Bytes::new())).await.is_err() {
                        break;
                    }
                }
                continue;
            }
            None => continue,
        };
        for event in events {
            let _ = socket.send(AxMessage::Text(event.into())).await;
        }
        if let Some(code) = close {
            let _ = socket
                .send(AxMessage::Close(Some(axum::extract::ws::CloseFrame {
                    code,
                    reason: "bye".into(),
                })))
                .await;
            break;
        }
    }
    *up.ended.lock().unwrap() += 1;
}

async fn upstream(script: Vec<Act>) -> (Arc<Upstream>, String) {
    let up = Arc::new(Upstream::default());
    up.script.lock().unwrap().extend(script);
    let app = axum::Router::new().fallback(handler).with_state(up.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (up, url)
}

fn credential(base_url: &str) -> Credential {
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/codex-ws.json"),
        serde_json::json!({"type": "codex"}).as_object().unwrap().clone(),
    )
    .unwrap();
    for (k, v) in [("api_key", "sk-FAKE"), ("base_url", base_url), ("websockets", "true")] {
        credential.attributes.insert(k.into(), v.into());
    }
    credential
}

fn request(body: &str) -> ExecRequest {
    let body = Bytes::from(body.to_owned());
    ExecRequest {
        operation: Operation::Generate,
        source_format: Format::OpenAIResponse,
        response_format: Format::OpenAIResponse,
        requested_model: "gpt-fixture".into(),
        model: "gpt-fixture".into(),
        original_body: body.clone(),
        body,
        stream: true,
        alt: None,
        session: None,
        execution_session: Some("conn-1".into()),
        derived_session: None,
        headers: HeaderMap::new(),
        caller: Caller {
            principal: "client-key-FAKE".into(),
            source: "authorization",
        },
    }
}

fn session(continuation: bool) -> ExecSession {
    ExecSession {
        id: "conn-1".into(),
        continuation,
    }
}

async fn collect(response: ExecResponse) -> Vec<Result<String, ExecError>> {
    let ResponseBody::Stream(stream) = response.body else {
        panic!("websocket turns stream");
    };
    stream
        .map(|item| item.map(|b| String::from_utf8_lossy(&b).into_owned()))
        .collect()
        .await
}

const CREATED: &str = r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#;
const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"r1","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#;
const BODY: &str = r#"{"model":"gpt-fixture","input":[{"type":"message","role":"user","content":"hi"}]}"#;

#[tokio::test]
async fn continuation_without_a_socket_requires_replay() {
    let (up, url) = upstream(vec![]).await;
    let executor = CodexExecutor::new().unwrap();
    let error = executor
        .execute_in_session(&credential(&url), request(BODY), &Config::default(), &session(true))
        .await
        .err()
        .expect("no socket to continue on");
    assert!(error.is_replay_required(), "{error}");
    assert!(up.dials.lock().unwrap().is_empty(), "a continuation never dials");
}

/// Two turns share one dial; the continuation turn reuses it, and closing the session
/// closes the upstream socket and drops the pooled session.
#[tokio::test]
async fn turns_reuse_the_socket_and_close_session_releases_it() {
    let (up, url) = upstream(vec![
        Act::Send(vec![CREATED, COMPLETED]),
        Act::Send(vec![CREATED, COMPLETED]),
    ])
    .await;
    let executor = CodexExecutor::new().unwrap();
    let cfg = Config::default();
    let cred = credential(&url);
    for continuation in [false, true] {
        let response = executor
            .execute_in_session(&cred, request(BODY), &cfg, &session(continuation))
            .await
            .unwrap();
        let events = collect(response).await;
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(events.iter().all(Result::is_ok));
    }
    assert_eq!(up.dials.lock().unwrap().len(), 1);
    assert_eq!(up.frames.lock().unwrap().len(), 2);
    assert_eq!(executor.ws.len(), 1);
    executor.close_session("conn-1");
    assert_eq!(executor.ws.len(), 0);
    for _ in 0..50 {
        if *up.ended.lock().unwrap() == 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("upstream socket still open after close_session");
}

/// A rejected handshake reports the upstream status with Go's classification and the
/// quota headers are observed.
#[tokio::test]
async fn handshake_rejection_is_classified() {
    let (up, url) = upstream(vec![]).await;
    *up.reject.lock().unwrap() = Some((401, r#"{"error":{"message":"bad key","type":"invalid_request_error"}}"#));
    let executor = CodexExecutor::new().unwrap();
    let error = executor
        .execute_in_session(&credential(&url), request(BODY), &Config::default(), &session(false))
        .await
        .err()
        .expect("401 handshake");
    assert_eq!(error.status, 401);
    assert_eq!(error.scope, FailureScope::Credential, "{error}");
    assert!(String::from_utf8_lossy(&error.body).contains("bad key"));
    assert_eq!(
        error
            .headers
            .get("x-codex-primary-used-percent")
            .map(|v| v.to_str().unwrap()),
        Some("97")
    );
}

/// With bootstrap buffering, an overload rejection before any output fails the attempt
/// over without telling the downstream session its socket was lost.
#[tokio::test]
async fn bootstrap_overload_fails_over_without_disconnect_notice() {
    const FAILED: &str = r#"{"type":"response.failed","response":{"id":"r1","error":{"code":"server_is_overloaded","message":"overloaded"}}}"#;
    let (_up, url) = upstream(vec![Act::Send(vec![CREATED, FAILED])]).await;
    let executor = CodexExecutor::new().unwrap();
    // Legacy top-level spelling: global, so it applies to this API key too.
    let cfg = Config::parse("codex:\n  stream-bootstrap-buffering: true\n").unwrap();
    let error = executor
        .execute_in_session(&credential(&url), request(BODY), &cfg, &session(false))
        .await
        .err()
        .expect("overload fails the attempt");
    assert_ne!(error.scope, FailureScope::Request, "another credential may serve it");
    let closed = executor.session_closed("conn-1");
    assert!(
        tokio::time::timeout(Duration::from_millis(200), closed).await.is_err(),
        "overload must not close the downstream session"
    );
}

/// An upstream close with 1009 reaches the turn as a request-scoped 413 carrying the
/// upstream reason, and the session learns its socket is gone.
#[tokio::test]
async fn close_1009_is_a_request_scoped_413_and_notifies() {
    let (_up, url) = upstream(vec![Act::SendClose(vec![CREATED], 1009)]).await;
    let executor = CodexExecutor::new().unwrap();
    let closed = executor.session_closed("conn-1");
    let response = executor
        .execute_in_session(&credential(&url), request(BODY), &Config::default(), &session(false))
        .await
        .unwrap();
    let events = collect(response).await;
    let error = events.last().unwrap().as_ref().expect_err("turn ends with the close");
    assert_eq!((error.status, error.scope), (413, FailureScope::Request));
    assert_eq!(
        gjson::get(&String::from_utf8_lossy(&error.body), "error.message").str(),
        "bye"
    );
    let notified = tokio::time::timeout(Duration::from_secs(2), closed)
        .await
        .expect("session notified");
    assert_eq!(notified.status, 413);
}

/// Go keeps one read deadline per application message: pings answered while waiting do
/// not extend it, so a stalled upstream that only pings still times out.
#[tokio::test]
async fn pings_do_not_extend_the_idle_deadline() {
    let (_up, url) = upstream(vec![Act::Pings(Duration::from_millis(100), 30)]).await;
    let mut executor = CodexExecutor::new().unwrap();
    executor.ws = Pool::with_idle(Duration::from_millis(400));
    let response = executor
        .execute_in_session(&credential(&url), request(BODY), &Config::default(), &session(false))
        .await
        .unwrap();
    let events = tokio::time::timeout(Duration::from_secs(2), collect(response))
        .await
        .expect("idle deadline fires despite pings");
    let error = events
        .last()
        .unwrap()
        .as_ref()
        .expect_err("the turn ends with the timeout");
    assert_eq!(error.scope, FailureScope::Transport);
    assert!(String::from_utf8_lossy(&error.body).contains("idle timeout"), "{error}");
}

/// A reader that fails before the first turn activates (binary message right after the
/// handshake) must fail that turn instead of leaving it waiting on a dead socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reader_failure_before_the_turn_activates_fails_the_turn() {
    for _ in 0..20 {
        let (up, url) = upstream(vec![]).await;
        *up.binary_first.lock().unwrap() = true;
        let executor = CodexExecutor::new().unwrap();
        let (cred, cfg, sess) = (credential(&url), Config::default(), session(false));
        let turn = executor.execute_in_session(&cred, request(BODY), &cfg, &sess);
        let outcome = tokio::time::timeout(Duration::from_secs(2), async {
            match turn.await {
                Ok(response) => collect(response).await.into_iter().find_map(Result::err),
                Err(error) => Some(error),
            }
        })
        .await
        .expect("the turn must not hang on a dead socket");
        let error = outcome.expect("the turn fails");
        assert_eq!(error.scope, FailureScope::Transport, "{error}");
    }
}

/// With model-level cooling, a usage-limit handshake rejection cools only the model
/// (`newCodexStatusErrWithCooling`); without it, the whole credential. API keys see
/// `ForAPIKey`: the v8 `oauth.providers.codex` spelling is OAuth-only, the legacy
/// top-level `codex` spelling stays global.
#[tokio::test]
async fn handshake_usage_limit_honours_model_level_cooling() {
    const LIMIT: &str = r#"{"error":{"type":"usage_limit_reached","message":"limit","resets_in_seconds":3600}}"#;
    for (yaml, scope) in [
        ("{}\n", FailureScope::Credential),
        ("codex:\n  model-level-cooling: true\n", FailureScope::Model),
        (
            "oauth:\n  providers:\n    codex:\n      model-level-cooling: true\n",
            FailureScope::Credential,
        ),
    ] {
        let (up, url) = upstream(vec![]).await;
        *up.reject.lock().unwrap() = Some((429, LIMIT));
        let cfg = Config::parse(yaml).unwrap();
        let error = CodexExecutor::new()
            .unwrap()
            .execute_in_session(&credential(&url), request(BODY), &cfg, &session(false))
            .await
            .err()
            .expect("429 handshake");
        assert_eq!((error.status, error.scope), (429, scope), "{yaml}");
        assert_eq!(error.retry_after, Some(Duration::from_secs(3600)));
    }
}

/// Upstream frames above tungstenite's 16 MiB default reach the client (Go sets no read
/// limit; the executor allows 64 MiB).
#[tokio::test]
async fn large_upstream_frames_are_delivered() {
    let delta = format!(
        r#"{{"type":"response.output_text.delta","delta":"{}"}}"#,
        "a".repeat(17 * 1024 * 1024)
    );
    let len = delta.len();
    let (up, url) = upstream(vec![Act::SendOwned(delta)]).await;
    up.script.lock().unwrap().push_back(Act::Send(vec![]));
    let executor = CodexExecutor::new().unwrap();
    let response = executor
        .execute_in_session(&credential(&url), request(BODY), &Config::default(), &session(false))
        .await
        .unwrap();
    let ResponseBody::Stream(mut stream) = response.body else {
        panic!("websocket turns stream");
    };
    let first = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("frame arrives")
        .expect("stream item")
        .expect("not an error");
    assert_eq!(first.len(), len);
}
