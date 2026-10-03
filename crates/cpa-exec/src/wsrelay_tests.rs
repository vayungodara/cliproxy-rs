//! The relay protocol against `tests/fixtures/aistudio_go.json`: what gorilla's
//! `ReadJSON` and `WriteJSON` (encoding/json) make of frames and messages, then session
//! behaviour from internal/wsrelay/session.go.

use std::time::Duration;

use serde_json::Value;

use super::*;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../tests/fixtures/aistudio_go.json")).unwrap()
}

#[test]
fn frames_decode_like_go_read_json() {
    let fixture = fixture();
    let vectors = fixture["decode"].as_array().unwrap();
    assert!(vectors.len() >= 20);
    for v in vectors {
        let frame = v["frame"].as_str().unwrap();
        let got = Message::decode(frame.as_bytes());
        if v["error"] == true {
            assert!(got.is_err(), "{frame}: {got:?}");
            continue;
        }
        let msg = got.unwrap_or_else(|e| panic!("{frame}: {e}"));
        assert_eq!(msg.id, v["id"].as_str().unwrap_or_default(), "{frame}: id");
        assert_eq!(msg.kind, v["type"].as_str().unwrap_or_default(), "{frame}: type");
        let payload = msg
            .payload
            .map(|p| String::from_utf8(GoValue::Object(p).marshal()).unwrap());
        assert_eq!(payload.as_deref(), v["payload"].as_str(), "{frame}: payload");
    }
}

#[test]
fn messages_encode_like_go_write_json() {
    let fixture = fixture();
    for v in fixture["encode"].as_array().unwrap() {
        let payload =
            v.get("payload")
                .and_then(Value::as_object)
                .map(|p| match GoValue::from_json(&Value::Object(p.clone())) {
                    GoValue::Object(map) => map,
                    _ => unreachable!(),
                });
        let msg = Message {
            id: v["id"].as_str().unwrap().into(),
            kind: v["type"].as_str().unwrap().into(),
            payload,
        };
        assert_eq!(msg.encode(), v["text"].as_str().unwrap());
    }
}

/// A frame the route would write, decoded.
async fn next(frames: &mut mpsc::UnboundedReceiver<String>) -> Message {
    let frame = tokio::time::timeout(Duration::from_secs(5), frames.recv())
        .await
        .unwrap()
        .unwrap();
    Message::decode(frame.as_bytes()).unwrap()
}

#[tokio::test]
async fn ping_gets_pong_and_requests_complete_on_terminal_messages() {
    let relay = Arc::new(Relay::default());
    let (session, mut frames) = relay.connect_as("AIStudio-X".into());
    assert_eq!(session.provider(), "aistudio-x");
    session.receive(br#"{"id":"p","type":"ping"}"#).await.unwrap();
    assert_eq!(
        next(&mut frames).await,
        Message {
            id: "p".into(),
            kind: PONG.into(),
            payload: None
        }
    );
    let request = HttpRequest {
        method: "POST".into(),
        url: "https://example.invalid/".into(),
        ..HttpRequest::default()
    };
    // Lookup trims and lower-cases the provider name.
    let relay2 = relay.clone();
    let call = tokio::spawn(async move { relay2.non_stream(" AISTUDIO-x ", &request).await });
    let sent = next(&mut frames).await;
    assert_eq!(sent.kind, HTTP_REQUEST);
    // Messages for other IDs are ignored; a stream answer collects until stream_end.
    session
        .receive(br#"{"id":"other","type":"http_response"}"#)
        .await
        .unwrap();
    for frame in [
        format!(
            r#"{{"id":"{}","type":"stream_start","payload":{{"status":201,"headers":{{"x-a":"1"}}}}}}"#,
            sent.id
        ),
        format!(
            r#"{{"id":"{}","type":"stream_chunk","payload":{{"data":"ab"}}}}"#,
            sent.id
        ),
        format!(
            r#"{{"id":"{}","type":"stream_chunk","payload":{{"data":"c"}}}}"#,
            sent.id
        ),
        format!(r#"{{"id":"{}","type":"stream_end"}}"#, sent.id),
    ] {
        session.receive(frame.as_bytes()).await.unwrap();
    }
    let resp = call.await.unwrap().unwrap();
    assert_eq!(resp.status, 201);
    assert_eq!(resp.headers.get("X-A"), Some(&vec!["1".to_owned()]));
    assert_eq!(resp.body, b"abc");
    assert!(
        lock(&session.pending).is_empty(),
        "the terminal message closed the request"
    );
}

#[tokio::test]
async fn closing_fails_waiting_requests_and_notifies_the_observer() {
    let relay = Arc::new(Relay::default());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    relay.set_observer(Arc::new(move |provider, cause| {
        lock(&log).push((provider.to_owned(), cause.map(str::to_owned)));
    }));
    let (first, _frames) = relay.connect_as("aistudio-a".into());
    // A new session of the same name replaces the old one.
    let (second, mut frames) = relay.connect_as("aistudio-a".into());
    assert!(first.is_closed());
    let relay2 = relay.clone();
    let call = tokio::spawn(async move { relay2.non_stream("aistudio-a", &HttpRequest::default()).await });
    next(&mut frames).await;
    // A frame Go cannot decode ends the session and fails the waiting request.
    assert!(second.receive(b"[1]").await.is_err());
    let error = call.await.unwrap().unwrap_err();
    assert!(error.ends_with("(status=0)"), "{error}");
    assert!(relay.providers().is_empty());
    let seen = lock(&seen).clone();
    assert_eq!(seen[0], ("aistudio-a".into(), None));
    assert_eq!(
        seen[1],
        ("aistudio-a".into(), Some("replaced by new connection".into()))
    );
    assert_eq!(seen[2], ("aistudio-a".into(), None));
    assert_eq!(seen.len(), 4);
    assert_eq!(
        relay
            .non_stream("aistudio-a", &HttpRequest::default())
            .await
            .unwrap_err(),
        "wsrelay: provider aistudio-a not connected"
    );
}
