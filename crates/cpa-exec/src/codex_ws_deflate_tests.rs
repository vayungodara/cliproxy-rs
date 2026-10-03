//! Expected values come from gorilla/websocket as CLIProxyAPI dials Codex
//! (`tests/fixtures/codex_ws_deflate_go.json`, `tests/reference/codex_ws_deflate`): its
//! offer, how it treats each `Sec-WebSocket-Extensions` answer, and what it reads from raw
//! server frames, RFC 7692's examples included.

use super::*;
use std::sync::LazyLock;

use futures_util::StreamExt;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::Role;

static GO: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../tests/fixtures/codex_ws_deflate_go.json")).unwrap());

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// Text messages read from `bytes` (followed by EOF) through [`Inflate`] and tungstenite.
async fn read(bytes: &[u8], limit: usize) -> Vec<String> {
    let (mut server, client) = tokio::io::duplex(1 << 20);
    server.write_all(bytes).await.unwrap();
    // EOF for the reader; the server half stays open so pong writes succeed.
    server.shutdown().await.unwrap();
    let mut socket = WebSocketStream::from_raw_socket(Inflate::new(client, true, limit), Role::Client, None).await;
    let mut messages = Vec::new();
    while let Some(Ok(message)) = socket.next().await {
        match message {
            Message::Text(text) => messages.push(text.as_str().to_owned()),
            Message::Binary(bytes) => messages.push(String::from_utf8_lossy(&bytes).into_owned()),
            _ => {}
        }
    }
    drop(server);
    messages
}

#[test]
fn offer_and_negotiation_match_gorilla() {
    for case in GO["negotiations"].as_array().unwrap() {
        assert_eq!(case["offer"], OFFER);
        let mut headers = HeaderMap::new();
        for value in case["headers"].as_array().into_iter().flatten() {
            headers.append(
                http::header::SEC_WEBSOCKET_EXTENSIONS,
                value.as_str().unwrap().parse().unwrap(),
            );
        }
        // Go reads the compressed "Hello" only when compression was agreed.
        let want = if case["dial_error"].is_string() {
            Err(())
        } else {
            Ok(!case["messages"].as_array().unwrap().is_empty())
        };
        assert_eq!(negotiated(&headers), want, "{case}");
    }
}

#[tokio::test]
async fn server_frames_read_like_gorilla() {
    for case in GO["frames"].as_array().unwrap() {
        let messages = read(&hex(case["bytes"].as_str().unwrap()), 64 << 20).await;
        let want: Vec<String> = serde_json::from_value(case["messages"].clone()).unwrap();
        assert_eq!(messages, want, "{}", case["name"]);
    }
}

/// The bound Go does not have: an inflated message larger than the socket's message
/// limit fails instead of growing without bound.
#[tokio::test]
async fn inflated_messages_are_bounded() {
    let case = GO["frames"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "big_json")
        .unwrap();
    let frame = hex(case["bytes"].as_str().unwrap());
    let message = case["messages"][0].as_str().unwrap();
    assert!(
        frame.len() < message.len() - 1,
        "the frame fits a limit its message exceeds"
    );
    assert_eq!(read(&frame, message.len()).await, [message]);
    assert!(read(&frame, message.len() - 1).await.is_empty());
}

/// Without an agreement the layer is transparent: tungstenite rejects RSV1 as gorilla
/// does when it did not negotiate.
#[tokio::test]
async fn without_agreement_frames_pass_through() {
    let (mut server, client) = tokio::io::duplex(1024);
    server
        .write_all(&hex("810548656c6c6fc107f248cdc9c90700"))
        .await
        .unwrap();
    server.shutdown().await.unwrap();
    let mut socket = WebSocketStream::from_raw_socket(Inflate::new(client, false, 1024), Role::Client, None).await;
    assert_eq!(socket.next().await.unwrap().unwrap(), Message::text("Hello"));
    assert!(socket.next().await.unwrap().is_err());
}
