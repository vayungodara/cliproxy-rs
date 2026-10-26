//! Differential tests against `tests/fixtures/gemini_go.json`, produced by the real Go
//! Gemini and Interactions executors (tests/reference/gemini). Each scenario replays the
//! scripted upstream answer to the Rust executor and compares the raw upstream request,
//! the output, the client stream bytes and errors. Expected values come only from Go.

use std::sync::{Arc, Mutex};

use cpa_core::exec::Caller;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::*;
use crate::{gemini_payload as payload, gemini_stream as sse};

/// The model info Go's conductor bound for the scenario (the fixture's `resolved`).
pub(crate) fn resolved_model(s: &Value) -> Option<cpa_core::exec::ResolvedModel> {
    let record = s.get("resolved").filter(|r| !r.is_null())?;
    let mut raw = record["info"].as_object().unwrap().clone();
    for flag in ["is_compat", "user_defined", "support_configuration_update"] {
        if record[flag].as_bool() == Some(true) {
            raw.insert(flag.into(), Value::Bool(true));
        }
    }
    Some(cpa_core::exec::ResolvedModel {
        info: cpa_core::registry::ModelInfo::from_raw(raw).unwrap(),
        source: cpa_core::exec::ResolvedSource::ApiKey,
    })
}

/// What an executor reported to its usage sink: the reasoning effort of the last
/// translated request (Go `SetTranslatedReasoningEffort`) and how many response reports
/// arrived. Token and response-model parity is checked through the router against Go's
/// published records (cpa-server tests/gemini_routes.rs `usage_records_match_go`).
#[derive(Default)]
pub(crate) struct UsageReports {
    effort: Mutex<Option<String>>,
    responses: Mutex<usize>,
    /// A failure the executor published itself: status, body and whether it carries
    /// the usage seen so far.
    failure: Mutex<Option<(u16, String, bool)>>,
    /// The executor type and model of an executor the attempt was handed to.
    pub(crate) delegated: Mutex<Option<(String, String)>>,
}

impl UsageReports {
    pub(crate) fn sink(self: &Arc<Self>) -> cpa_core::exec::UsageSink {
        cpa_core::exec::UsageSink::new(self.clone())
    }

    /// Checks the reports against the record Go's reporter published (the fixture's
    /// `usage`): Go's record keeps an empty effort unless the executor set one.
    pub(crate) fn check(&self, s: &Value) {
        let name = s["name"].as_str().unwrap();
        let effort = self.effort.lock().unwrap().clone().unwrap_or_default();
        match s.get("usage").filter(|u| !u.is_null()) {
            Some(want) => assert_eq!(
                effort,
                want["reasoning_effort"].as_str().unwrap(),
                "{name}: reasoning effort"
            ),
            // No record: Go failed before its reporter existed, so nothing was sent.
            None => assert_eq!(*self.responses.lock().unwrap(), 0, "{name}: no reporter in Go"),
        }
        // A client that went away mid-stream: Go's record failed with the executor's
        // error, or succeeded, which Rust leaves to the server's end-of-attempt record.
        // Every such upstream reports usage before the drop, so Go's failure carries
        // tokens exactly when it keeps the usage seen so far.
        if s["cancel_when_idle"] == true {
            let usage = &s["usage"];
            let want = (usage["failed"] == true).then(|| {
                (
                    usage["fail_status"].as_u64().unwrap() as u16,
                    usage["fail_body"].as_str().unwrap().to_owned(),
                    usage["total_tokens"].as_u64().unwrap() > 0,
                )
            });
            assert_eq!(*self.failure.lock().unwrap(), want, "{name}: executor failure on drop");
        }
    }
}

impl cpa_core::exec::UsageObserver for UsageReports {
    fn publish_failure(&self, status: u16, body: &str) {
        self.failure
            .lock()
            .unwrap()
            .get_or_insert((status, body.to_owned(), false));
    }

    fn publish_failure_with_usage(&self, status: u16, body: &str) {
        self.failure
            .lock()
            .unwrap()
            .get_or_insert((status, body.to_owned(), true));
    }

    fn response_body(&self, _: Format, _: &[u8]) {
        *self.responses.lock().unwrap() += 1;
    }

    fn response_line(&self, _: Format, _: &[u8]) {
        *self.responses.lock().unwrap() += 1;
    }

    fn request(&self, format: Format, payload: &[u8]) {
        let effort = cpa_common::thinking::extract_translated_reasoning_effort(payload, format.as_str());
        *self.effort.lock().unwrap() = Some(effort);
    }

    fn delegated_executor(&self, executor_type: &str, model: &str) {
        *self.delegated.lock().unwrap() = Some((executor_type.to_owned(), model.to_owned()));
    }
}

/// Upstream capture events (cpa_core::exec::CaptureEvent) as an executor recorded them.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Captured {
    Request {
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        provider: String,
        auth_id: String,
        auth_type: String,
        auth_value: String,
    },
    Metadata(u16),
    Chunk(Vec<u8>),
    Error(String),
}

#[derive(Default)]
pub(crate) struct Captures(Mutex<Vec<Captured>>);

