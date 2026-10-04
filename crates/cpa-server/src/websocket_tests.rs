//! Connection lifecycle under backpressure. Behavioural parity with Go is covered by
//! `tests/ws_e2e.rs` (Go fixtures); this checks the bound Rust adds where Go closes the
//! socket under a blocked writer.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message as AxMessage, WebSocket as AxSocket, WebSocketUpgrade};
use cpa_core::config::Config;
use cpa_exec::Executors;
use serde_json::Value;

use crate::router;
use crate::websocket_tools::is_retained;

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("127.0.0.1:{}", addr.port())
}

/// Answers the first request with far more than the loopback buffers hold, then closes
/// with 1009.
async fn flood_then_close(mut socket: AxSocket) {
    if socket.recv().await.is_none() {
        return;
    }
    let created = r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#;
    let _ = socket.send(AxMessage::Text(created.into())).await;
    let delta = format!(
        r#"{{"type":"response.output_text.delta","delta":"{}"}}"#,
        "x".repeat(4 * 1024 * 1024)
    );
    for _ in 0..8 {
        let _ = socket.send(AxMessage::Text(delta.clone().into())).await;
    }
    let _ = socket
        .send(AxMessage::Close(Some(CloseFrame {
            code: 1009,
            reason: "too big".into(),
        })))
        .await;
}

