//! Differential tests against `tests/fixtures/openai_compat_go.json`, produced by the real
//! Go executor (tests/reference/openai_compat). Each scenario replays the same scripted
//! upstream answer to the Rust executor and compares the raw upstream request and the
//! executor's output. Expected values come only from Go.

use std::sync::{Arc, Mutex};

use cpa_core::exec::Caller;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::*;
use crate::openai_compat_usage::Logged;

const FIXTURE: &str = include_str!("../tests/fixtures/openai_compat_go.json");

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
        let mut body = up["body"].as_str().unwrap().as_bytes().to_vec();
        if up["gzip"].as_bool() == Some(true) {
            use async_compression::tokio::bufread::GzipEncoder;
            let mut encoder = GzipEncoder::new(std::io::Cursor::new(body));
            let mut out = Vec::new();
            encoder.read_to_end(&mut out).await.unwrap();
            body = out;
        }
        let no_length = up["no_length"].as_bool() == Some(true);
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
            if !no_length {
                out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
            }
            out.extend_from_slice(b"Connection: close\r\n\r\n");
            out.extend_from_slice(&body);
            let _ = write.write_all(&out).await;
            let _ = write.shutdown().await;
        });
        Self { addr, raw }
    }

    fn request(&self) -> Option<Vec<u8>> {
        let raw = self.raw.lock().unwrap().clone()?;
        let mut out = replace(&raw, self.addr.as_bytes(), b"UPSTREAM");
        // Go's multipart writer picks a random 60-hex-digit boundary; so does Rust.
        let marker = b"boundary=";
        if let Some(i) = out.windows(marker.len()).position(|w| w == marker)
            && let Some(boundary) = out.get(i + marker.len()..i + marker.len() + 60)
            && boundary.iter().all(u8::is_ascii_hexdigit)
        {
            let boundary = boundary.to_vec();
            out = replace(&out, &boundary, b"BOUNDARY");
        }
        Some(out)
    }
}

