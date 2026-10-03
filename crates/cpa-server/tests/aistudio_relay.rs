//! The `/v1/ws` relay end to end. A test websocket client plays the AI Studio browser:
//! connecting gives the runtime its `aistudio` credential, a client request for a
//! Gemini model travels to the browser and back, and the credential disappears when
//! the browser leaves. `ws-auth` decides whether the handshake needs a client key.
//! Byte-level parity of the relayed requests is checked in cpa-exec (aistudio_tests.rs).

use std::sync::Arc;
use std::time::Duration;

use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::{Runtime, router};
use futures_util::StreamExt;
use serde_json::Value;
use wreq::ws::message::Message;

const ANSWER: &str = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}],"modelVersion":"gemini-2.5-flash"}"#;

async fn serve(rt: Arc<Runtime>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(listener, router(rt)).await.unwrap() });
    addr
}

fn runtime(config: &str) -> Arc<Runtime> {
    let executors = Executors {
        claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let config = Config::parse(config).unwrap();
    Arc::new(cpa_server::testing::runtime(config, Vec::new(), executors))
}

/// The relay credentials the runtime holds, waiting up to five seconds for `want`.
async fn relay_credentials(rt: &Runtime, want: usize) -> Vec<String> {
    for _ in 0..500 {
        let ids: Vec<String> = rt
            .store()
            .snapshot()
            .iter()
            .filter(|c| c.provider == "aistudio")
            .map(|c| c.id.clone())
            .collect();
        if ids.len() == want {
            return ids;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("expected {want} relay credentials");
}

async fn next_text(socket: &mut wreq::ws::WebSocket) -> String {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .unwrap()
        {
            Some(Ok(Message::Text(text))) => return text.as_str().to_owned(),
            Some(Ok(_)) => continue,
            other => panic!("socket ended: {other:?}"),
        }
    }
}

#[tokio::test]
async fn browser_session_serves_requests_until_it_leaves() {
    let rt = runtime("access:\n  api-keys: [client-key]\n");
    let addr = serve(rt.clone()).await;
    let client = wreq::Client::new();

    // ws-auth is on by default: no key, no session.
    let refused = client.websocket(format!("ws://{addr}/v1/ws")).send().await.unwrap();
    assert_eq!(refused.status(), 401);
    assert!(relay_credentials(&rt, 0).await.is_empty());

    let response = client
        .websocket(format!("ws://{addr}/v1/ws"))
        .header("authorization", "Bearer client-key")
        .send()
        .await
        .unwrap();
    let mut socket = response.into_websocket().await.unwrap();
    let ids = relay_credentials(&rt, 1).await;
    let id = &ids[0];
    assert!(
        id.len() == 25
            && id.starts_with("aistudio-")
            && id[9..].bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
        "{id}"
    );
    let credential = rt.store().get(id).unwrap();
    assert_eq!(credential.label, *id);
    assert_eq!(credential.attributes["runtime_only"], "true");

    // A Gemini request goes to the browser as an http_request message.
    let url = format!("http://{addr}/v1beta/models/gemini-2.5-flash:generateContent");
    let call = tokio::spawn(async move {
        let res = wreq::Client::new()
            .post(url)
            .header("authorization", "Bearer client-key")
            .header("content-type", "application/json")
            .body(r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#)
            .send()
            .await
            .unwrap();
        (res.status().as_u16(), res.text().await.unwrap())
    });
    let frame = next_text(&mut socket).await;
    assert!(frame.ends_with('\n'), "WriteJSON ends the frame with a newline");
    let request: Value = serde_json::from_str(&frame).unwrap();
    assert_eq!(request["type"], "http_request");
    assert_eq!(
        request["payload"]["url"],
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent"
    );
    let reply = serde_json::json!({
        "id": request["id"],
        "type": "http_response",
        "payload": {"status": 200, "headers": {"Content-Type": ["application/json"]}, "body": ANSWER},
    });
    socket.send(Message::text(reply.to_string())).await.unwrap();
    let (status, text) = call.await.unwrap();
    assert_eq!(status, 200, "{text}");
    // ensureColonSpacedJSON: keys sorted, a space after each colon.
    assert_eq!(
        text,
        r#"{"candidates": [{"content": {"parts": [{"text": "ok"}],"role": "model"},"finishReason": "STOP"}],"modelVersion": "gemini-2.5-flash"}"#
    );

    // Pings from the browser get pongs with the same ID.
    socket
        .send(Message::text(r#"{"id":"p1","type":"ping"}"#))
        .await
        .unwrap();
    assert_eq!(next_text(&mut socket).await, "{\"id\":\"p1\",\"type\":\"pong\"}\n");

    // Leaving removes the credential.
    socket.close(1000u16, "bye").await.unwrap();
    assert!(relay_credentials(&rt, 0).await.is_empty());
}

#[tokio::test]
async fn ws_auth_off_admits_browsers_without_a_key() {
    let rt = runtime("access:\n  api-keys: [client-key]\noauth:\n  providers:\n    aistudio:\n      ws-auth: false\n");
    let addr = serve(rt.clone()).await;
    // A plain GET is not a handshake: gorilla's 400.
    let plain = wreq::Client::new()
        .get(format!("http://{addr}/v1/ws"))
        .send()
        .await
        .unwrap();
    assert_eq!(plain.status(), 400);
    assert_eq!(plain.headers()["sec-websocket-version"], "13");
    assert_eq!(plain.text().await.unwrap(), "Bad Request\n");
    let connect = || async {
        let response = wreq::Client::new()
            .websocket(format!("ws://{addr}/v1/ws"))
            .send()
            .await
            .unwrap();
        response.into_websocket().await.unwrap()
    };
    let mut socket = connect().await;
    relay_credentials(&rt, 1).await;
    // A frame Go cannot decode ends the session and its credential.
    socket.send(Message::text("[1]")).await.unwrap();
    assert!(relay_credentials(&rt, 0).await.is_empty());

    let _socket = connect().await;
    relay_credentials(&rt, 1).await;
    // The relay credential has no file: a watcher reconcile keeps it.
    rt.store().reconcile(Vec::new());
    relay_credentials(&rt, 1).await;
    // Turning ws-auth on ends the sessions that connected without a key.
    rt.publish_config(Config::parse("access:\n  api-keys: [client-key]\n").unwrap());
    assert!(relay_credentials(&rt, 0).await.is_empty());
}
