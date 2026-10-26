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
use crate::gemini::tests::{Captured, Captures, UsageReports, client_bytes};

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
        stream_payloads: false,
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
            provider: String::new(),
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
    // The scenarios recorded again (or added) at v8.0.35 replace their base recordings.
    let fixture = crate::payload_barrier::with_v8_0_35(
        FIXTURE,
        include_str!("../tests/fixtures/aistudio_go_v8.0.35.json"),
        "scenarios",
    );
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
        let captures = Arc::new(Captures::default());
        req.usage = reports.sink().with_capture(captures.sink());
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
                    ResponseBody::Stream(mut stream) => loop {
                        // `cancel_when_idle`: the client goes away once nothing arrives.
                        let next = if s["cancel_when_idle"] == true {
                            match tokio::time::timeout(Duration::from_millis(200), stream.next()).await {
                                Ok(next) => next,
                                Err(_) => break,
                            }
                        } else {
                            stream.next().await
                        };
                        match next {
                            Some(Ok(bytes)) => streamed.extend_from_slice(&bytes),
                            Some(Err(e)) => error = Some((e.status, String::from_utf8_lossy(&e.body).into_owned())),
                            None => break,
                        }
                    },
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
        check_capture(s, &captures.events());
    }
    assert!(!session.is_closed());
}