fn replace(haystack: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

fn credential(s: &Value, cfg: &Config, addr: &str) -> Credential {
    let index = s["config_auth"].as_i64().unwrap();
    if index >= 0 {
        let compat: Vec<Credential> = cpa_core::config::credentials::from_config(cfg)
            .into_iter()
            .filter(|c| {
                c.attributes.get("compat_name").is_some_and(|n| !n.is_empty()) || c.provider == "openai-compatibility"
            })
            .collect();
        return compat[index as usize].clone();
    }
    let metadata = s["metadata"].as_object().cloned().unwrap_or_default();
    let mut c = Credential::from_file(
        std::path::Path::new("/fixture"),
        std::path::Path::new("/fixture/compat.json"),
        {
            let mut m = metadata;
            m.insert("type".into(), s["provider"].clone());
            m
        },
    )
    .unwrap();
    for (k, v) in s["attributes"].as_object().into_iter().flatten() {
        c.attributes
            .insert(k.clone(), v.as_str().unwrap().replace("UPSTREAM", addr));
    }
    c
}

fn response_format(s: &Value) -> Format {
    let source = Format::parse(s["source"].as_str().unwrap()).unwrap();
    s["response"].as_str().and_then(Format::parse).unwrap_or(source)
}

fn request(s: &Value, usage: cpa_core::exec::UsageSink) -> (ExecRequest, String) {
    let op = s["op"].as_str().unwrap();
    let source = Format::parse(s["source"].as_str().unwrap()).unwrap();
    let response = s["response"].as_str().and_then(Format::parse).unwrap_or(source);
    let model = s["model"].as_str().unwrap().to_owned();
    let payload = match s["payload_b64"].as_str() {
        Some(b64) => {
            Bytes::from(base64::engine::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap())
        }
        None => Bytes::from(s["payload"].as_str().unwrap().to_owned()),
    };
    // Go sets opts.OriginalRequest only when the scenario has one.
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
    if let Some(ct) = s["content_type"].as_str() {
        headers.insert(http::header::CONTENT_TYPE, ct.parse().unwrap());
    }
    // Go's executor-side fallback (EnsureSessionContext -> CanonicalSessionID) for a
    // request without conductor metadata, which is how the fixtures were generated.
    let session_body: &[u8] = if original.is_empty() { &payload } else { &original };
    let meta = cpa_common::session::Meta {
        execution_session: s["execution_session"].as_str(),
        derived: s["derived_session"].as_str(),
    };
    let session = Some(cpa_common::session::extract_session_id(&headers, session_body, &meta))
        .filter(|id| !id.is_empty())
        .map(|id| cpa_common::session::bound_session_identity(&id));
    let req = ExecRequest {
        operation: if op == "count" {
            Operation::CountTokens
        } else {
            Operation::Generate
        },
        source_format: source,
        response_format: response,
        requested_model: s["requested_model"].as_str().unwrap_or(&model).to_owned(),
        model,
        original_body: original,
        body: payload,
        stream: op.ends_with("stream") || s["stream"].as_bool() == Some(true),
        alt: s["alt"].as_str().map(str::to_owned),
        session,
        execution_session: s["execution_session"].as_str().map(str::to_owned),
        derived_session: s["derived_session"].as_str().map(str::to_owned),
        resolved_model: bound_model(&s["resolved_model"]),
        usage,
        request_path: String::new(),
        headers,
        caller: Caller {
            principal: "fake-client-key".into(),
            source: "authorization",
        },
    };
    (req, op.to_owned())
}

/// The dispatch loop's binding (cpa-server capabilities::resolve), as Go's conductor
/// bound it in the generator.
pub(crate) fn bound_model(v: &Value) -> Option<cpa_core::exec::ResolvedModel> {
    let raw = v.as_object()?.clone();
    Some(cpa_core::exec::ResolvedModel {
        info: cpa_core::registry::ModelInfo::from_raw(raw).expect("resolved model info"),
        source: cpa_core::exec::ResolvedSource::ApiKey,
    })
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

fn want_bytes(s: &Value) -> Option<Vec<u8>> {
    match s["request_b64"].as_str() {
        Some(b64) => Some(base64::engine::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap()),
        None => s["request"].as_str().map(|r| r.as_bytes().to_vec()),
    }
}

#[tokio::test]
async fn go_reference_scenarios() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let scenarios = fixture["scenarios"].as_array().unwrap();
    assert!(scenarios.len() >= 60, "fixture lost scenarios");
    let executor = OpenAICompatExecutor::default();
    let mut skipped = Vec::new();
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
        let cfg = Config::parse(&s["config"].as_str().unwrap_or_default().replace("UPSTREAM", &addr)).unwrap();
        let cred = credential(s, &cfg, &addr);
        let (recorder, capture, sink) = crate::openai_compat_usage::sinks();
        let (req, op) = request(s, sink);
        let result = if op.starts_with("images") {
            let path = s["request_path"].as_str().unwrap_or_default().to_owned();
            executor.images(&cred, req, &path, &cfg).await
        } else {
            executor.execute(&cred, req, &cfg).await
        };
        let mut output = None;
        let mut chunks = Vec::new();
        let mut error = None;
        match result {
            Err(e) => error = Some(error_json(&e)),
            Ok(response) => match response.body {
                ResponseBody::Buffered(bytes) => output = Some(String::from_utf8_lossy(&bytes).into_owned()),
                ResponseBody::Stream(mut stream) => {
                    let mut joined = Vec::new();
                    while let Some(item) = stream.next().await {
                        // An error is terminal: nothing may follow it.
                        assert!(error.is_none(), "{name}: stream item after an error");
                        match item {
                            Ok(bytes) => {
                                joined.extend_from_slice(&bytes);
                                chunks.push(String::from_utf8_lossy(&bytes).into_owned());
                            }
                            Err(e) => error = Some(error_json(&e)),
                        }
                    }
                    if op == "images_stream" {
                        output = Some(String::from_utf8_lossy(&joined).into_owned());
                        chunks.clear();
                    }
                }
            },
        }
        if let Some(mock) = &mock {
            let want = match s["request_b64"].as_str() {
                Some(b64) => {
                    Some(base64::engine::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap())
                }
                None => s["request"].as_str().map(|r| r.as_bytes().to_vec()),
            };
            assert_eq!(
                mock.request().map(|r| String::from_utf8_lossy(&r).into_owned()),
                want.map(|r| String::from_utf8_lossy(&r).into_owned()),
                "{name}: upstream request"
            );
            assert_eq!(mock.request(), want_bytes(s), "{name}: upstream request bytes");
            let raw = mock.raw.lock().unwrap().clone().unwrap();
            let target = String::from_utf8_lossy(&raw)
                .split(' ')
                .nth(1)
                .unwrap_or_default()
                .to_owned();
            let up = &s["upstream"];
            let status = up["status"].as_u64().unwrap() as u16;
            let ok = (200..300).contains(&status);
            let attempt = crate::openai_compat_usage::HttpAttempt {
                raw: &raw,
                url: &format!("http://{}{target}", mock.addr),
                provider: s["provider"].as_str().unwrap(),
                credential: &cred,
                logged_body: None,
                status,
                // Go's transport drops Content-Encoding from a body it decompressed.
                headers: up["headers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|h| (h[0].as_str().unwrap().to_owned(), h[1].as_str().unwrap().to_owned()))
                    .filter(|(n, _)| !(up["gzip"] == true && n.eq_ignore_ascii_case("content-encoding")))
                    .collect(),
                body: up["body"].as_str().unwrap(),
                logged: match op.as_str() {
                    "stream" if ok => Logged::Lines,
                    "images_stream" if ok => Logged::Reads,
                    _ => Logged::Whole,
                },
            };
            let events = capture.take();
            let problems = crate::openai_compat_usage::http_capture_problems(&events, &attempt);
            assert!(problems.is_empty(), "{name}: {problems:#?}");
            if op == "stream"
                && ok
                && let Some(e) = &error
            {
                stream_failure_logged(name, e, &events, &recorder.0.lock().unwrap());
            }
        } else {
            assert_eq!(
                capture.take(),
                Vec::new(),
                "{name}: capture without an upstream attempt"
            );
        }
        assert_eq!(output.as_deref(), s["output"].as_str(), "{name}: output");
        // Go's chunks are what the client's route frames; the Rust translator contract
        // emits the framed events, one per item. Responses routes join chunks into frames
        // (responsesSSEFramer), so those compare frame by frame. The route's frame repairs
        // (repairErrorPayload) are server logic and not modelled here.
        let go_chunks: Vec<&str> = s["chunks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        if response_format(s) == Format::OpenAIResponse {
            let mut framer = cpa_translate::stream::ResponsesFramer::default();
            let mut want: Vec<bytes::Bytes> = Vec::new();
            for chunk in &go_chunks {
                want.extend(framer.write(chunk.as_bytes()));
            }
            want.extend(framer.flush());
            let want: Vec<String> = want.iter().map(|f| String::from_utf8_lossy(f).into_owned()).collect();
            assert_eq!(chunks, want, "{name}: stream frames");
        } else {
            let want_chunks: Vec<String> = go_chunks
                .iter()
                .filter_map(|c| cpa_translate::stream::frame(response_format(s), c.as_bytes()))
                .map(|f| String::from_utf8_lossy(&f).into_owned())
                .collect();
            assert_eq!(chunks, want_chunks, "{name}: stream chunks");
        }
        // Go's published usage record, derived from what the executor reported.
        if let Some(want) = s.get("usage").filter(|u| !u.is_null()) {
            let got = crate::openai_compat_usage::derived(&recorder.0.lock().unwrap(), want["failed"] == true);
            assert_eq!(&got, want, "{name}: usage");
        }
        let mut want_error = s["error"].clone();
        if !want_error.is_null() {
            let plain = want_error["status"] == 0;
            if plain {
                // A plain Go error: the handler answers 500.
                want_error["status"] = 500.into();
            }
            want_error["plain"] = plain.into();
        }
        assert_eq!(error.unwrap_or(Value::Null), want_error, "{name}: error");
    }
    assert_eq!(skipped, Vec::<&str>::new());
}

/// Go's images paths read the whole response before checking the status
/// (openai_compat_executor.go executeImages and executeImagesStream): a non-2xx body that
/// ends early is logged (`RecordAPIResponseError`) and its read error returned, where the
/// chat paths ignore it.
#[tokio::test]
async fn images_error_body_read_failure_is_logged_and_returned() {
    use crate::openai_compat_usage::Captured;
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let executor = OpenAICompatExecutor::default();
    for name in ["images_error_status", "images_stream_error_status"] {
        let s = fixture["scenarios"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == name)
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let (read, mut write) = socket.into_split();
            let mut reader = BufReader::new(read);
            let mut length = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body).await;
            // 100 bytes declared, 8 sent, then the connection ends.
            let _ = write
                .write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 100\r\n\r\n{\"error\"")
                .await;
            let _ = write.shutdown().await;
        });
        let cfg = Config::parse(&s["config"].as_str().unwrap().replace("UPSTREAM", &addr)).unwrap();
        let cred = credential(s, &cfg, &addr);
        let (_recorder, capture, sink) = crate::openai_compat_usage::sinks();
        let (req, _) = request(s, sink);
        let path = s["request_path"].as_str().unwrap().to_owned();
        let error = executor.images(&cred, req, &path, &cfg).await.err().expect(name);
        assert_eq!(
            error.scope,
            FailureScope::Transport,
            "{name}: the read error, not the 429"
        );
        let events = capture.take();
        assert!(
            matches!(
                events.as_slice(),
                [Captured::Request { .. }, Captured::Metadata(429, _), Captured::Error(_)]
            ),
            "{name}: {events:?}"
        );
    }
}

