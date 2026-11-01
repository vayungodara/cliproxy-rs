//! Go `Options.WebSocketResponseObserver` end to end: a native plugin declaring
//! `websocket_response_observer` receives every upstream Codex WebSocket text message of
//! a request (Go `EmitWebSocketResponseEvent` beside `AppendCodexAPIWebsocketResponse`),
//! with fake credentials and a loopback upstream.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_server::router;
use futures_util::StreamExt;
use serde_json::{Value, json};
use wreq::ws::WebSocket;
use wreq::ws::message::Message;

const RATE_LIMITS: &str = r#"{"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":42}}}"#;
const CREATED: &str = r#"{"type":"response.created","response":{"id":"r1","status":"in_progress","output":[]}}"#;
const DONE: &str = r#"{"type":"response.completed","response":{"id":"r1","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#;
const CREATE: &str = r#"{"type":"response.create","model":"gpt-fixture","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;

/// The recorder plugin keeps its record directory and hold files in process-wide state,
/// so the tests take turns when they run as threads of one process (plain `cargo test`;
/// nextest runs one per process). Otherwise one test's plugin load redirects the other's
/// records.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Answers every turn with a rate-limit frame, then the response frames.
async fn serve_codex() -> String {
    use axum::extract::ws::{Message, WebSocketUpgrade};
    let handler = |ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
                   req: axum::http::Request<axum::body::Body>| async move {
        let Ok(ws) = ws else {
            panic!("expected a WebSocket upgrade, got {} {}", req.method(), req.uri());
        };
        ws.on_upgrade(|mut socket| async move {
            while let Some(Ok(message)) = socket.recv().await {
                if matches!(message, Message::Text(_)) {
                    for frame in [RATE_LIMITS, CREATED, DONE] {
                        let _ = socket.send(Message::Text(frame.into())).await;
                    }
                }
            }
        })
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, axum::Router::new().fallback(handler))
            .await
            .unwrap()
    });
    url
}

/// A proxy with one Codex API key and the recorder plugin `watcher`.
struct Fixture {
    dir: PathBuf,
    records: PathBuf,
    base: String,
    auth_id: String,
}