impl Captures {
    pub(crate) fn sink(self: &Arc<Self>) -> cpa_core::exec::CaptureSink {
        cpa_core::exec::CaptureSink::new(self.clone())
    }

    pub(crate) fn events(&self) -> Vec<Captured> {
        self.0.lock().unwrap().clone()
    }
}

impl cpa_core::exec::CaptureObserver for Captures {
    fn record(&self, event: cpa_core::exec::CaptureEvent<'_>) {
        use cpa_core::exec::CaptureEvent as E;
        let captured = match event {
            E::Request(r) => {
                assert_eq!(r.method, "POST");
                Captured::Request {
                    url: r.url.into(),
                    headers: r.headers.to_vec(),
                    body: r.body.to_vec(),
                    provider: r.provider.into(),
                    auth_id: r.auth_id.into(),
                    auth_type: r.auth_type.into(),
                    auth_value: r.auth_value.into(),
                }
            }
            E::ResponseMetadata(status, _) => Captured::Metadata(status),
            E::ResponseChunk(chunk) => Captured::Chunk(chunk.to_vec()),
            E::ResponseError(error) => Captured::Error(error.into()),
            other => panic!(
                "unexpected websocket capture {}",
                matches!(other, E::WebsocketRequest(_))
            ),
        };
        self.0.lock().unwrap().push(captured);
    }
}

/// bufio.ScanLines.
fn scan_lines(body: &[u8]) -> Vec<Vec<u8>> {
    let body = body.strip_suffix(b"\n").unwrap_or(body);
    if body.is_empty() {
        return Vec::new();
    }
    body.split(|b| *b == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l).to_vec())
        .collect()
}

/// Checks the capture of one physical upstream attempt against the raw request Go's
/// executor sent (`wire`, from the fixture) and the reply it got. Go logs
/// `httpReq.Header` before the transport adds `User-Agent`, `Accept-Encoding`,
/// `Content-Length` and `Host`; so the captured headers must be the wire headers less
/// exactly those additions. Then the reply's status, and its body: whole for buffered
/// reads, one chunk per scanner line for streams (the fixtures read every stream to
/// its end).
pub(crate) fn check_attempt(
    name: &str,
    events: &mut std::collections::VecDeque<Captured>,
    wire: &str,
    reply: Option<&Value>,
    lines: bool,
    provider: &str,
    account: Option<(&str, &str)>,
) {
    let Some(Captured::Request {
        url,
        headers,
        body,
        provider: got_provider,
        auth_type,
        auth_value,
        ..
    }) = events.pop_front()
    else {
        panic!("{name}: no request capture");
    };
    let (head, wire_body) = wire.split_once("\r\n\r\n").or_else(|| wire.split_once("\n\n")).unwrap();
    let mut head_lines = head.lines();
    let request_line = head_lines.next().unwrap();
    let target = request_line.split(' ').nth(1).unwrap();
    assert!(
        url.ends_with(target.trim_start_matches("UPSTREAM")),
        "{name}: url {url} for {target}"
    );
    assert_eq!(String::from_utf8_lossy(&body), wire_body, "{name}: request body");
    assert_eq!(got_provider, provider, "{name}: provider");
    let wire_headers: Vec<(String, String)> = head_lines
        .filter_map(|l| l.split_once(": "))
        .map(|(n, v)| (n.to_ascii_lowercase(), v.to_owned()))
        .collect();
    for (n, v) in &headers {
        if n.eq_ignore_ascii_case("host") {
            continue;
        }
        assert!(
            wire_headers.contains(&(n.to_ascii_lowercase(), v.clone())),
            "{name}: captured {n}: {v} was not sent"
        );
    }
    for (n, v) in &wire_headers {
        let captured = headers.iter().any(|(cn, cv)| cn.eq_ignore_ascii_case(n) && cv == v);
        assert!(
            captured || matches!(n.as_str(), "host" | "user-agent" | "accept-encoding" | "content-length"),
            "{name}: sent {n}: {v} without capture"
        );
    }
    if let Some(account) = account {
        assert_eq!((auth_type.as_str(), auth_value.as_str()), account, "{name}: account");
    }
    let Some(reply) = reply else {
        return;
    };
    let status = reply["status"].as_u64().unwrap() as u16;
    match events.pop_front() {
        Some(Captured::Metadata(got)) => assert_eq!(got, status, "{name}: response status"),
        other => panic!("{name}: expected response metadata, got {other:?}"),
    }
    let reply_body = reply["body"].as_str().unwrap_or_default().as_bytes();
    let mut chunks = Vec::new();
    while let Some(Captured::Chunk(_)) = events.front() {
        let Some(Captured::Chunk(chunk)) = events.pop_front() else {
            unreachable!()
        };
        chunks.push(chunk);
    }
    if lines && (200..300).contains(&status) {
        let text = |v: &[Vec<u8>]| {
            v.iter()
                .map(|c| String::from_utf8_lossy(c).into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(text(&chunks), text(&scan_lines(reply_body)), "{name}: stream chunks");
    } else {
        assert_eq!(chunks, vec![reply_body.to_vec()], "{name}: response body");
    }
    // A client gone mid-read: Go records the canceled read when it fails the attempt
    // with it (UsageReports::check compares that failure).
    if reply["stall"] == true {
        if let Some(Captured::Error(text)) = events.front() {
            assert_eq!(text, "context canceled", "{name}: response error");
            events.pop_front();
        }
    // A reply cut short (`missing` bytes): Go's RecordAPIResponseError of the scanner.
    } else if reply["missing"].as_u64().unwrap_or(0) > 0 {
        match events.pop_front() {
            Some(Captured::Error(text)) => assert_eq!(text, "unexpected EOF", "{name}: response error"),
            other => panic!("{name}: expected the read error, got {other:?}"),
        }
    }
}

const FIXTURE: &str = include_str!("../tests/fixtures/gemini_go.json");

/// `gemini_go.json` with the scenarios the payload barrier changed taken from their
/// v8.0.35 recording.
fn fixture() -> &'static Value {
    static PARSED: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    PARSED.get_or_init(|| {
        crate::payload_barrier::with_v8_0_35(
            FIXTURE,
            include_str!("../tests/fixtures/gemini_go_v8.0.35.json"),
            "scenarios",
        )
    })
}

/// One-shot raw HTTP/1.1 capture server answering with the scripted response.
pub(crate) struct Mock {
    addr: String,
    raw: Arc<Mutex<Option<Vec<u8>>>>,
}

impl Mock {
    pub(crate) async fn start(up: &Value) -> Self {
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
        let up_missing = up["missing"].as_u64().unwrap_or(0) as usize;
        let stall = up["stall"] == true;
        let mut body = up["body"].as_str().unwrap().as_bytes().to_vec();
        if up["gzip"].as_bool() == Some(true) {
            use async_compression::tokio::bufread::GzipEncoder;
            let mut encoder = GzipEncoder::new(std::io::Cursor::new(body));
            let mut out = Vec::new();
            encoder.read_to_end(&mut out).await.unwrap();
            body = out;
        }
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
            // `missing`: bytes promised beyond the body, a connection closed mid-stream.
            let missing = up_missing;
            out.extend_from_slice(format!("Content-Length: {}\r\n", body.len() + missing).as_bytes());
            out.extend_from_slice(b"Connection: close\r\n\r\n");
            out.extend_from_slice(&body);
            let _ = write.write_all(&out).await;
            if stall {
                // `stall`: the connection stays open until the client closes it.
                let mut rest = Vec::new();
                let wait = reader.read_to_end(&mut rest);
                let _ = tokio::time::timeout(std::time::Duration::from_secs(5), wait).await;
            }
            let _ = write.shutdown().await;
        });
        Self { addr, raw }
    }

    pub(crate) fn addr(&self) -> &str {
        &self.addr
    }

    pub(crate) fn request(&self) -> Option<String> {
        let raw = self.raw.lock().unwrap().clone()?;
        Some(String::from_utf8(raw).unwrap().replace(&self.addr, "UPSTREAM"))
    }
}