/// Scenarios whose stream fails on an upstream error payload: Go logs and publishes a
/// fixed text instead (`publishStreamError(err, true)`).
const PAYLOAD_ERRORS: &[&str] = &[
    "stream_named_error_event",
    "stream_data_error_with_status",
    "stream_data_type_failed",
    "stream_data_top_level_code_message",
    "stream_plain_json_after_blank_lines",
    "stream_data_error_fractional_status_string",
    "stream_data_error_overflowing_status_string",
];

/// Scenarios whose apply_patch input fails only at EOF: `EndApplyPatchStream` publishes
/// the failure without logging it.
const EOF_TOOL_FAILURES: &[&str] = &["apply_patch_stream_truncated"];

/// Go's stream failure: `RecordAPIResponseError` and `PublishFailure` with the logged
/// error, before the client gets the real one.
fn stream_failure_logged(
    name: &str,
    error: &Value,
    events: &[crate::openai_compat_usage::Captured],
    reports: &[crate::openai_compat_usage::Report],
) {
    use crate::openai_compat_usage::{Captured, Report};
    let message = error["message"].as_str().unwrap();
    let logged = if PAYLOAD_ERRORS.contains(&name) {
        "upstream stream returned an error payload"
    } else {
        message
    };
    let tail = if EOF_TOOL_FAILURES.contains(&name) {
        None
    } else {
        Some(Captured::Error(logged.to_owned()))
    };
    assert_eq!(
        events.last().filter(|e| matches!(e, Captured::Error(_))),
        tail.as_ref(),
        "{name}: logged"
    );
    let published = reports.iter().find_map(|r| match r {
        Report::PublishFailure(status, body) => Some((*status, body.as_str())),
        _ => None,
    });
    let status = error["status"].as_u64().unwrap() as u16;
    assert_eq!(published, Some((status, logged)), "{name}: published failure");
}