impl Fixture {
    async fn start(test: &str, caps: &str) -> Option<Self> {
        let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
        let built = cpa_plugin::testing::built_plugins(tmp, "plugin_ws_observer")?;
        let dir = cpa_plugin::testing::scratch(tmp, test);
        let (auths, plugins, records) = (dir.join("auths"), dir.join("plugins"), dir.join("records"));
        for d in [&auths, &plugins, &records.join("respond/watcher")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::copy(built.join("recorder.so"), plugins.join("watcher.so")).unwrap();
        let codex = serve_codex().await;
        let yaml = format!(
            "auth-dir: {}\naccess:\n  api-keys: [client-key-1]\napi-keys:\n  codex:\n    - base-url: {codex}\n      models: [{{name: gpt-fixture}}]\n      keys: [{{api-key: sk-fake-codex, websockets: true}}]\nplugins:\n  enabled: true\n  dir: {}\n  configs:\n    watcher:\n      enabled: true\n      label: watcher\n      record: {}\n      caps: {caps}\n",
            auths.display(),
            plugins.display(),
            records.display()
        );
        let cfg = Config::parse(&yaml).unwrap();
        let credentials = cpa_core::config::credentials::load(&cfg);
        let auth_id = credentials[0].id.clone();
        let executors = Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        };
        let rt = Arc::new(cpa_server::testing::runtime(cfg, credentials, executors));
        cpa_server::install_registry(&rt);
        cpa_server::plugins::start(&rt).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router(rt)).await.unwrap() });
        Some(Self {
            dir,
            records,
            base,
            auth_id,
        })
    }

    /// Answers the plugin's `method` calls with `envelope`.
    fn respond(&self, method: &str, envelope: &Value) {
        std::fs::write(self.respond_path(method, "json"), envelope.to_string()).unwrap();
    }

    fn respond_path(&self, method: &str, extension: &str) -> PathBuf {
        self.records.join(format!("respond/watcher/{method}.{extension}"))
    }

    /// Go's Codex auto executor takes the upstream WebSocket for a downstream WebSocket.
    async fn socket(&self) -> WebSocket {
        wreq::Client::new()
            .websocket(format!("{}/v1/responses", self.base.replacen("http", "ws", 1)))
            .header("Authorization", "Bearer client-key-1")
            .send()
            .await
            .unwrap()
            .into_websocket()
            .await
            .unwrap()
    }

    /// The requests of the plugin's `method` calls so far.
    fn calls(&self, method: &str) -> Vec<Value> {
        cpa_server::testing::records(&self.records.join("watcher.jsonl"))
            .into_iter()
            .filter(|record| record["method"] == method)
            .map(|record| serde_json::from_str(record["request"].as_str().unwrap()).unwrap())
            .collect()
    }

    fn events(&self) -> Vec<Value> {
        self.calls("websocket.response_event")
    }

    /// Waits up to five seconds for `method` to have been called `count` times.
    async fn wait_calls(&self, method: &str, count: usize) -> Vec<Value> {
        for _ in 0..100 {
            let calls = self.calls(method);
            if calls.len() >= count {
                return calls;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("{method}: {:?}", self.calls(method));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Reads client messages until `count` responses completed.
async fn completions(socket: &mut WebSocket, count: usize) {
    let mut done = 0;
    while done < count {
        let next = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .unwrap();
        let Some(Ok(Message::Text(text))) = next else {
            panic!("{next:?}");
        };
        if text.as_str().contains("\"response.completed\"") {
            done += 1;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observer_plugins_receive_every_upstream_websocket_frame() {
    let _serial = SERIAL.lock().await;
    let Some(fx) = Fixture::start("plugin-ws-observer", "websocket_response_observer,request_interceptor").await else {
        return;
    };
    fx.respond("request.intercept_before", &json!({"ok": true, "result": {}}));
    // Go recomputes the canonical session after an after-auth header rewrite; the
    // observer sees the executed attempt's metadata.
    fx.respond(
        "request.intercept_after",
        &json!({"ok": true, "result": {"Headers": {"X-Session-ID": ["after-auth"]}}}),
    );
    let mut socket = fx.socket().await;
    for turn in 0..2 {
        socket.send(Message::text(CREATE)).await.unwrap();
        completions(&mut socket, 1).await;
        // The executor awaits each observation before it handles the frame, so every
        // event is recorded before the client sees the completion.
        let seen = fx.events();
        let want = 3 * (turn + 1);
        assert_eq!(seen.len(), want, "turn {turn}: {seen:?}");
        let turn_events = &seen[want - 3..];
        let types: Vec<&str> = turn_events.iter().map(|e| e["EventType"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            ["codex.rate_limits", "response.created", "response.completed"],
            "turn {turn}"
        );
        let request_id = turn_events[0]["RequestID"].as_str().unwrap();
        assert!(!request_id.is_empty());
        for (event, frame) in turn_events.iter().zip([RATE_LIMITS, CREATED, DONE]) {
            let payload = base64::engine::general_purpose::STANDARD
                .decode(event["Payload"].as_str().unwrap())
                .unwrap();
            assert_eq!(payload, frame.as_bytes(), "turn {turn}");
            assert_eq!(event["RequestID"], request_id, "one request ID per turn");
            assert!(!event["TraceID"].as_str().unwrap().is_empty());
            assert_eq!(event["SourceFormat"], "openai-response");
            assert_eq!(event["Provider"], "codex");
            assert_eq!(event["Model"], "gpt-fixture");
            assert_eq!(event["RequestedModel"], "gpt-fixture");
            assert_eq!(event["AuthID"], fx.auth_id.as_str());
            assert_eq!(event["AuthType"], "api_key");
            assert_eq!(event["Metadata"]["selected_auth_id"], fx.auth_id.as_str());
            assert_eq!(event["Metadata"]["canonical_session_id"], "header:after-auth");
        }
        if turn == 1 {
            assert_ne!(seen[0]["RequestID"], seen[3]["RequestID"], "each turn has its own ID");
        }
    }
}

/// A slow observer holds the upstream read: neither the terminal frame nor the next turn
/// overtakes its observation, and observations keep the order of the frames.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_observer_is_not_overtaken() {
    let _serial = SERIAL.lock().await;
    let Some(fx) = Fixture::start("plugin-ws-observer-order", "websocket_response_observer").await else {
        return;
    };
    let hold = fx.respond_path("websocket.response_event", "hold");
    std::fs::write(&hold, "").unwrap();
    let mut socket = fx.socket().await;
    socket.send(Message::text(CREATE)).await.unwrap();
    fx.wait_calls("websocket.response_event", 1).await;
    // Turn B is sent without waiting for turn A.
    socket.send(Message::text(CREATE)).await.unwrap();
    let early = tokio::time::timeout(Duration::from_millis(500), socket.next()).await;
    assert!(
        early.is_err(),
        "a frame reached the client before its observer returned: {early:?}"
    );
    assert_eq!(
        fx.events().len(),
        1,
        "the next frame was observed before the first returned"
    );
    std::fs::remove_file(&hold).unwrap();
    completions(&mut socket, 2).await;
    let seen = fx.events();
    let types: Vec<&str> = seen.iter().map(|e| e["EventType"].as_str().unwrap()).collect();
    let turn = ["codex.rate_limits", "response.created", "response.completed"];
    assert_eq!(types, [turn, turn].concat());
    assert!(seen[..3].iter().all(|e| e["RequestID"] == seen[0]["RequestID"]));
    assert!(seen[3..].iter().all(|e| e["RequestID"] == seen[3]["RequestID"]));
    assert_ne!(seen[0]["RequestID"], seen[3]["RequestID"]);
}
