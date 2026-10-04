//! Differential tests against `tests/fixtures/xai_go.json`, produced by the real Go
//! XAIExecutor (tests/reference/xai). Scenarios run in order on one executor, as the Go
//! generator runs them in one process, so the reasoning replay cache carries from one
//! turn to the next. Each scenario replays the scripted upstream answer and compares the
//! URL the executor chose, the raw upstream request and the executor's output. Expected
//! values come only from Go.

use std::sync::{Arc, Mutex};

use cpa_core::exec::Caller;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::*;
use crate::openai_compat_usage::{HttpAttempt, Logged, http_capture_problems, published};

const FIXTURE: &str = include_str!("../tests/fixtures/xai_go.json");

/// Shared helpers whose real port has not landed: scenarios that need them are skipped
/// and listed, so integration can remove an entry and see the scenario run.
const PENDING: &[&str] = &[];

/// One-shot raw HTTP/1.1 capture server answering with the scripted response.
struct Mock {
    addr: String,
    raw: Arc<Mutex<Option<Vec<u8>>>>,
}

impl Mock {
    async fn start(up: &Value) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let raw: Arc<Mutex<Option<Vec<u8>>>> = Arc::default();
        let sink = raw.clone();
        let status = up["status"].as_u64().unwrap() as u16;
        let headers: Vec<(String, String)> = up["headers"]
            .as_array()
            .map(|hs| {
                hs.iter()
                    .map(|h| (h[0].as_str().unwrap().to_owned(), h[1].as_str().unwrap().to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        let body = up["body"].as_str().unwrap().as_bytes().to_vec();
        tokio::spawn(async move {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let (read, mut write) = socket.into_split();
            let mut reader = BufReader::new(read);
            let mut captured = Vec::new();
            let mut length = 0;
            loop {
                let mut line = Vec::new();
                if reader.read_until(b'\n', &mut line).await.unwrap_or(0) == 0 {
                    break;
                }
                captured.extend_from_slice(&line);
                let text = String::from_utf8_lossy(&line).to_ascii_lowercase();
                if let Some(v) = text.strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
                if line == b"\r\n" {
                    break;
                }
            }
            let mut payload = vec![0; length];
            reader.read_exact(&mut payload).await.unwrap();
            captured.extend_from_slice(&payload);
            *sink.lock().unwrap() = Some(captured);
            let reason = http::StatusCode::from_u16(status)
                .ok()
                .and_then(|s| s.canonical_reason())
                .unwrap_or("");
            let mut out = format!("HTTP/1.1 {status} {reason}\r\n").into_bytes();
            for (n, v) in &headers {
                out.extend_from_slice(format!("{n}: {v}\r\n").as_bytes());
            }
            out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
            out.extend_from_slice(b"Connection: close\r\n\r\n");
            out.extend_from_slice(&body);
            let _ = write.write_all(&out).await;
            let _ = write.shutdown().await;
        });
        Self { addr, raw }
    }

    fn request(&self) -> Option<String> {
        let raw = self.raw.lock().unwrap().clone()?;
        Some(String::from_utf8_lossy(&raw).replace(&self.addr, "UPSTREAM"))
    }
}

fn credential(s: &Value, cfg: &Config) -> Credential {
    let index = s["config_auth"].as_i64().unwrap();
    if index >= 0 {
        let xai: Vec<Credential> = cpa_core::config::credentials::from_config(cfg)
            .into_iter()
            .filter(|c| c.provider == "xai")
            .collect();
        return xai[index as usize].clone();
    }
    let mut metadata = s["metadata"].as_object().cloned().unwrap_or_default();
    metadata.insert("type".into(), "xai".into());
    let mut c = Credential::from_file(
        std::path::Path::new("/fixture"),
        std::path::Path::new("/fixture/xai.json"),
        metadata,
    )
    .unwrap();
    for (k, v) in s["attributes"].as_object().into_iter().flatten() {
        c.attributes.insert(k.clone(), v.as_str().unwrap().to_owned());
    }
    c
}

fn response_format(s: &Value) -> Format {
    let source = Format::parse(s["source"].as_str().unwrap()).unwrap();
    s["response"].as_str().and_then(Format::parse).unwrap_or(source)
}

/// The dispatch loop's model binding, as the generator bound it.
fn bound_model(v: &Value) -> Option<cpa_core::exec::ResolvedModel> {
    Some(cpa_core::exec::ResolvedModel {
        info: cpa_core::registry::ModelInfo::from_raw(v.as_object()?.clone()).expect("resolved model info"),
        source: cpa_core::exec::ResolvedSource::ApiKey,
    })
}

fn request(s: &Value, usage: cpa_core::exec::UsageSink) -> ExecRequest {
    let op = s["op"].as_str().unwrap();
    let source = Format::parse(s["source"].as_str().unwrap()).unwrap();
    let payload = Bytes::from(s["payload"].as_str().unwrap().to_owned());
    let original = s["original"]
        .as_str()
        .map(|o| Bytes::from(o.to_owned()))
        .unwrap_or_default();
    let mut headers = http::HeaderMap::new();
    for (k, v) in s["headers"].as_object().into_iter().flatten() {
        headers.insert(
            http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.as_str().unwrap().parse().unwrap(),
        );
    }
    // Go's executor-side fallback (EnsureSessionContext -> CanonicalSessionID).
    let session_body: &[u8] = if original.is_empty() { &payload } else { &original };
    let meta = cpa_common::session::Meta {
        execution_session: s["execution_session"].as_str(),
        derived: s["derived_session"].as_str(),
    };
    let session = Some(cpa_common::session::extract_session_id(&headers, session_body, &meta))
        .filter(|id| !id.is_empty())
        .map(|id| cpa_common::session::bound_session_identity(&id));
    let model = s["model"].as_str().unwrap().to_owned();
    ExecRequest {
        operation: if op == "count" {
            Operation::CountTokens
        } else {
            Operation::Generate
        },
        source_format: source,
        response_format: response_format(s),
        requested_model: s["requested_model"].as_str().unwrap_or(&model).to_owned(),
        model,
        original_body: original,
        body: payload,
        stream: op == "stream",
        alt: s["alt"].as_str().map(str::to_owned),
        session,
        execution_session: s["execution_session"].as_str().map(str::to_owned),
        derived_session: s["derived_session"].as_str().map(str::to_owned),
        resolved_model: bound_model(&s["resolved_model"]),
        usage,
        request_path: s["request_path"].as_str().unwrap_or_default().to_owned(),
        headers,
        caller: Caller {
            principal: s["caller_key"].as_str().unwrap_or_default().to_owned(),
            source: "authorization",
        },
    }
}

fn error_json(e: &ExecError) -> Value {
    serde_json::json!({
        "status": e.status,
        "message": String::from_utf8_lossy(&e.body),
        "retry_after_ms": e.retry_after.map_or(-1, |d| d.as_millis() as i64),
        // The server's reading of IsCredentialScoped (classify::credential_scoped).
        "credential_scoped": e.scope == FailureScope::Credential && e.status == 429,
        // A plain Go error (no status) is a transport-scoped 500 here.
        "plain": e.scope == FailureScope::Transport,
    })
}

/// `scheme://authority` of every upstream URL goes to the capture server.
fn redirect(url: &str, addr: &str) -> String {
    let rest = &url[url.find("://").map_or(0, |i| i + 3)..];
    let path = rest.find('/').map_or("", |i| &rest[i..]);
    format!("http://{addr}{path}")
}

#[tokio::test]
async fn go_reference_scenarios() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let scenarios = fixture["scenarios"].as_array().unwrap();
    assert!(scenarios.len() >= 80, "fixture lost scenarios");
    // One executor for all scenarios: Go's replay cache is process-wide.
    let target: Arc<Mutex<String>> = Arc::default();
    let chosen: Arc<Mutex<Option<String>>> = Arc::default();
    let executor = {
        let (target, chosen) = (target.clone(), chosen.clone());
        XaiExecutor::default().with_url_rewrite(move |url| {
            *chosen.lock().unwrap() = Some(url.to_owned());
            redirect(url, &target.lock().unwrap())
        })
    };
    let mut skipped = Vec::new();
    let mut failures = Vec::new();
    for s in scenarios {
        let name = s["name"].as_str().unwrap();
        let needs: Vec<&str> = s["needs"]
            .as_array()
            .map(|n| n.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if needs.iter().any(|n| PENDING.contains(n)) {
            skipped.push(name);
            continue;
        }
        let mock = match s.get("upstream").filter(|u| !u.is_null()) {
            Some(up) => Some(Mock::start(up).await),
            None => None,
        };
        let addr = mock.as_ref().map_or("127.0.0.1:9".to_owned(), |m| m.addr.clone());
        *target.lock().unwrap() = addr.clone();
        *chosen.lock().unwrap() = None;
        let cfg = Config::parse(&s["config"].as_str().unwrap_or_default().replace("UPSTREAM", &addr)).unwrap();
        let cred = credential(s, &cfg);
        let (recorder, capture, sink) = crate::openai_compat_usage::sinks();
        let req = request(s, sink);
        let op = s["op"].as_str().unwrap();
        let path = s["request_path"].as_str().unwrap_or_default().to_owned();
        let result = match op {
            "images" => executor.images(&cred, req, &path, &cfg).await,
            "videos" => executor.videos(&cred, req, &path, &cfg).await,
            _ => {
                executor
                    .execute(&cred, req, &cfg, s["websocket"].as_bool() == Some(true))
                    .await
            }
        };
        let mut output = None;
        let mut chunks = Vec::new();
        let mut error = None;
        match result {
            Err(e) => error = Some(error_json(&e)),
            Ok(response) => match response.body {
                ResponseBody::Buffered(bytes) => output = Some(String::from_utf8_lossy(&bytes).into_owned()),
                ResponseBody::Stream(mut stream) => {
                    while let Some(item) = stream.next().await {
                        assert!(error.is_none(), "{name}: stream item after an error");
                        match item {
                            Ok(bytes) => chunks.push(String::from_utf8_lossy(&bytes).into_owned()),
                            Err(e) => error = Some(error_json(&e)),
                        }
                    }
                }
            },
        }
        let mut problems = Vec::new();
        let raw = mock.as_ref().and_then(|m| m.raw.lock().unwrap().clone());
        if let Some(mock) = &mock {
            let url = chosen.lock().unwrap().clone();
            if url.as_deref() != s["url"].as_str() {
                problems.push(format!("url: {url:?} != {:?}", s["url"]));
            }
            let got = mock.request();
            if got.as_deref() != s["request"].as_str() {
                problems.push(format!(
                    "request:\n  rust: {got:?}\n  go:   {:?}",
                    s["request"].as_str()
                ));
            }
        }
        if op != "count"
            && let Some(problem) = crate::openai_compat_usage::ttft_problem(
                &recorder.0.lock().unwrap(),
                raw.is_some(),
                raw.is_some() && !s["upstream"]["body"].as_str().unwrap_or_default().is_empty(),
            )
        {
            problems.push(problem);
        }
        // Capture: one logged attempt per request the upstream received, none otherwise.
        if let Some(raw) = raw {
            let up = &s["upstream"];
            let status = up["status"].as_u64().unwrap() as u16;
            let get = raw.starts_with(b"GET ");
            let attempt = HttpAttempt {
                raw: &raw,
                // The URL Go chose, as the rewrite sent it to the mock.
                url: &redirect(s["url"].as_str().unwrap_or_default(), &addr),
                provider: "xai",
                credential: &cred,
                // The video poll is a GET that logs its payload.
                logged_body: get.then(|| s["payload"].as_str().unwrap()),
                status,
                headers: up["headers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|h| (h[0].as_str().unwrap().to_owned(), h[1].as_str().unwrap().to_owned()))
                    .collect(),
                body: up["body"].as_str().unwrap(),
                logged: if op == "stream" && (200..300).contains(&status) {
                    Logged::Lines
                } else {
                    Logged::Whole
                },
            };
            for problem in http_capture_problems(&capture.take(), &attempt) {
                problems.push(problem);
            }
        } else {
            let events = capture.take();
            if !events.is_empty() {
                problems.push(format!("capture without an upstream attempt: {events:?}"));
            }
        }
        if output.as_deref() != s["output"].as_str() {
            problems.push(format!(
                "output:\n  rust: {output:?}\n  go:   {:?}",
                s["output"].as_str()
            ));
        }
        let go_chunks: Vec<&str> = s["chunks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let want: Vec<String> = if response_format(s) == Format::OpenAIResponse {
            let mut framer = cpa_translate::stream::ResponsesFramer::default();
            let mut frames: Vec<Bytes> = Vec::new();
            for chunk in &go_chunks {
                frames.extend(framer.write(chunk.as_bytes()));
            }
            frames.extend(framer.flush());
            frames.iter().map(|f| String::from_utf8_lossy(f).into_owned()).collect()
        } else {
            go_chunks
                .iter()
                .filter_map(|c| cpa_translate::stream::frame(response_format(s), c.as_bytes()))
                .map(|f| String::from_utf8_lossy(&f).into_owned())
                .collect()
        };
        if chunks != want {
            problems.push(format!("frames:\n  rust: {chunks:?}\n  go:   {want:?}"));
        }
        // Go publishes at most one record per attempt; the server publishes what the
        // executor's reports say (the rules of cpa-server's usage_record.rs, including
        // `discard` for an error Go raises before its reporter exists), compared with
        // Go's record or its absence. The server tracks no record for CountTokens.
        let want = s.get("usage").filter(|u| !u.is_null()).cloned();
        let reports = recorder.0.lock().unwrap();
        if op != "count" {
            let got = published(&reports, error.is_some());
            if got != want {
                problems.push(format!("usage: {got:?} != {want:?}"));
            }
        }
        drop(reports);
        let mut want_error = s["error"].clone();
        if !want_error.is_null() {
            let plain = want_error["status"] == 0;
            if plain {
                // A plain Go error: the handler answers 500.
                want_error["status"] = 500.into();
            }
            want_error["plain"] = plain.into();
        }
        let error = error.unwrap_or(Value::Null);
        if error != want_error {
            problems.push(format!("error: {error} != {want_error}"));
        }
        if !problems.is_empty() {
            failures.push(format!("{name}:\n{}", problems.join("\n")));
        }
    }
    assert!(
        failures.is_empty(),
        "{} scenarios differ:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
    assert_eq!(skipped, Vec::<&str>::new());
}

/// Go marks the first response byte only on a body read that returns data
/// (`usageTTFTReadCloser`): an empty error body starts the round trip and never marks it.
#[tokio::test]
async fn empty_error_body_starts_ttft_without_first_byte() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let s = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "api_key_default_base")
        .unwrap();
    let mock = Mock::start(&serde_json::json!({"status": 500, "headers": [], "body": ""})).await;
    let addr = mock.addr.clone();
    let executor = XaiExecutor::default().with_url_rewrite(move |url| redirect(url, &addr));
    let cfg = Config::default();
    let cred = credential(s, &cfg);
    let (recorder, _capture, sink) = crate::openai_compat_usage::sinks();
    let error = executor
        .execute(&cred, request(s, sink), &cfg, false)
        .await
        .err()
        .expect("the 500");
    assert_eq!(error.status, 500);
    let reports = recorder.0.lock().unwrap();
    assert_eq!(crate::openai_compat_usage::ttft_problem(&reports, true, false), None);
}