#[test]
fn provider_keys() {
    assert!(handles("openai-compatibility"));
    assert!(handles("openai-compatible-acme"));
    assert!(!handles("openai"));
    assert!(!handles("codex"));
}

fn b64(s: &Value) -> Vec<u8> {
    base64::engine::Engine::decode(&base64::engine::general_purpose::STANDARD, s.as_str().unwrap()).unwrap()
}

/// Go standard-library answers recorded by the generator (`vectors`).
#[test]
fn go_primitive_vectors() {
    use crate::openai_compat_go as go;
    use crate::openai_compat_multipart::{MediaType, parse_media_type};
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let v = &fixture["vectors"];
    for case in v["http_time"].as_array().unwrap() {
        let raw = case[0].as_str().unwrap();
        let got = go::parse_http_time(raw).map(|t| match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i64,
            Err(e) => -(e.duration().as_nanos() as i64),
        });
        assert_eq!(got, case[1].as_i64(), "http.ParseTime({raw:?})");
    }
    for case in v["json_valid"].as_array().unwrap() {
        let input = b64(&case[0]);
        assert_eq!(
            go::json_valid(&input),
            case[1].as_bool().unwrap(),
            "json.Valid({:?})",
            String::from_utf8_lossy(&input[..input.len().min(40)])
        );
    }
    for case in v["gjson_int"].as_array().unwrap() {
        let raw = case[0].as_str().unwrap();
        assert_eq!(
            Some(gj::parse(raw.as_bytes()).int()),
            case[1].as_i64(),
            "gjson Int({raw})"
        );
    }
    for case in v["trim_space"].as_array().unwrap() {
        assert_eq!(go::trim_space(&b64(&case[0])), b64(&case[1]).as_slice());
    }
    for case in v["media_type"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let (media, error) = (case["media"].as_str().unwrap(), case["error"].as_str().unwrap());
        let want = if error.is_empty() {
            let params = case["params"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                .collect();
            MediaType::Ok(media.to_owned(), params)
        } else if media.is_empty() {
            MediaType::Invalid
        } else {
            MediaType::BadParams(media.to_owned())
        };
        assert_eq!(parse_media_type(input), want, "mime.ParseMediaType({input:?})");
    }
    for case in v["token_counts"].as_array().unwrap() {
        let (model, payload) = (case[0].as_str().unwrap(), case[1].as_str().unwrap());
        assert_eq!(
            crate::openai_compat_payload::count_chat_tokens(model, payload.as_bytes()),
            Ok(case[2].as_i64().unwrap()),
            "CountOpenAIChatTokens({model:?}, {payload})"
        );
    }
}

