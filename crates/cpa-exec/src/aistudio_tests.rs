//! Differential tests against `tests/fixtures/aistudio_go.json`, produced by Go's
//! `AIStudioExecutor` over a real `wsrelay.Manager` with a scripted browser connected by
//! websocket (tests/reference/aistudio). Each scenario replays the browser's replies to
//! a Rust relay session and compares the relayed frame, the output, the client stream
//! bytes, errors and the reasoning effort reported for usage. Expected values come only
//! from Go.

use std::sync::Arc;
use std::time::Duration;

use cpa_core::exec::Caller;
use serde_json::Value;

use super::*;
use crate::gemini::tests::{UsageReports, client_bytes};

const FIXTURE: &str = include_str!("../tests/fixtures/aistudio_go.json");
const CHANNEL: &str = "aistudio-test";

fn request(s: &Value) -> ExecRequest {
    let op = s["op"].as_str().unwrap();
    let source = Format::parse(s["source"].as_str().unwrap()).unwrap();
    let body = Bytes::from(s["payload"].as_str().unwrap().to_owned());
    let mut headers = http::HeaderMap::new();
    for (k, v) in s["headers"].as_object().into_iter().flatten() {
        headers.insert(
            http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.as_str().unwrap().parse().unwrap(),
        );
    }
    let model = s["model"].as_str().unwrap().to_owned();
    ExecRequest {
        operation: if op == "count" {
            Operation::CountTokens
        } else {
            Operation::Generate
        },
        source_format: source,
        response_format: source,
        requested_model: model.clone(),
        model,
        original_body: body.clone(),
        body,
        stream: op == "stream",
        alt: s["alt"].as_str().map(str::to_owned),
        session: None,
        execution_session: None,
        derived_session: None,
        resolved_model: None,
        usage: Default::default(),
        request_path: String::new(),
        headers,
        caller: Caller {
            principal: "fake-client-key".into(),
            source: "authorization",
        },
    }
}

/// The frame as Go's browser recorded it: message ID and send time replaced.
fn normalized_frame(frame: &str) -> String {
    let value: Value = serde_json::from_str(frame).unwrap();
    let id = value["id"].as_str().unwrap();
    let sent = value["payload"]["sent_at"].as_str().unwrap();
    assert!(
        chrono::DateTime::parse_from_rfc3339(sent).is_ok(),
        "sent_at is RFC 3339: {sent}"
    );
    frame.replacen(id, "ID", 1).replacen(sent, "SENT_AT", 1)
}

#[tokio::test]
async fn go_reference_scenarios() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let scenarios = fixture["scenarios"].as_array().unwrap();
    assert!(scenarios.len() >= 30, "fixture lost scenarios");
    let relay = Arc::new(Relay::default());
    let (session, mut frames) = relay.connect_as(CHANNEL.into());
    let executor = AiStudioExecutor { relay: relay.clone() };
    // Responses output and OpenAI chunks stamp wall-clock values.
    let created = regex::Regex::new(r#""created(_at)?": ?1[0-9]{9}([,}])"#).unwrap();
    let norm = |t: &str| created.replace_all(t, r#""created$1":<unix>$2"#).into_owned();
    for s in scenarios {
        let name = s["name"].as_str().unwrap();
        let cfg = Config::parse(s["config"].as_str().unwrap_or_default()).unwrap();
        let mut credential = cpa_core::credential::Credential::relay_session(if s["disconnected"] == true {
            "aistudio-missing"
        } else {
            CHANNEL
        });
        for (k, v) in s["attributes"].as_object().into_iter().flatten() {
            credential.attributes.insert(k.clone(), v.as_str().unwrap().to_owned());
        }
        let mut req = request(s);
        let reports = Arc::new(UsageReports::default());
        req.usage = reports.sink();
        let client = req.response_format;
        let alt = req.alt.as_deref().is_some_and(|a| !a.is_empty());
        let replies = s["replies"].as_array().cloned().unwrap_or_default();
        let browser = async {
            let Ok(Some(frame)) = tokio::time::timeout(Duration::from_millis(200), frames.recv()).await else {
                return None;
            };
            let id = serde_json::from_str::<Value>(&frame).unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned();
            for reply in &replies {
                let mut message = serde_json::json!({"id": id, "type": reply["type"]});
                if let Some(payload) = reply.get("payload") {
                    message["payload"] = payload.clone();
                }
                session.receive(message.to_string().as_bytes()).await.unwrap();
            }
            Some(normalized_frame(&frame))
        };
        let run = async {
            let mut output = None;
            let mut streamed = Vec::new();
            let mut error = None;
            match executor.execute(&credential, req, &cfg).await {
                Err(e) => error = Some((e.status, String::from_utf8_lossy(&e.body).into_owned())),
                Ok(response) => match response.body {
                    ResponseBody::Buffered(bytes) => output = Some(String::from_utf8(bytes.to_vec()).unwrap()),
                    ResponseBody::Stream(mut stream) => {
                        while let Some(item) = stream.next().await {
                            match item {
                                Ok(bytes) => streamed.extend_from_slice(&bytes),
                                Err(e) => error = Some((e.status, String::from_utf8_lossy(&e.body).into_owned())),
                            }
                        }
                    }
                },
            }
            (output, streamed, error)
        };
        let (frame, (output, streamed, error)) = tokio::join!(browser, run);
        let want_frames: Vec<String> = s["frames"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|f| f.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            frame.into_iter().collect::<Vec<_>>(),
            want_frames,
            "{name}: relayed frame"
        );
        // A Go error without a status code is answered 500 by Go's handler.
        let want_error = s["error"].as_object().map(|e| {
            let status = e["status"].as_u64().unwrap() as u16;
            (
                if status == 0 { 500 } else { status },
                e["message"].as_str().unwrap().to_owned(),
            )
        });
        assert_eq!(error, want_error, "{name}: error");
        assert_eq!(
            output.as_deref().map(norm),
            s["output"].as_str().map(norm),
            "{name}: output"
        );
        let want_stream = client_bytes(client, alt, s["chunks"].as_array().map_or(&[][..], Vec::as_slice));
        assert_eq!(
            norm(&String::from_utf8_lossy(&streamed)),
            norm(&String::from_utf8_lossy(&want_stream)),
            "{name}: stream"
        );
        reports.check(s);
    }
    assert!(!session.is_closed());
}

/// Go's `ensureColonSpacedJSON` on the shapes its comment names: escaped quotes inside
/// strings, nested containers, and non-JSON passed through.
#[test]
fn colon_spacing_matches_go_rules() {
    let spaced = ensure_colon_spaced_json(br#"{"b":[1,{"x":"a\"b: c\\"}],"a":1.50,"e":{},"f":[]}"#);
    assert_eq!(
        String::from_utf8(spaced).unwrap(),
        r#"{"a": 1.5,"b": [1,{"x": "a\"b: c\\"}],"e": {},"f": []}"#
    );
    assert_eq!(ensure_colon_spaced_json(b"data: {\"a\":1}"), b"data: {\"a\":1}");
    assert_eq!(ensure_colon_spaced_json(b"  "), b"  ");
}
