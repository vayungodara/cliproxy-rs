//! Responses WebSocket (`GET /v1/responses`) end to end, replaying
//! `tests/ws_fixtures/ws_e2e.json`. Each scenario ran through Go's real handler, auth
//! manager and `CodexAutoExecutor` against a scripted loopback upstream
//! (`tests/ws_fixtures/go/zz_rsfix_ws_test.go`). Here the same client frames and upstream
//! script run through the Rust router and Codex executor; the frames the client sees,
//! close codes, and what upstream received must match. Fake credentials only.

use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message as AxMessage, WebSocket as AxSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_server::{Runtime, router};
use futures_util::StreamExt;
use serde_json::Value;
use wreq::ws::message::Message;

static FIXTURE: LazyLock<Vec<Value>> = LazyLock::new(|| {
    serde_json::from_str::<Value>(include_str!("ws_fixtures/ws_e2e.json"))
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
});

#[derive(Debug, Clone, PartialEq)]
struct Captured {
    kind: String,
    headers: Vec<(String, String)>,
    body: String,
}

#[derive(Default)]
struct Upstream {
    script: Mutex<VecDeque<Value>>,
    captured: Mutex<Vec<Captured>>,
}

fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or_default().to_owned()))
        .collect()
}

async fn upstream_handler(
    State(up): State<Arc<Upstream>>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Ok(ws) = ws {
        up.captured.lock().unwrap().push(Captured {
            kind: "ws_dial".into(),
            headers: header_pairs(&headers),
            body: String::new(),
        });
        return ws.on_upgrade(move |socket| upstream_socket(up, socket));
    }
    up.captured.lock().unwrap().push(Captured {
        kind: "http".into(),
        headers: header_pairs(&headers),
        body: String::from_utf8_lossy(&body).into_owned(),
    });
    let Some(reply) = up.script.lock().unwrap().pop_front() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let status = reply["status"].as_u64().unwrap_or(0) as u16;
    if status != 0 {
        return (
            StatusCode::from_u16(status).unwrap(),
            [("content-type", "application/json")],
            reply["body"].as_str().unwrap_or_default().to_owned(),
        )
            .into_response();
    }
    let mut sse = String::new();
    for event in reply["events"].as_array().into_iter().flatten() {
        let event = event.as_str().unwrap();
        let kind = gjson::get(event, "type");
        sse.push_str(&format!("event: {}\ndata: {event}\n\n", kind.str()));
    }
    ([("content-type", "text/event-stream")], sse).into_response()
}

async fn upstream_socket(up: Arc<Upstream>, mut socket: AxSocket) {
    while let Some(Ok(message)) = socket.recv().await {
        let AxMessage::Text(text) = message else {
            continue;
        };
        up.captured.lock().unwrap().push(Captured {
            kind: "ws_frame".into(),
            headers: Vec::new(),
            body: text.as_str().to_owned(),
        });
        let Some(reply) = up.script.lock().unwrap().pop_front() else {
            continue;
        };
        for event in reply["events"].as_array().into_iter().flatten() {
            let _ = socket.send(AxMessage::Text(event.as_str().unwrap().into())).await;
        }
        let then = reply["then"].as_str().unwrap_or_default();
        if !then.is_empty() {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let close = |code, reason: &str| {
            AxMessage::Close(Some(CloseFrame {
                code,
                reason: reason.into(),
            }))
        };
        match then {
            "close" => {
                let _ = socket.send(close(1000, "bye")).await;
                return;
            }
            "close1009" => {
                let _ = socket.send(close(1009, "too big")).await;
                return;
            }
            "drop" => return,
            _ => {}
        }
    }
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("127.0.0.1:{}", addr.port())
}

/// Go's credentials as `codex-api-key` config entries, loaded the way the server loads
/// them (`cpa_core::config::credentials::load`).
fn config(scenario: &Value, upstream: &str) -> Config {
    // The scenario's own Go config (top-level keys), then its credentials.
    let mut yaml = scenario["config"].as_str().unwrap_or_default().to_owned();
    yaml.push_str("codex-api-key:\n");
    for cred in scenario["credentials"].as_array().unwrap() {
        let attrs = &cred["attributes"];
        yaml.push_str(&format!(
            "  - api-key: {}\n    base-url: http://{upstream}\n",
            attrs["api_key"].as_str().unwrap()
        ));
        if attrs["websockets"].as_str() == Some("true") {
            yaml.push_str("    websockets: true\n");
        }
        yaml.push_str("    models:\n");
        for model in cred["models"].as_array().unwrap() {
            yaml.push_str(&format!("      - name: {}\n", model.as_str().unwrap()));
        }
    }
    Config::parse(&yaml).unwrap()
}

/// Random values Go and Rust both generate: UUIDs and the prewarm timestamp.
fn mask(s: &str) -> String {
    static UUID: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap());
    static CREATED: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r#""created_at":\d+"#).unwrap());
    let s = UUID.replace_all(s, "<uuid>");
    CREATED.replace_all(&s, r#""created_at":0"#).into_owned()
}

#[derive(Debug, PartialEq)]
enum Frame {
    Text(String),
    Close(u16, String),
}

fn go_frames(step: &Value) -> Vec<Frame> {
    step["frames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| match f["text"].as_str() {
            Some(text) => Frame::Text(mask(text)),
            None => Frame::Close(
                f["close"].as_u64().unwrap() as u16,
                f["reason"].as_str().unwrap_or_default().to_owned(),
            ),
        })
        .collect()
}