/// A translator holding one unfinished Responses frame.
struct Pending;

impl StreamTranslator for Pending {
    fn event(&mut self, _: &[u8]) -> Result<Vec<Bytes>, cpa_translate::Error> {
        Ok(vec![])
    }

    fn finish(&mut self) -> Result<Vec<Bytes>, cpa_translate::Error> {
        Ok(vec![])
    }

    fn flush_frames(&mut self) -> Vec<Bytes> {
        vec![Bytes::from_static(b"event: pending\n\n")]
    }
}

#[tokio::test]
async fn terminal_errors_flush_pending_frames_first() {
    // Go's responsesSSEFramer flushes before the handler writes a terminal error.
    for (lines, status) in [
        (
            vec![
                Ok(Bytes::from_static(b"data: {\"error\":{\"status\":429}}")),
                Ok(Bytes::new()),
            ],
            429,
        ),
        (vec![Err(ExecError::local(502, FailureScope::Transport, "cut"))], 502),
    ] {
        let lines: ExecStream = futures_util::stream::iter(lines).boxed();
        let out: Vec<_> = frames(lines, Box::new(Pending), true, Default::default(), Default::default())
            .collect()
            .await;
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].as_ref().unwrap(), &Bytes::from_static(b"event: pending\n\n"));
        assert_eq!(out[1].as_ref().unwrap_err().status, status);
    }
}