/// The client stops reading mid-response and upstream then fails. The close frame cannot
/// be flushed behind the queued data, yet the connection must still end and release its
/// session state (Go closes the socket instead of waiting behind the writer).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_loss_while_the_client_stalls_releases_the_session() {
    let upstream =
        serve(axum::Router::new().fallback(|ws: WebSocketUpgrade| async move { ws.on_upgrade(flood_then_close) }))
            .await;
    let cfg = Config::parse(&format!(
        "codex-api-key:\n  - api-key: sk-FAKE\n    base-url: http://{upstream}\n    websockets: true\n    models:\n      - name: gpt-fixture\n"
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    assert_eq!(
        credentials[0].metadata["models"],
        serde_json::json!([{ "name": "gpt-fixture" }])
    );
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let proxy = serve(router(Arc::new(crate::testing::runtime(cfg, credentials, executors)))).await;

    let key = "stalled-client-session";
    let mut socket = wreq::Client::new()
        .websocket(format!("ws://{proxy}/v1/responses"))
        .header("session_id", key)
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    let create =
        r#"{"type":"response.create","model":"gpt-fixture","input":[{"type":"message","role":"user","content":"hi"}]}"#;
    socket.send(wreq::ws::message::Message::text(create)).await.unwrap();

    let started = Instant::now();
    let mut seen = false;
    while started.elapsed() < Duration::from_secs(10) {
        let retained = is_retained(key);
        seen |= retained;
        if seen && !retained {
            // The client never read a byte: the server gave up on the flush and cleaned up.
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let state: Value = serde_json::json!({ "connection_seen": seen });
    panic!("connection still holds its session after 10s: {state}");
}

/// Drops with the upstream response body: the proxy cancelled its request.
struct BodyDropped(Arc<std::sync::atomic::AtomicBool>);

impl Drop for BodyDropped {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// With steering configured, Go's downstream reader cancels the request context when the
/// client leaves, so a turn in flight on an HTTP credential stops its upstream request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steering_reader_cancels_the_turn_when_the_client_leaves() {
    use futures_util::StreamExt;
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = dropped.clone();
    let upstream = serve(axum::Router::new().fallback(move || {
        let guard = BodyDropped(flag.clone());
        async move {
            let events = concat!(
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"output\":[]}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"hi\"}\n\n",
            );
            // The response never completes; only a cancelled request releases it.
            let body =
                futures_util::stream::once(async move { Ok::<_, std::io::Error>(axum::body::Bytes::from(events)) })
                    .chain(futures_util::stream::pending())
                    .map(move |chunk| {
                        let _ = &guard;
                        chunk
                    });
            (
                [("content-type", "text/event-stream")],
                axum::body::Body::from_stream(body),
            )
        }
    }))
    .await;
    let cfg = Config::parse(&format!(
        "codex:\n  response-steering: true\ncodex-api-key:\n  - api-key: sk-FAKE\n    base-url: http://{upstream}\n    models:\n      - name: gpt-fixture\n"
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let proxy = serve(router(Arc::new(crate::testing::runtime(cfg, credentials, executors)))).await;

    let mut socket = wreq::Client::new()
        .websocket(format!("ws://{proxy}/v1/responses"))
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    let create =
        r#"{"type":"response.create","model":"gpt-fixture","input":[{"type":"message","role":"user","content":"hi"}]}"#;
    socket.send(wreq::ws::message::Message::text(create)).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .expect("the turn streams")
        .expect("a frame")
        .unwrap();
    let wreq::ws::message::Message::Text(first) = first else {
        panic!("text frame: {first:?}");
    };
    assert!(first.contains("response.created"), "{first}");
    assert!(
        !dropped.load(std::sync::atomic::Ordering::SeqCst),
        "upstream still streaming"
    );
    drop(socket);

    let started = Instant::now();
    while !dropped.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the upstream request outlived the client by 5s"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The request log of a Responses WebSocket connection: Go's downstream timeline
/// (`openai_responses_websocket_timeline.go`) with one `websocket.request` part per client
/// message, one `websocket.response` part per frame written and the read error that ended
/// the connection as `websocket.disconnect` (gorilla's close text), plus the upstream
/// capture of each turn under the upgrade request's log.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_log_records_the_websocket_timeline_and_each_turn() {
    use futures_util::StreamExt;
    use wreq::ws::message::{CloseFrame as WsClose, Message as WsMessage};
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#;
    const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"r1","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#;
    let upstream = serve(axum::Router::new().fallback(|| async {
        (
            [("content-type", "text/event-stream")],
            format!("data: {CREATED}\n\ndata: {COMPLETED}\n\n"),
        )
    }))
    .await;
    let dir = std::env::temp_dir().join(format!("cpa-ws-timeline-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\nobservability: {{logs: {{request-log: true}}}}\ncodex-api-key:\n  - api-key: sk-FAKE\n    base-url: http://{upstream}\n    models:\n      - name: gpt-fixture\n",
        dir.join("auths").display()
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(crate::testing::runtime(cfg, credentials, executors));
    let management = crate::management::Management::with_options(
        rt.clone(),
        dir.join("config.yaml"),
        crate::management::Options {
            log_dir: Some(dir.clone()),
            management_password: Some(String::new()),
            ..Default::default()
        },
    );
    let proxy = serve(crate::request_logging::router(&management, router(rt))).await;

    let mut socket = wreq::Client::new()
        .websocket(format!("ws://{proxy}/v1/responses"))
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    let create = r#"{"type":"response.create","model":"gpt-fixture","input":[]}"#;
    socket.send(WsMessage::text(create)).await.unwrap();
    let mut sent = Vec::new();
    while let Some(Ok(WsMessage::Text(text))) = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .unwrap()
    {
        let done = text.as_str().contains("response.completed");
        sent.push(text.as_str().to_owned());
        if done {
            break;
        }
    }
    assert_eq!(sent.len(), 2, "{sent:?}");
    socket
        .send(WsMessage::Close(Some(WsClose {
            code: 1000.into(),
            reason: "bye".into(),
        })))
        .await
        .unwrap();
    drop(socket);

    let log = written_log(&dir).await;
    let _ = std::fs::remove_dir_all(&dir);
    let section = |name: &str| -> String {
        let start = log
            .find(&format!("=== {name}"))
            .unwrap_or_else(|| panic!("no {name} in\n{log}"));
        let rest = &log[start..];
        let body = &rest[rest.find('\n').unwrap() + 1..];
        body[..body.find("\n=== ").unwrap_or(body.len())].to_owned()
    };
    // Timestamps vary; the events, their order and payloads are Go's.
    let timeline: Vec<(String, String)> = section("WEBSOCKET TIMELINE ===")
        .split("\n\n")
        .filter(|part| !part.trim().is_empty())
        .map(|part| {
            let mut lines = part.lines();
            assert!(lines.next().unwrap().starts_with("Timestamp: "), "{part}");
            let event = lines.next().unwrap().trim_start_matches("Event: ").to_owned();
            (event, lines.collect::<Vec<_>>().join("\n"))
        })
        .collect();
    let mut want = vec![("websocket.request".to_owned(), create.to_owned())];
    want.extend(
        sent.iter()
            .map(|frame| ("websocket.response".to_owned(), frame.clone())),
    );
    want.push((
        "websocket.disconnect".into(),
        "websocket: close 1000 (normal): bye".into(),
    ));
    assert_eq!(timeline, want, "\n{log}");
    // The turn's upstream attempt is captured under the upgrade's log.
    let api_request = section("API REQUEST 1 ===");
    assert!(
        api_request.contains(&format!("Upstream URL: http://{upstream}/responses")),
        "{api_request}"
    );
    assert!(
        section("API RESPONSE").contains(&format!("data: {COMPLETED}")),
        "\n{log}"
    );
}

/// gorilla's `ReadMessage` returns the client's close at once. With steering on and the
/// client no longer reading, its close reply cannot be flushed, yet the turn must still
/// be cancelled (Go's reader cancels the request on that error).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_close_cancels_a_stalled_steering_turn() {
    use futures_util::StreamExt;
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = dropped.clone();
    let upstream = serve(axum::Router::new().fallback(move || {
        let guard = BodyDropped(flag.clone());
        async move {
            let created = axum::body::Bytes::from_static(
                b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"output\":[]}}\n\n",
            );
            let delta = axum::body::Bytes::from(format!(
                "data: {{\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"{}\"}}\n\n",
                "x".repeat(1 << 20)
            ));
            // Far more than the socket buffers hold, then a response that never ends.
            let body = futures_util::stream::iter(
                std::iter::once(created)
                    .chain(std::iter::repeat_n(delta, 32))
                    .map(Ok::<_, std::io::Error>),
            )
            .chain(futures_util::stream::pending())
            .map(move |chunk| {
                let _ = &guard;
                chunk
            });
            (
                [("content-type", "text/event-stream")],
                axum::body::Body::from_stream(body),
            )
        }
    }))
    .await;
    let cfg = Config::parse(&format!(
        "codex:\n  response-steering: true\ncodex-api-key:\n  - api-key: sk-FAKE\n    base-url: http://{upstream}\n    models:\n      - name: gpt-fixture\n"
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let proxy = serve(router(Arc::new(crate::testing::runtime(cfg, credentials, executors)))).await;
    let mut socket = wreq::Client::new()
        .websocket(format!("ws://{proxy}/v1/responses"))
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    let create =
        r#"{"type":"response.create","model":"gpt-fixture","input":[{"type":"message","role":"user","content":"hi"}]}"#;
    socket.send(wreq::ws::message::Message::text(create)).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .expect("the turn streams")
        .expect("a frame")
        .unwrap();
    assert!(matches!(first, wreq::ws::message::Message::Text(_)), "{first:?}");
    // The client stops reading; the server's writes stall behind the full buffers.
    tokio::time::sleep(Duration::from_millis(500)).await;
    socket
        .send(wreq::ws::message::Message::Close(Some(wreq::ws::message::CloseFrame {
            code: 1000.into(),
            reason: "bye".into(),
        })))
        .await
        .unwrap();
    let started = Instant::now();
    while !dropped.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the turn outlived the client's close by 5s"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(socket);
}

/// Home with a gated request-log forward and a refused pick: the turn fails and the
/// server closes the connection.
struct GatedHome(Arc<tokio::sync::Notify>);

impl crate::remote::RemoteDispatch for GatedHome {
    fn available(&self) -> bool {
        true
    }
    fn dispatch(
        &self,
        _: crate::remote::RemoteRequest,
    ) -> futures_util::future::BoxFuture<'_, Result<crate::remote::RemoteGrant, crate::remote::RemoteError>> {
        Box::pin(async {
            Err(crate::remote::RemoteError::plain(
                cpa_core::exec::ExecError::local(503, cpa_core::exec::FailureScope::Credential, "busy"),
                "busy",
            ))
        })
    }
    fn models(
        &self,
        _: Vec<(String, String)>,
        _: Vec<(String, String)>,
    ) -> futures_util::future::BoxFuture<'_, Result<Vec<u8>, crate::remote::ModelsError>> {
        Box::pin(async { Err(crate::remote::ModelsError::Unavailable) })
    }
    fn request_log(&self, _: Vec<u8>) -> futures_util::future::BoxFuture<'_, Result<(), String>> {
        let gate = self.0.clone();
        Box::pin(async move {
            gate.notified().await;
            Ok(())
        })
    }
}

/// Go closes the socket as soon as the connection ends and writes its request log in a
/// deferred step: a slow log delivery must not hold the client's connection open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_socket_closes_before_the_request_log_is_delivered() {
    use futures_util::StreamExt;
    let dir = std::env::temp_dir().join(format!("cpa-ws-gated-log-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\nobservability: {{logs: {{request-log: true}}}}\ncodex-api-key:\n  - api-key: sk-FAKE\n    base-url: http://127.0.0.1:1\n    models:\n      - name: gpt-fixture\n",
        dir.join("auths").display()
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(crate::testing::runtime(cfg, credentials, executors));
    let gate = Arc::new(tokio::sync::Notify::new());
    rt.set_remote_dispatch(Some(Arc::new(GatedHome(gate.clone()))));
    let management = crate::management::Management::with_options(
        rt.clone(),
        dir.join("config.yaml"),
        crate::management::Options {
            log_dir: Some(dir.clone()),
            management_password: Some(String::new()),
            ..Default::default()
        },
    );
    let proxy = serve(crate::request_logging::router(&management, router(rt))).await;
    let mut socket = wreq::Client::new()
        .websocket(format!("ws://{proxy}/v1/responses"))
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    socket
        .send(wreq::ws::message::Message::text(
            r#"{"type":"response.create","model":"gpt-fixture","input":[]}"#,
        ))
        .await
        .unwrap();
    // Every frame until the connection ends, while the log forward is still held.
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match socket.next().await {
                Some(Ok(wreq::ws::message::Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;
    gate.notify_one();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(closed.is_ok(), "the socket stayed open while the request log was held");
}

/// One Responses WebSocket turn through the request-log layer against an upstream that
/// answers `status` with `body`: the text frames the client saw and the written log.
async fn logged_failure(status: u16, body: &'static str) -> (Vec<String>, String) {
    use futures_util::StreamExt;
    let upstream = serve(axum::Router::new().fallback(move || async move {
        (
            axum::http::StatusCode::from_u16(status).unwrap(),
            [("content-type", "application/json")],
            body,
        )
    }))
    .await;
    let dir = std::env::temp_dir().join(format!("cpa-ws-timeline-error-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\nobservability: {{logs: {{request-log: true}}}}\ncodex-api-key:\n  - api-key: sk-FAKE\n    base-url: http://{upstream}\n    models:\n      - name: gpt-fixture\n",
        dir.join("auths").display()
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(crate::testing::runtime(cfg, credentials, executors));
    let management = crate::management::Management::with_options(
        rt.clone(),
        dir.join("config.yaml"),
        crate::management::Options {
            log_dir: Some(dir.clone()),
            management_password: Some(String::new()),
            ..Default::default()
        },
    );
    let proxy = serve(crate::request_logging::router(&management, router(rt))).await;
    let mut socket = wreq::Client::new()
        .websocket(format!("ws://{proxy}/v1/responses"))
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    socket
        .send(wreq::ws::message::Message::text(
            r#"{"type":"response.create","model":"gpt-fixture","input":[]}"#,
        ))
        .await
        .unwrap();
    let mut frames = Vec::new();
    while let Ok(Some(Ok(message))) = tokio::time::timeout(Duration::from_secs(10), socket.next()).await {
        match message {
            wreq::ws::message::Message::Text(text) => frames.push(text.as_str().to_owned()),
            wreq::ws::message::Message::Close(_) => break,
            _ => {}
        }
    }
    drop(socket);
    let log = written_log(&dir).await;
    let _ = std::fs::remove_dir_all(&dir);
    (frames, log)
}

/// The `=== WEBSOCKET TIMELINE ===` parts as (event, payload), timestamps dropped.
fn timeline_events(log: &str) -> Vec<(String, String)> {
    let start = log
        .find("=== WEBSOCKET TIMELINE ===")
        .unwrap_or_else(|| panic!("no timeline in\n{log}"));
    let rest = &log[start..];
    let body = &rest[rest.find('\n').unwrap() + 1..];
    let body = &body[..body.find("\n=== ").unwrap_or(body.len())];
    body.split("\n\n")
        .filter(|part| !part.trim().is_empty())
        .map(|part| {
            let mut lines = part.lines();
            assert!(lines.next().unwrap().starts_with("Timestamp: "), "{part}");
            let event = lines.next().unwrap().trim_start_matches("Event: ").to_owned();
            (event, lines.collect::<Vec<_>>().join("\n"))
        })
        .collect()
}

/// Go's terminal-error timeline (`writeResponsesWebsocketTerminalError`): an exposed
/// request fault is the `websocket.response` part the client received; a hidden failure
/// (quota) keeps the upstream reason as a `websocket.disconnect` part although the client
/// only sees the close. Either way the connection then ends with gorilla's "close sent".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_log_records_terminal_errors_like_go() {
    const REQUEST: &str = r#"{"type":"response.create","model":"gpt-fixture","input":[]}"#;
    const CLOSE_SENT: &str = "websocket: close sent";

    let (frames, log) = logged_failure(
        400,
        r#"{"error":{"message":"bad input","type":"invalid_request_error","code":"invalid_value"}}"#,
    )
    .await;
    assert_eq!(frames.len(), 1, "the exposed error event: {frames:?}");
    assert_eq!(
        timeline_events(&log),
        [
            ("websocket.request".to_owned(), REQUEST.to_owned()),
            ("websocket.response".to_owned(), frames[0].clone()),
            ("websocket.disconnect".to_owned(), CLOSE_SENT.to_owned()),
        ],
        "\n{log}"
    );

    const QUOTA: &str = r#"{"error":{"type":"usage_limit_reached","message":"quota exhausted"}}"#;
    let (frames, log) = logged_failure(429, QUOTA).await;
    assert!(frames.is_empty(), "a quota failure closes silently: {frames:?}");
    let events = timeline_events(&log);
    assert_eq!(events.len(), 3, "\n{log}");
    assert_eq!(events[0], ("websocket.request".to_owned(), REQUEST.to_owned()));
    assert_eq!(events[1].0, "websocket.disconnect", "\n{log}");
    assert!(
        events[1].1.contains("usage_limit_reached"),
        "the upstream reason: {}",
        events[1].1
    );
    assert_eq!(events[2], ("websocket.disconnect".to_owned(), CLOSE_SENT.to_owned()));
}

/// The request log once it is complete: the writer streams into the `.log` file, so the
/// file is read until its content stops changing.
async fn written_log(dir: &std::path::Path) -> String {
    let started = Instant::now();
    let mut last: Option<String> = None;
    loop {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "no complete request log written"
        );
        let found = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "log"));
        let current = found.map(|path| std::fs::read_to_string(path).unwrap());
        match (&last, &current) {
            (Some(previous), Some(now)) if previous == now && !now.is_empty() => return now.clone(),
            _ => last = current,
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A continuation pinned to its credential whose second turn the upstream fails with
/// `failure` (after `shown`, events the client sees first). The client frames of that
/// turn, the close as `close <code> <reason>` last.
async fn pinned_continuation_failure(shown: &'static [&'static str], failure: &'static str) -> Vec<String> {
    use futures_util::StreamExt;
    use wreq::ws::message::Message as WsMessage;
    const FIRST: [&str; 2] = [
        r#"{"type":"response.created","response":{"id":"r1","status":"in_progress","output":[]}}"#,
        r#"{"type":"response.completed","response":{"id":"r1","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
    ];
    let upstream = serve(axum::Router::new().fallback(move |ws: WebSocketUpgrade| async move {
        ws.on_upgrade(move |mut socket: AxSocket| async move {
            let mut turn = 0;
            while let Some(Ok(message)) = socket.recv().await {
                if !matches!(message, AxMessage::Text(_)) {
                    continue;
                }
                turn += 1;
                let events: Vec<&str> = if turn == 1 {
                    FIRST.to_vec()
                } else {
                    shown.iter().copied().chain([failure]).collect()
                };
                for event in events {
                    let _ = socket.send(AxMessage::Text(event.into())).await;
                }
            }
        })
    }))
    .await;
    let dir = std::env::temp_dir().join(format!("cpa-ws-pinned-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\ncodex-api-key:\n  - api-key: sk-FAKE\n    base-url: http://{upstream}\n    websockets: true\n    models:\n      - name: gpt-fixture\n",
        dir.display()
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let proxy = serve(router(Arc::new(crate::testing::runtime(cfg, credentials, executors)))).await;
    let mut socket = wreq::Client::new()
        .websocket(format!("ws://{proxy}/v1/responses"))
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    let read = async |socket: &mut wreq::ws::WebSocket| {
        let mut frames = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(10), socket.next())
                .await
                .unwrap()
            {
                Some(Ok(WsMessage::Text(text))) => {
                    let done = text.as_str().contains("response.completed");
                    frames.push(text.as_str().to_owned());
                    if done {
                        break;
                    }
                }
                Some(Ok(WsMessage::Close(frame))) => {
                    let (code, reason) = frame.map_or((1005, String::new()), |f| {
                        (u16::from(f.code), f.reason.as_str().to_owned())
                    });
                    frames.push(format!("close {code} {reason}"));
                    break;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => {
                    frames.push("close 1006".into());
                    break;
                }
            }
        }
        frames
    };
    let input = r#""input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]"#;
    socket
        .send(WsMessage::text(format!(
            r#"{{"type":"response.create","model":"gpt-fixture",{input}}}"#
        )))
        .await
        .unwrap();
    let first = read(&mut socket).await;
    assert_eq!(first.len(), 2, "{first:?}");
    socket
        .send(WsMessage::text(format!(
            r#"{{"type":"response.create","model":"gpt-fixture","previous_response_id":"r1",{input}}}"#
        )))
        .await
        .unwrap();
    let frames = read(&mut socket).await;
    let _ = std::fs::remove_dir_all(&dir);
    frames
}

/// `replayPinnedAuthFailure`: a pinned continuation's 401 before anything was shown also
/// lost the session's socket, yet the client is told to replay over HTTP (1012).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinned_continuation_bootstrap_auth_failure_asks_for_replay() {
    let frames = pinned_continuation_failure(
        &[],
        r#"{"type":"error","status":401,"error":{"type":"invalid_request_error","message":"bad token"}}"#,
    )
    .await;
    assert_eq!(frames, ["close 1012 upstream requires HTTP replay"]);
}

/// The same after the response started: the shown event, then the replay close.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinned_continuation_later_quota_failure_asks_for_replay() {
    const DELTA: &str = r#"{"type":"response.output_text.delta","output_index":0,"delta":"hello"}"#;
    let frames = pinned_continuation_failure(
        &[DELTA],
        r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"limit","resets_in_seconds":60}}"#,
    )
    .await;
    assert_eq!(frames, [DELTA, "close 1012 upstream requires HTTP replay"]);
}