/// Headers both implementations must agree on. Transport headers (WebSocket key,
/// extensions, connection, length, encoding) legitimately differ between gorilla/net
/// http and wreq.
const COMPARED_HEADERS: [&str; 10] = [
    "authorization",
    "openai-beta",
    "originator",
    "user-agent",
    "x-codex-turn-state",
    "session_id",
    "session-id",
    "conversation_id",
    "accept",
    "content-type",
];

fn compared(headers: impl Iterator<Item = (String, String)>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = headers
        .filter(|(k, _)| COMPARED_HEADERS.contains(&k.as_str()))
        .map(|(k, v)| (k, mask(&v)))
        .collect();
    out.sort();
    out
}

async fn run(name: &str) {
    let scenario = FIXTURE
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("no scenario {name}"));
    let up = Arc::new(Upstream::default());
    for step in scenario["steps"].as_array().unwrap() {
        for reply in step["upstream"].as_array().into_iter().flatten() {
            up.script.lock().unwrap().push_back(reply.clone());
        }
    }
    let upstream = serve(axum::Router::new().fallback(upstream_handler).with_state(up.clone())).await;
    let cfg = config(scenario, &upstream);
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    assert_eq!(credentials.len(), scenario["credentials"].as_array().unwrap().len());
    // Synthesis carries each key's name-only `models` entries, as Go's registry reads them.
    for (credential, go) in credentials.iter().zip(scenario["credentials"].as_array().unwrap()) {
        let models: Vec<Value> = go["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| serde_json::json!({ "name": m }))
            .collect();
        assert_eq!(credential.metadata["models"], Value::Array(models));
    }
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(Runtime::new(cfg, credentials, executors));
    // Like main.rs (and Go's global registry): translators and the Codex client rewrites
    // read this runtime's models. One scenario at a time owns the process-wide overlay.
    static REGISTRY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _registry = REGISTRY.lock().await;
    cpa_server::install_registry(&rt);
    let proxy = serve(router(rt)).await;

    let mut request = wreq::Client::new().websocket(format!("ws://{proxy}/v1/responses"));
    for (k, v) in scenario["client_headers"].as_object().into_iter().flatten() {
        request = request.header(k.as_str(), v.as_str().unwrap());
    }
    let response = request.send().await.unwrap();
    let turn_state = response
        .headers()
        .get("x-codex-turn-state")
        .map(|v| v.to_str().unwrap().to_owned());
    assert_eq!(
        turn_state.as_deref(),
        scenario["upgrade_turn_state"].as_str(),
        "{name}: upgrade echoes x-codex-turn-state"
    );
    let mut socket = response.into_websocket().await.unwrap();

    let mut last_id = String::new();
    let mut closed = false;
    for (i, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
        let expected = go_frames(step);
        if closed {
            assert!(expected.is_empty(), "{name} step {i}: Go read after close");
            continue;
        }
        if let Some(send) = step["send"].as_str() {
            let send = send.replace("{{last_response_id}}", &last_id);
            socket.send(Message::text(send)).await.unwrap();
        }
        let mode = step["read"].as_str().unwrap();
        let mut got = Vec::new();
        loop {
            let next = tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .unwrap_or_else(|_| panic!("{name} step {i}: read timed out after {got:?}"));
            let text = match next {
                Some(Ok(Message::Text(text))) => text.as_str().to_owned(),
                Some(Ok(Message::Close(frame))) => {
                    let (code, reason) = frame.map_or((1005, String::new()), |f| {
                        (u16::from(f.code), f.reason.as_str().to_owned())
                    });
                    got.push(Frame::Close(code, reason));
                    closed = true;
                    break;
                }
                Some(Ok(_)) => continue,
                // TCP closed without a close frame: gorilla reports 1006.
                Some(Err(_)) | None => {
                    got.push(Frame::Close(1006, "unexpected EOF".into()));
                    closed = true;
                    break;
                }
            };
            let kind = gjson::get(&text, "type").str().to_owned();
            if kind == "response.completed" || kind == "response.done" {
                last_id = gjson::get(&text, "response.id").str().to_owned();
            }
            got.push(Frame::Text(mask(&text)));
            if mode == "one"
                || (mode == "completed"
                    && matches!(
                        kind.as_str(),
                        "response.completed" | "response.done" | "response.incomplete"
                    ))
            {
                break;
            }
        }
        assert_eq!(got, expected, "{name} step {i}: client frames");
    }

    tokio::time::sleep(Duration::from_millis(200)).await;
    let captured = up.captured.lock().unwrap().clone();
    let go: Vec<&Value> = scenario["upstream"].as_array().unwrap().iter().collect();
    let kinds = |c: &[Captured]| c.iter().map(|c| c.kind.clone()).collect::<Vec<_>>();
    let go_kinds: Vec<String> = go.iter().map(|c| c["kind"].as_str().unwrap().to_owned()).collect();
    assert_eq!(
        kinds(&captured),
        go_kinds,
        "{name}: upstream dials, frames and requests"
    );
    for (i, (rust, go)) in captured.iter().zip(&go).enumerate() {
        assert_eq!(
            mask(&rust.body),
            mask(go["body"].as_str().unwrap_or_default()),
            "{name}: upstream body #{i}"
        );
        let go_headers = go["headers"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()));
        assert_eq!(
            compared(rust.headers.clone().into_iter()),
            compared(go_headers),
            "{name}: upstream headers #{i}"
        );
    }
}