fn credential(s: &Value, cfg: &Config, addr: &str) -> Credential {
    let provider = s["provider"].as_str().unwrap();
    let index = s["config_auth"].as_i64().unwrap();
    if index >= 0 {
        let matching: Vec<Credential> = cpa_core::config::credentials::from_config(cfg)
            .into_iter()
            .filter(|c| c.provider == provider)
            .collect();
        return matching[index as usize].clone();
    }
    let mut metadata = serde_json::Map::new();
    metadata.insert("type".into(), provider.into());
    let mut c = Credential::from_file(
        std::path::Path::new("/fixture"),
        std::path::Path::new("/fixture/gemini.json"),
        metadata,
    )
    .unwrap();
    for (k, v) in s["attributes"].as_object().into_iter().flatten() {
        c.attributes
            .insert(k.clone(), v.as_str().unwrap().replace("UPSTREAM", addr));
    }
    c
}

fn request(s: &Value) -> ExecRequest {
    let op = s["op"].as_str().unwrap();
    let source = Format::parse(s["source"].as_str().unwrap()).unwrap();
    let response = s["response"].as_str().and_then(Format::parse).unwrap_or(source);
    let model = s["model"].as_str().unwrap().to_owned();
    let body = Bytes::from(s["payload"].as_str().unwrap().to_owned());
    let original = s["original"]
        .as_str()
        .map(|o| Bytes::from(o.to_owned()))
        .unwrap_or_else(|| body.clone());
    let mut headers = http::HeaderMap::new();
    for (k, v) in s["headers"].as_object().into_iter().flatten() {
        headers.insert(
            http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.as_str().unwrap().parse().unwrap(),
        );
    }
    ExecRequest {
        operation: if op == "count" {
            Operation::CountTokens
        } else {
            Operation::Generate
        },
        source_format: source,
        response_format: response,
        stream_payloads: false,
        requested_model: s["requested_model"].as_str().unwrap_or(&model).to_owned(),
        model,
        original_body: original,
        body,
        stream: op == "stream",
        alt: s["alt"].as_str().map(str::to_owned),
        session: s["session"].as_str().map(str::to_owned),
        execution_session: None,
        derived_session: None,
        resolved_model: resolved_model(s),
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

/// The bytes Go's route handler writes for the executor's stream chunks, written out
/// from the handlers rather than taken from the Rust framer. Empty chunks never reach a
/// handler (handlers_stream.go). The Responses route's terminal tracking and the Gemini
/// route's keep-alives belong to the server, so this stops at each chunk's framing.
pub(crate) fn client_bytes(client: Format, alt: bool, chunks: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in chunks {
        let chunk = chunk.as_str().unwrap().as_bytes();
        if chunk.is_empty() {
            continue;
        }
        match client {
            // gemini_handlers.go: `data: ` + chunk + `\n\n`, or the chunk with an alt.
            Format::Gemini if alt => out.extend_from_slice(chunk),
            // openai_handlers.go: fmt.Fprintf("data: %s\n\n").
            Format::Gemini | Format::OpenAI => {
                out.extend_from_slice(b"data: ");
                out.extend_from_slice(chunk);
                out.extend_from_slice(b"\n\n");
            }
            // interactions_handlers.go WriteChunk.
            Format::Interactions => {
                let trimmed = cpa_common::gostr::trim_space(chunk);
                if !(trimmed.starts_with(b"event:") || trimmed.starts_with(b"data:")) {
                    out.extend_from_slice(b"data: ");
                }
                out.extend_from_slice(chunk);
                if !chunk.ends_with(b"\n\n") {
                    out.extend_from_slice(b"\n\n");
                }
            }
            // responsesSSEFramer for complete events: the event and its blank line.
            Format::OpenAIResponse => {
                out.extend_from_slice(chunk);
                if !chunk.ends_with(b"\n\n") {
                    out.extend_from_slice(b"\n\n");
                }
            }
            // code_handlers.go: the chunk as is.
            _ => out.extend_from_slice(chunk),
        }
    }
    out
}

/// Translator registrations a scenario needs that Rust does not have yet.
fn missing(s: &Value) -> Vec<String> {
    let parse = |entry: &str| {
        let (client, upstream) = entry.split_once("->").unwrap();
        (Format::parse(client).unwrap(), Format::parse(upstream).unwrap())
    };
    s["needs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|need| match need.split_once(':') {
            Some(("pair", pair)) => {
                let (c, u) = parse(pair);
                cpa_translate::pair(c, u).is_none()
            }
            Some(("token_count", pair)) => {
                let (c, u) = parse(pair);
                cpa_translate::token_count(c, u).is_none()
            }
            _ => panic!("unknown need {need}"),
        })
        .map(str::to_owned)
        .collect()
}

/// Go v8.0.35 (f62d4d28) dropped `gemini-3.7-flash`: with no registry entry
/// `capGeminiMaxOutputTokens` leaves the request's limit alone, while the
/// `gemini-3.8-flash` the historical golden now uses is capped at its completion limit.
#[test]
fn a_removed_model_has_no_output_cap() {
    let body = br#"{"generationConfig":{"maxOutputTokens":70000}}"#.to_vec();
    assert_eq!(payload::cap_max_output_tokens(body.clone(), "gemini-3.7-flash"), body);
    assert_eq!(
        payload::cap_max_output_tokens(body, "gemini-3.8-flash"),
        br#"{"generationConfig":{"maxOutputTokens":65536}}"#
    );
}

#[tokio::test]
async fn go_reference_scenarios() {
    let scenarios = fixture()["scenarios"].as_array().unwrap();
    assert!(scenarios.len() >= 60, "fixture lost scenarios");
    let executor = GeminiExecutor::default();
    let mut ran = 0;
    let mut skipped = Vec::new();
    let ids = regex::Regex::new(r"interaction_[0-9]{16,20}").unwrap();
    let stamps = regex::Regex::new(r#"\"(created|updated)\":\"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]{8}Z\""#).unwrap();
    // Interactions -> OpenAI chat stamps `created` with time.Now().Unix() (Go
    // openai_interactions_response.go); only 10-digit Unix times count as clock values.
    let unix = regex::Regex::new(r#"\"created\":1[0-9]{9}([,}])"#).unwrap();
    for s in scenarios {
        let name = s["name"].as_str().unwrap();
        // Recorded with 6fecc6e's catalog, where gemini-3.7-flash had a completion limit
        // and no output limit. Go v8.0.35 (f62d4d28) drops that model; gemini-3.8-flash
        // has the same limits and an ID of the same length, so the recorded bytes hold
        // with the name swapped.
        let swapped: Value;
        let s = if name == "gen_cap_uses_max_completion_tokens" {
            swapped = serde_json::from_str(&s.to_string().replace("gemini-3.7-flash", "gemini-3.8-flash")).unwrap();
            &swapped
        } else {
            s
        };
        let missing = missing(s);
        if !missing.is_empty() {
            skipped.push(format!("{name} ({})", missing.join(", ")));
            continue;
        }
        let mock = match s.get("upstream").filter(|u| !u.is_null()) {
            Some(up) => Some(Mock::start(up).await),
            None => None,
        };
        let addr = mock.as_ref().map_or("127.0.0.1:9".to_owned(), |m| m.addr.clone());
        let cfg = Config::parse(&s["config"].as_str().unwrap_or_default().replace("UPSTREAM", &addr)).unwrap();
        let cred = credential(s, &cfg, &addr);
        let mut req = request(s);
        let reports = Arc::new(UsageReports::default());
        let captures = Arc::new(Captures::default());
        req.usage = reports.sink().with_capture(captures.sink());
        let (client, alt) = (req.response_format, req.alt.as_deref().is_some_and(|a| !a.is_empty()));
        let mut output = None;
        let mut streamed = Vec::new();
        let mut error = None;
        match executor.execute(&cred, req, &cfg).await {
            Err(e) => error = Some((e.status, String::from_utf8_lossy(&e.body).into_owned())),
            Ok(response) => match response.body {
                ResponseBody::Buffered(bytes) => output = Some(String::from_utf8(bytes.to_vec()).unwrap()),
                ResponseBody::Stream(mut stream) => loop {
                    // `cancel_when_idle`: the client goes away once nothing arrives.
                    let next = if s["cancel_when_idle"] == true {
                        match tokio::time::timeout(std::time::Duration::from_millis(200), stream.next()).await {
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
        let want_error = s["error"].as_object().map(|e| {
            (
                e["status"].as_u64().unwrap() as u16,
                e["message"].as_str().unwrap().to_owned(),
            )
        });
        // Go's status 0 is an error without a status code (a scanner error); Rust errors
        // always carry one, so only the message is compared.
        let error = error.map(|(status, message)| match want_error {
            Some((0, _)) => (0, message),
            _ => (status, message),
        });
        assert_eq!(error, want_error, "{name}: error");
        // Go mints Interactions IDs from the wall clock (`interaction_<UnixNano>`); only
        // their shape can match a recorded fixture.
        // Their `created`/`updated` stamps are wall-clock RFC 3339 times as well.
        let norm = |text: &str| {
            let text = ids.replace_all(text, "interaction_<id>");
            let text = stamps.replace_all(&text, r#""$1":"<time>""#);
            unix.replace_all(&text, r#""created":<unix>$1"#).into_owned()
        };
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
        let request = mock.as_ref().and_then(Mock::request);
        assert_eq!(request.as_deref(), s["request"].as_str(), "{name}: upstream request");
        reports.check(s);
        let mut events: std::collections::VecDeque<Captured> = captures.events().into();
        if let Some(wire) = s["request"].as_str() {
            let provider = if s["provider"] == "gemini-interactions" {
                "gemini-interactions"
            } else {
                "gemini"
            };
            // Config keys are API-key accounts (Go AccountInfo); the attribute-only
            // credential has neither an API key nor OAuth metadata.
            let key = s["config_auth"].as_i64().unwrap() >= 0;
            let key_value = wire
                .lines()
                .find_map(|l| l.strip_prefix("X-Goog-Api-Key: "))
                .unwrap_or_default();
            let account = if key { ("api_key", key_value) } else { ("", "") };
            check_attempt(
                name,
                &mut events,
                wire,
                s.get("upstream"),
                s["op"] == "stream",
                provider,
                Some(account),
            );
        }
        assert!(events.is_empty(), "{name}: extra captures {events:?}");
        ran += 1;
    }
    eprintln!("gemini scenarios: {ran} ran, {} wait for translators:", skipped.len());
    for name in &skipped {
        eprintln!("  {name}");
    }
    assert!(ran >= 45, "too few scenarios ran: {ran}");
}

fn pairs(name: &str) -> Vec<(&'static str, &'static str)> {
    fixture()["vectors"][name]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["in"].as_str().unwrap(), p["out"].as_str().unwrap()))
        .collect()
}

/// `helps.FilterSSEUsageMetadata`, in Go's call order (trace IDs are remembered).
#[test]
fn filter_sse_usage_matches_go() {
    let vectors = pairs("filter_sse_usage");
    assert!(vectors.len() > 20);
    for (input, want) in vectors {
        let got = sse::filter_sse_usage_metadata(input.as_bytes());
        assert_eq!(String::from_utf8_lossy(&got), want, "input {input:?}");
    }
}

#[test]
fn json_payload_matches_go() {
    for (input, want) in pairs("json_payload") {
        let got = sse::json_payload(input.as_bytes()).unwrap_or_default();
        assert_eq!(String::from_utf8_lossy(got), want, "input {input:?}");
    }
}

#[test]
fn boundary_user_turns_match_go() {
    let leading = pairs("leading_user_content");
    for (i, (input, want)) in leading.iter().enumerate() {
        let path = if i == leading.len() - 1 {
            "request.contents"
        } else {
            "contents"
        };
        let got = payload::ensure_leading_user_content(input.as_bytes().to_vec(), path);
        assert_eq!(String::from_utf8_lossy(&got), *want, "leading {input:?}");
    }
    for (input, want) in pairs("trailing_user") {
        let got = payload::ensure_trailing_user_content(input.as_bytes().to_vec(), "contents");
        assert_eq!(String::from_utf8_lossy(&got), want, "trailing {input:?}");
    }
}

/// `TranslateStreamWithClaudeInputTokens` with a transform-less upstream: chunks pass
/// through and the first `message_start` without input tokens gets the estimate.
#[test]
fn claude_input_tokens_match_go() {
    let cases = fixture()["vectors"]["claude_input_tokens"].as_array().unwrap();
    assert!(cases.len() >= 7);
    for case in cases {
        let original = case["original"].as_str().unwrap();
        let mut state = ClaudeInputTokens::new(
            Format::Claude,
            Format::Gemini,
            Format::Claude,
            Bytes::from(original.to_owned()),
        );
        let mut got = Vec::new();
        for chunk in case["chunks"].as_array().unwrap() {
            let mut out = vec![Bytes::from(chunk.as_str().unwrap().to_owned())];
            state.apply(&mut out);
            got.extend(out.into_iter().map(|b| String::from_utf8(b.to_vec()).unwrap()));
        }
        let want: Vec<String> = case["out"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(got, want, "original {original}");
    }
}

#[test]
fn white_images_are_go_pngs() {
    use base64::Engine;
    for (ratio, (w, h)) in [
        ("16:9", (1344u32, 768u32)),
        ("21:9", (1536, 672)),
        ("7:3", (1024, 1024)),
    ] {
        let mut body =
            br#"{"contents":[{"parts":[{"text":"x"}]}],"generationConfig":{"imageConfig":{"aspectRatio":""}}}"#
                .to_vec();
        cpa_common::json::set_str(&mut body, "generationConfig.imageConfig.aspectRatio", ratio);
        let out = payload::fix_image_aspect_ratio("gemini-2.5-flash-image-preview", body);
        let data = cpa_common::json::get(&out, "contents.0.parts.1.inlineData.data")
            .str()
            .into_owned();
        let png = base64::engine::general_purpose::STANDARD.decode(data).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        // IHDR width and height.
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), w, "{ratio}");
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), h, "{ratio}");
    }
}

/// Go schedules one unconditional delete ten minutes after every remember and never
/// cancels it (rememberStopWithoutUsage), so the first timer wins and an old timer can
/// delete a re-remembered trace.
#[test]
fn stop_memory_follows_go_timers() {
    use std::time::{Duration, Instant};
    let t0 = Instant::now();
    let at0 = |secs: f64| t0 + Duration::from_secs_f64(secs);
    let filter = |line: &str, now| String::from_utf8(sse::filter_sse_usage_metadata_at(line.as_bytes(), now)).unwrap();
    let stop = |t: &str| format!(r#"data: {{"traceId":"{t}","candidates":[{{"finishReason":"STOP"}}]}}"#);
    let usage = |t: &str| format!(r#"data: {{"traceId":"{t}","usageMetadata":{{"n":1}}}}"#);
    let renamed = |t: &str| format!(r#"data: {{"traceId":"{t}","cpaUsageMetadata":{{"n":1}}}}"#);

    // A second remember does not extend the first one's lifetime.
    filter(&stop("clock-a"), at0(0.0));
    filter(&stop("clock-a"), at0(599.0));
    assert_eq!(filter(&usage("clock-a"), at0(600.5)), renamed("clock-a"));

    // Within the window the usage chunk keeps its usage and consumes the entry.
    filter(&stop("clock-b"), at0(0.0));
    assert_eq!(filter(&usage("clock-b"), at0(100.0)), usage("clock-b"));
    assert_eq!(filter(&usage("clock-b"), at0(101.0)), renamed("clock-b"));
    // Re-remembered after consumption: the first timer still deletes it at 600s.
    filter(&stop("clock-b"), at0(200.0));
    assert_eq!(filter(&usage("clock-b"), at0(601.0)), renamed("clock-b"));

    // Without an older timer the same timeline keeps the usage.
    filter(&stop("clock-c"), at0(200.0));
    assert_eq!(filter(&usage("clock-c"), at0(601.0)), usage("clock-c"));
}

/// Before a terminal error reaches a Responses client, the frame the translator is still
/// joining is written first (Go's responsesSSEFramer.Flush in WriteTerminalError).
#[tokio::test]
async fn pending_responses_frame_is_flushed_before_terminal_error() {
    struct Pending(Option<Bytes>);
    impl StreamTranslator for Pending {
        fn event(&mut self, event: &[u8]) -> Result<Vec<Bytes>, cpa_translate::Error> {
            if event == br#"{"fail":1}"# {
                return Err(cpa_translate::Error("bad tool input".into()));
            }
            self.0 = Some(Bytes::from(format!(
                "event: x\ndata: {}\n\n",
                String::from_utf8_lossy(event)
            )));
            Ok(Vec::new())
        }
        fn finish(&mut self) -> Result<Vec<Bytes>, cpa_translate::Error> {
            Ok(Vec::new())
        }
        fn flush_frames(&mut self) -> Vec<Bytes> {
            self.0.take().into_iter().collect()
        }
    }
    let output = Output {
        translator: Box::new(Pending(None)),
        client: Format::OpenAIResponse,
        raw: false,
        executor_chunks: false,
        claude: ClaudeInputTokens::new(
            Format::OpenAIResponse,
            Format::Gemini,
            Format::OpenAIResponse,
            Bytes::new(),
        ),
        usage: Default::default(),
        capture: Default::default(),
        claude_errors: false,
        retained: None,
        drop_rule: Default::default(),
    };
    let lines = futures_util::stream::iter([
        Ok(Bytes::from_static(br#"data: {"a":1}"#)),
        Ok(Bytes::from_static(br#"data: {"fail":1}"#)),
        Ok(Bytes::from_static(br#"data: {"never":1}"#)),
    ])
    .boxed();
    let items: Vec<Result<Bytes, ExecError>> = gemini_lines(lines, output, Format::Gemini).collect().await;
    assert_eq!(items.len(), 2, "{items:?}");
    assert_eq!(
        items[0].as_ref().unwrap(),
        &Bytes::from_static(b"event: x\ndata: {\"a\":1}\n\n")
    );
    let error = items[1].as_ref().unwrap_err();
    assert_eq!((error.status, &error.body[..]), (502, EMPTY_TRANSLATION.as_bytes()));
}

/// helps.StopApplyPatchStream / EndApplyPatchStream: a failed tool input ends the stream
/// with the sanitized 502 right after that event's frames, and at EOF the translator's
/// finalization runs before the synthetic `[DONE]` (which a stopped stream never sees).
#[tokio::test]
async fn apply_patch_hooks_follow_go_order() {
    #[derive(Default)]
    struct Hooks {
        seen: Arc<Mutex<Vec<String>>>,
        fail_on: Option<&'static [u8]>,
        failed: bool,
        finalize_fails: bool,
    }
    impl StreamTranslator for Hooks {
        fn event(&mut self, event: &[u8]) -> Result<Vec<Bytes>, cpa_translate::Error> {
            self.seen
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(event).into_owned());
            if self.fail_on == Some(event) {
                self.failed = true;
                return Ok(vec![Bytes::from_static(b"event: response.failed\ndata: {}\n\n")]);
            }
            Ok(vec![Bytes::from(format!(
                "data: {}\n\n",
                String::from_utf8_lossy(event)
            ))])
        }
        fn finish(&mut self) -> Result<Vec<Bytes>, cpa_translate::Error> {
            self.seen.lock().unwrap().push("finish".into());
            Ok(Vec::new())
        }
        fn tool_input_failed(&self) -> bool {
            self.failed
        }
        fn finalize_tool_input(&mut self) -> Vec<Bytes> {
            self.seen.lock().unwrap().push("finalize".into());
            if self.finalize_fails {
                self.failed = true;
                return vec![Bytes::from_static(b"event: response.failed\ndata: {\"eof\":1}\n\n")];
            }
            Vec::new()
        }
    }
    let run = |hooks: Hooks| async move {
        let output = Output {
            translator: Box::new(hooks),
            client: Format::OpenAI,
            raw: false,
            executor_chunks: false,
            claude: ClaudeInputTokens::new(Format::OpenAI, Format::Gemini, Format::OpenAI, Bytes::new()),
            usage: Default::default(),
            capture: Default::default(),
            claude_errors: false,
            retained: None,
            drop_rule: Default::default(),
        };
        let lines = futures_util::stream::iter([
            Ok(Bytes::from_static(br#"data: {"a":1}"#)),
            Ok(Bytes::from_static(br#"data: {"b":2}"#)),
        ])
        .boxed();
        let items: Vec<Result<Bytes, ExecError>> = gemini_lines(lines, output, Format::Gemini).collect().await;
        items
            .into_iter()
            .map(|i| match i {
                Ok(b) => String::from_utf8(b.to_vec()).unwrap(),
                Err(e) => format!("ERR {} {}", e.status, String::from_utf8_lossy(&e.body)),
            })
            .collect::<Vec<_>>()
    };
    let stop = format!("ERR 502 {EMPTY_TRANSLATION}");

    let seen = Arc::new(Mutex::new(Vec::new()));
    let out = run(Hooks {
        seen: seen.clone(),
        ..Hooks::default()
    })
    .await;
    assert_eq!(out, ["data: {\"a\":1}\n\n", "data: {\"b\":2}\n\n", "data: [DONE]\n\n"]);
    assert_eq!(
        *seen.lock().unwrap(),
        [r#"{"a":1}"#, r#"{"b":2}"#, "finalize", "[DONE]", "finish"]
    );

    let seen = Arc::new(Mutex::new(Vec::new()));
    let out = run(Hooks {
        seen: seen.clone(),
        fail_on: Some(br#"{"a":1}"#),
        ..Hooks::default()
    })
    .await;
    assert_eq!(out, ["event: response.failed\ndata: {}\n\n".to_owned(), stop.clone()]);
    assert_eq!(*seen.lock().unwrap(), [r#"{"a":1}"#]);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let out = run(Hooks {
        seen: seen.clone(),
        finalize_fails: true,
        ..Hooks::default()
    })
    .await;
    assert_eq!(
        out,
        [
            "data: {\"a\":1}\n\n".to_owned(),
            "data: {\"b\":2}\n\n".to_owned(),
            "event: response.failed\ndata: {\"eof\":1}\n\n".to_owned(),
            stop
        ]
    );
    assert_eq!(*seen.lock().unwrap(), [r#"{"a":1}"#, r#"{"b":2}"#, "finalize"]);
}

/// TestGeminiExecutorAppliesPayloadRulesAfterLeadingUserNormalization (868e137): the
/// rules see the synthetic leading user turn, not the client's first turn.
#[tokio::test]
async fn payload_rules_run_after_the_leading_user_turn() {
    let reply = serde_json::json!({
        "status": 200,
        "headers": [["Content-Type", "application/json"]],
        "body": r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}]}"#,
    });
    let mock = Mock::start(&reply).await;
    let cfg = Config::parse(
        r#"
payload:
  override:
    - models: [{name: gemini-3.7-flash, protocol: gemini}]
      params: {"contents.0.parts.0.text": payload override}
"#,
    )
    .unwrap();
    let s = serde_json::json!({
        "provider": "gemini", "config_auth": -1, "op": "execute", "source": "gemini", "model": "gemini-3.7-flash",
        "attributes": {"api_key": "test-key", "base_url": "http://UPSTREAM"},
        "payload": r#"{"contents":[{"role":"model","parts":[{"text":"prior output"}]},{"role":"user","parts":[{"text":"continue"}]}]}"#,
    });
    let cred = credential(&s, &cfg, mock.addr());
    GeminiExecutor::default()
        .execute(&cred, request(&s), &cfg)
        .await
        .unwrap();
    let wire = mock.request().unwrap();
    let body = wire.split_once("\r\n\r\n").unwrap().1.as_bytes().to_vec();
    assert_eq!(gj::get(&body, "contents.#").int(), 3, "{wire}");
    assert_eq!(gj::get(&body, "contents.0.role").str(), "user", "{wire}");
    assert_eq!(
        gj::get(&body, "contents.0.parts.0.text").str(),
        "payload override",
        "{wire}"
    );
    assert_eq!(
        gj::get(&body, "contents.1.parts.0.text").str(),
        "prior output",
        "{wire}"
    );
}

/// A count whose original request fails to translate refuses rather than checking
/// `default` rules against an empty baseline: Go's partial translation keeps the
/// client's temperature, so no default would apply (DIFFERENCES-FROM-GO.md).
#[tokio::test]
async fn count_refuses_when_the_original_does_not_translate() {
    let reply = serde_json::json!({
        "status": 200,
        "headers": [["Content-Type", "application/json"]],
        "body": r#"{"totalTokens":3}"#,
    });
    let mock = Mock::start(&reply).await;
    let cfg = Config::parse(
        r#"
payload:
  default:
    - models: [{name: gemini-3.7-flash, protocol: gemini}]
      params: {"generationConfig.temperature": 0.9}
"#,
    )
    .unwrap();
    let s = serde_json::json!({
        "provider": "gemini", "config_auth": -1, "op": "count", "source": "claude", "model": "gemini-3.7-flash",
        "attributes": {"api_key": "test-key", "base_url": "http://UPSTREAM"},
        "payload": r#"{"model":"gemini-3.7-flash","max_tokens":16,"temperature":0.2,"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#,
        "original": r#"{"model":"gemini-3.7-flash","max_tokens":16,"temperature":0.2,"messages":[{"role":"user","content":[{"type":"container_upload","source":{"type":"file","file_id":"f1"}}]}]}"#,
    });
    let cred = credential(&s, &cfg, mock.addr());
    let got = GeminiExecutor::default().execute(&cred, request(&s), &cfg).await;
    assert!(got.is_err(), "{:?}", mock.request());
    assert_eq!(mock.request(), None);
}

/// Go ignores a refusal only the original (baseline) body hits: the working body is the
/// one sent (TranslateRequestPairWithAPIKeyModelCompatibilityAndUpdateIntent).
#[tokio::test]
async fn a_refused_baseline_alone_does_not_refuse_the_request() {
    let reply = serde_json::json!({
        "status": 200,
        "headers": [["Content-Type", "application/json"]],
        "body": r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}]}"#,
    });
    let mock = Mock::start(&reply).await;
    let cfg = Config::parse("").unwrap();
    let mut s = serde_json::json!({
        "provider": "gemini", "config_auth": -1, "op": "execute", "source": "claude", "model": "gemini-2.5-pro",
        "attributes": {"api_key": "test-key", "base_url": "http://UPSTREAM"},
        "payload": r#"{"model":"gemini-2.5-pro","max_tokens":16,"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#,
        "original": r#"{"model":"gemini-2.5-pro","max_tokens":16,"messages":[{"role":"user","content":[{"type":"container_upload","source":{"type":"file","file_id":"f1"}}]}]}"#,
    });
    let cred = credential(&s, &cfg, mock.addr());
    GeminiExecutor::default()
        .execute(&cred, request(&s), &cfg)
        .await
        .unwrap();
    let wire = mock.request().unwrap();
    assert!(wire.contains(r#""text":"hi""#), "{wire}");
    // The working body's own refusal still answers without calling upstream.
    s["payload"] = s["original"].clone();
    let mock = Mock::start(&reply).await;
    let cred = credential(&s, &cfg, mock.addr());
    assert!(
        GeminiExecutor::default()
            .execute(&cred, request(&s), &cfg)
            .await
            .is_err()
    );
    assert_eq!(mock.request(), None);
}
