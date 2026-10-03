//! Connection lifecycle under backpressure. Behavioural parity with Go is covered by
//! `tests/ws_e2e.rs` (Go fixtures); this checks the bound Rust adds where Go closes the
//! socket under a blocked writer.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message as AxMessage, WebSocket as AxSocket, WebSocketUpgrade};
use cpa_core::config::Config;
use cpa_exec::Executors;
use serde_json::Value;

use crate::websocket_tools::is_retained;
use crate::{Runtime, router};

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
    let mut credentials = cpa_core::config::credentials::from_config(&cfg);
    // ponytail: config synthesis does not carry `models` into metadata yet (see ws_e2e.rs).
    credentials[0]
        .metadata
        .insert("models".into(), serde_json::json!([{ "name": "gpt-fixture" }]));
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let proxy = serve(router(Arc::new(Runtime::new(cfg, credentials, executors)))).await;

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