#[tokio::test]
async fn ws_two_turns_reuse_one_upstream_socket() {
    run("ws_two_turns").await;
}

#[tokio::test]
async fn http_mode_merges_the_transcript_each_turn() {
    run("http_incremental").await;
}

#[tokio::test]
async fn request_shape_errors_keep_the_connection_open() {
    run("previous_not_found_then_create").await;
}

#[tokio::test]
async fn local_prewarm_then_followup_sends_the_warmup_input() {
    run("prewarm_then_followup").await;
}

#[tokio::test]
async fn upstream_request_fault_is_shown_then_closed() {
    run("ws_request_fault_exposed").await;
}

#[tokio::test]
async fn quota_failure_closes_silently() {
    run("ws_quota_silent_close").await;
}

#[tokio::test]
async fn continuation_on_another_transport_closes_1012() {
    run("ws_continuation_needs_replay").await;
}

#[tokio::test]
async fn upstream_loss_between_turns_closes_the_client() {
    run("ws_upstream_closes_between_turns").await;
}

#[tokio::test]
async fn upstream_1009_reaches_the_client_as_1009() {
    run("ws_upstream_message_too_big").await;
}

#[tokio::test]
async fn stream_ending_before_completion_closes_silently() {
    run("http_stream_ends_early").await;
}

#[tokio::test]
async fn ws_multi_agent_v2_renames_upstream_and_restores_for_the_client() {
    run("ws_multi_agent_v2").await;
}

#[tokio::test]
async fn http_multi_agent_v2_renames_upstream_and_restores_for_the_client() {
    run("http_multi_agent_v2").await;
}

#[tokio::test]
async fn http_upstream_400_is_shown_then_closed() {
    run("http_upstream_400_exposed").await;
}

#[test]
fn every_go_scenario_has_a_test() {
    let names: Vec<&str> = FIXTURE.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "ws_two_turns",
            "http_incremental",
            "previous_not_found_then_create",
            "prewarm_then_followup",
            "ws_request_fault_exposed",
            "ws_quota_silent_close",
            "ws_continuation_needs_replay",
            "ws_upstream_closes_between_turns",
            "ws_upstream_message_too_big",
            "http_stream_ends_early",
            "ws_multi_agent_v2",
            "http_multi_agent_v2",
            "http_upstream_400_exposed",
        ]
    );
}