/// Go's capture for a relayed attempt: the request as the browser received it, the
/// first status-bearing reply's metadata, every non-empty reply payload, and one error
/// per relay error.
fn check_capture(s: &Value, events: &[Captured]) {
    let name = s["name"].as_str().unwrap();
    if s["error"]["status"] == 501 {
        assert!(events.is_empty(), "{name}: {events:?}");
        return;
    }
    let Some(Captured::Request {
        url,
        headers,
        body,
        provider,
        auth_id,
        auth_type,
        auth_value,
    }) = events.first()
    else {
        panic!("{name}: no request capture in {events:?}");
    };
    let channel = if s["disconnected"] == true {
        "aistudio-missing"
    } else {
        CHANNEL
    };
    assert_eq!(
        (
            provider.as_str(),
            auth_id.as_str(),
            auth_type.as_str(),
            auth_value.as_str()
        ),
        ("aistudio", channel, "oauth", channel),
        "{name}: account"
    );
    if let Some(frame) = s["frames"].as_array().and_then(|f| f.first()) {
        let frame: Value = serde_json::from_str(frame.as_str().unwrap()).unwrap();
        let payload = &frame["payload"];
        assert_eq!(url, payload["url"].as_str().unwrap(), "{name}: url");
        assert_eq!(
            String::from_utf8_lossy(body),
            payload["body"].as_str().unwrap(),
            "{name}: body"
        );
        let mut want: Vec<(String, String)> = Vec::new();
        for (k, values) in payload["headers"].as_object().unwrap() {
            for v in values.as_array().unwrap() {
                want.push((k.clone(), v.as_str().unwrap().to_owned()));
            }
        }
        assert_eq!(headers, &want, "{name}: headers");
    }
    let rest = &events[1..];
    let replies = s["replies"].as_array().cloned().unwrap_or_default();
    let status_of = |r: &Value| match r.get("payload") {
        None => 502,
        Some(p) => p["status"].as_u64().unwrap_or(200) as u16,
    };
    let want_status = replies
        .iter()
        .find(|r| matches!(r["type"].as_str(), Some("http_response" | "stream_start")))
        .map(status_of);
    let statuses: Vec<u16> = rest
        .iter()
        .filter_map(|e| match e {
            Captured::Metadata(status) => Some(*status),
            _ => None,
        })
        .collect();
    assert_eq!(
        statuses,
        want_status.into_iter().collect::<Vec<_>>(),
        "{name}: metadata"
    );
    let chunks: Vec<u8> = rest
        .iter()
        .filter_map(|e| match e {
            Captured::Chunk(c) => Some(c.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    let payloads: Vec<u8> = replies
        .iter()
        .filter_map(|r| r["payload"]["data"].as_str().or(r["payload"]["body"].as_str()))
        .flat_map(|p| p.bytes())
        .collect();
    assert_eq!(
        String::from_utf8_lossy(&chunks),
        String::from_utf8_lossy(&payloads),
        "{name}: chunks"
    );
    let errors = rest.iter().filter(|e| matches!(e, Captured::Error(_))).count();
    // Go records relay errors, a vanished relay, and (5ec31442) a non-2xx answer relayed
    // after the stream started.
    let late_status_errors = replies
        .iter()
        .skip(1)
        .filter(|r| r["type"] == "http_response")
        .filter(|r| !(200..300).contains(&r["payload"]["status"].as_i64().unwrap_or(0)))
        .count();
    // A retained Claude error event (7f2f17d7) ends the stream ahead of the relay error,
    // which Go then never records.
    let retained = s["error"]["message"]
        .as_str()
        .is_some_and(|m| m.starts_with(r#"{"type":"error""#));
    let relay_errors = if retained {
        0
    } else {
        replies.iter().filter(|r| r["type"] == "error").count()
    };
    let want_errors = relay_errors
        + usize::from(s["disconnected"] == true)
        + if s["op"] == "stream" { late_status_errors } else { 0 };
    assert_eq!(errors, want_errors, "{name}: errors in {rest:?}");
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

/// TestPayloadBarrierAIStudio, TestAIStudioTranslateRequestPayloadOverrideWinsThinkingNormalization
/// and TestAIStudioCountPayloadAfterCleanup (868e137): the rules run after the
/// `generationConfig` cleanup, the boundary turns and the thinking-level normalization,
/// and after the count request's deletes.
#[test]
fn payload_rules_run_after_the_cleanup() {
    let cfg = Config::parse(
        r#"
payload:
  override:
    - models: [{name: "*"}]
      params:
        "generationConfig.maxOutputTokens": 900000
        "generationConfig.thinkingConfig.thinkingLevel": low
        "safetySettings": [{category: configured}]
  filter:
    - models: [{name: "*"}]
      params: ["contents"]
"#,
    )
    .unwrap();
    let payload = r#"{"contents":[{"role":"model","parts":[{"text":"hi"}]}]}"#;
    for op in ["execute", "stream", "count"] {
        let req = request(&serde_json::json!({
            "op": op, "source": "gemini", "model": "gemini-3.1-pro-preview", "payload": payload,
        }));
        let (body, _) = translate(&req, &cfg, op == "count").unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(!gj::get(&body, "contents").exists(), "{op}: {text}");
        assert_eq!(
            gj::get(&body, "generationConfig.maxOutputTokens").int(),
            900000,
            "{op}: {text}"
        );
        assert_eq!(
            gj::get(&body, "generationConfig.thinkingConfig.thinkingLevel").str(),
            "low",
            "{op}: {text}"
        );
        assert_eq!(
            gj::get(&body, "safetySettings.0.category").str(),
            "configured",
            "{op}: {text}"
        );
    }
}

/// Go translates the original (baseline) body without checking its error: only the
/// working body's refusal refuses the request.
#[test]
fn a_refused_baseline_alone_does_not_refuse_the_request() {
    let cfg = Config::parse("").unwrap();
    let s = serde_json::json!({"op": "execute", "source": "claude", "model": "gemini-2.5-pro", "payload": r#"{"model":"gemini-2.5-pro","max_tokens":16,"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#});
    let mut req = request(&s);
    req.original_body = Bytes::from_static(br#"{"model":"gemini-2.5-pro","max_tokens":16,"messages":[{"role":"user","content":[{"type":"container_upload","source":{"type":"file","file_id":"f1"}}]}]}"#);
    let (body, action) = translate(&req, &cfg, false).unwrap();
    assert_eq!(action, "generateContent");
    assert_eq!(cpa_common::json::get(&body, "contents.0.parts.0.text").str(), "hi");
    std::mem::swap(&mut req.body, &mut req.original_body);
    assert!(translate(&req, &cfg, false).is_err());
}
