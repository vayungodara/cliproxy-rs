//! Test harness shared by the Kimi, Meta and Devin differential tests.
//!
//! Fixtures under `tests/device_fixtures/<provider>/` were produced by running the real Go
//! executors (CLIProxyAPI 6fecc6e) against a raw HTTP/1.1 capture server; see
//! `tests/device_fixtures/README.md`. This module replays the same scripted upstream
//! responses to the Rust executors and captures their requests the same way, so ordered
//! header lines and body bytes can be compared directly.

use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use cpa_core::credential::Credential;
use cpa_core::exec::{Caller, ExecRequest, ExecResponse, Operation, ResponseBody};
use cpa_core::format::Format;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

pub(crate) fn fixture(provider: &str, name: &str) -> Value {
    let path = format!(
        "{}/tests/device_fixtures/{provider}/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap()
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Captured {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Captured {
    pub(crate) fn from_fixture(value: &Value) -> Self {
        let body = match value["body_b64"].as_str() {
            Some(b64) if !b64.is_empty() => STANDARD.decode(b64).unwrap(),
            _ => value["body"].as_str().unwrap_or_default().as_bytes().to_vec(),
        };
        Self {
            method: value["method"].as_str().unwrap().to_owned(),
            target: value["target"].as_str().unwrap().to_owned(),
            headers: value["headers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| (h[0].as_str().unwrap().to_owned(), h[1].as_str().unwrap().to_owned()))
                .collect(),
            body,
        }
    }

    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// One scripted upstream answer: status, ordered headers, body.
type Scripted = (u16, Vec<(String, String)>, Vec<u8>);

/// Raw HTTP/1.1 capture server answering with scripted responses in order.
pub(crate) struct Mock {
    pub url: String,
    captured: Arc<Mutex<Vec<Captured>>>,
}

impl Mock {
    pub(crate) async fn start(responses: &Value) -> Self {
        Self::start_replacing(responses, None).await
    }

    /// Like [`Mock::start`], with every occurrence of `go_origin` in the scripted bodies
    /// replaced by this mock's URL (Go fixtures that answer with their own server URL).
    pub(crate) async fn start_replacing(responses: &Value, go_origin: Option<&str>) -> Self {
        let mut scripted: Vec<Scripted> = responses
            .as_array()
            .map(|list| {
                list.iter()
                    .map(|r| {
                        let body = match r["body_b64"].as_str() {
                            Some(b64) if !b64.is_empty() => STANDARD.decode(b64).unwrap(),
                            _ => r["body"].as_str().unwrap_or_default().as_bytes().to_vec(),
                        };
                        let headers = r["headers"]
                            .as_array()
                            .map(|hs| {
                                hs.iter()
                                    .map(|h| (h[0].as_str().unwrap().to_owned(), h[1].as_str().unwrap().to_owned()))
                                    .collect()
                            })
                            .unwrap_or_default();
                        (r["status"].as_u64().unwrap() as u16, headers, body)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        if let Some(origin) = go_origin {
            for (_, _, body) in &mut scripted {
                if let Ok(text) = std::str::from_utf8(body) {
                    *body = text.replace(origin, &url).into_bytes();
                }
            }
        }
        let captured: Arc<Mutex<Vec<Captured>>> = Arc::default();
        let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(scripted)));
        let sink = captured.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let sink = sink.clone();
                let queue = queue.clone();
                tokio::spawn(async move {
                    let (read, mut write) = socket.into_split();
                    let mut reader = BufReader::new(read);
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let mut parts = line.trim_end().splitn(3, ' ');
                        let method = parts.next().unwrap_or_default().to_owned();
                        let target = parts.next().unwrap_or_default().to_owned();
                        let mut headers = Vec::new();
                        let (mut length, mut chunked) = (0usize, false);
                        loop {
                            let mut h = String::new();
                            if reader.read_line(&mut h).await.unwrap_or(0) == 0 {
                                return;
                            }
                            let h = h.trim_end_matches(['\r', '\n']);
                            if h.is_empty() {
                                break;
                            }
                            let (name, value) = h.split_once(':').unwrap_or((h, ""));
                            let value = value.trim().to_owned();
                            if name.eq_ignore_ascii_case("content-length") {
                                length = value.parse().unwrap_or(0);
                            }
                            if name.eq_ignore_ascii_case("transfer-encoding") {
                                chunked = value.eq_ignore_ascii_case("chunked");
                            }
                            headers.push((name.to_owned(), value));
                        }
                        let mut body = Vec::new();
                        if chunked {
                            loop {
                                let mut size = String::new();
                                reader.read_line(&mut size).await.unwrap();
                                let n = usize::from_str_radix(size.trim().split(';').next().unwrap(), 16).unwrap();
                                let mut chunk = vec![0; n + 2];
                                reader.read_exact(&mut chunk).await.unwrap();
                                if n == 0 {
                                    break;
                                }
                                body.extend_from_slice(&chunk[..n]);
                            }
                        } else {
                            body.resize(length, 0);
                            reader.read_exact(&mut body).await.unwrap();
                        }
                        sink.lock().unwrap().push(Captured {
                            method,
                            target,
                            headers,
                            body,
                        });
                        let (status, headers, body) = queue.lock().unwrap().pop_front().unwrap_or((
                            500,
                            Vec::new(),
                            b"no scripted response".to_vec(),
                        ));
                        let mut out = format!("HTTP/1.1 {status} {}\r\n", reason(status)).into_bytes();
                        for (n, v) in &headers {
                            out.extend_from_slice(format!("{n}: {v}\r\n").as_bytes());
                        }
                        out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
                        out.extend_from_slice(&body);
                        if write.write_all(&out).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Self { url, captured }
    }

    pub(crate) fn captured(&self) -> Vec<Captured> {
        self.captured.lock().unwrap().clone()
    }
}

fn reason(status: u16) -> &'static str {
    http::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("Unknown")
}

/// Asserts the same method, target, ordered header names and values, and body bytes.
/// `masked` headers must be present in both at the same position; their values differ by
/// environment (Host port, hostname, random device IDs).
pub(crate) fn assert_same_request(name: &str, go: &Captured, rust: &Captured, masked: &[&str]) {
    assert_eq!(rust.method, go.method, "{name}: method");
    assert_eq!(rust.target, go.target, "{name}: request target");
    let names = |c: &Captured| c.headers.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
    assert_eq!(names(rust), names(go), "{name}: header order and spelling");
    for ((n, rv), (_, gv)) in rust.headers.iter().zip(&go.headers) {
        if !masked.iter().any(|m| m.eq_ignore_ascii_case(n)) {
            assert_eq!(rv, gv, "{name}: header {n}");
        }
    }
    assert_eq!(
        String::from_utf8_lossy(&rust.body),
        String::from_utf8_lossy(&go.body),
        "{name}: body bytes"
    );
}

/// Builds the credential the Go generator used, pointing `base_key` at the mock.
pub(crate) fn credential(provider: &str, fixture: &Value, base_key: Option<(&str, String)>) -> Credential {
    let mut metadata = fixture["credential"].as_object().cloned().unwrap_or_default();
    if let Some((key, url)) = base_key {
        metadata.insert(key.into(), url.into());
    }
    let mut c = Credential::from_file(
        std::path::Path::new("/fixture"),
        &std::path::Path::new("/fixture").join(format!("{provider}-fixture.json")),
        metadata,
    )
    .unwrap();
    c.provider = provider.into();
    if let Some(attrs) = fixture["attributes"].as_object() {
        for (k, v) in attrs {
            c.attributes
                .insert(k.clone(), v.as_str().unwrap_or_default().to_owned());
        }
    }
    c
}

pub(crate) fn format(name: &str) -> Format {
    Format::parse(name).unwrap_or_else(|| panic!("format {name}"))
}

/// The request the Go generator sent (see rsfixRunExecutor).
pub(crate) fn request(fixture: &Value, principal: &str) -> ExecRequest {
    let r = &fixture["request"];
    let body = Bytes::from(r["body"].as_str().unwrap().to_owned());
    let source = format(r["source"].as_str().unwrap());
    let mut headers = http::HeaderMap::new();
    if let Some(list) = r["headers"].as_array() {
        for h in list {
            headers.append(
                http::HeaderName::from_bytes(h[0].as_str().unwrap().as_bytes()).unwrap(),
                h[1].as_str().unwrap().parse().unwrap(),
            );
        }
    }
    ExecRequest {
        operation: if r["count"].as_bool() == Some(true) {
            Operation::CountTokens
        } else {
            Operation::Generate
        },
        source_format: source,
        response_format: source,
        requested_model: r["model"].as_str().unwrap().into(),
        model: r["model"].as_str().unwrap().into(),
        original_body: body.clone(),
        body,
        stream: r["stream"].as_bool().unwrap_or(false),
        alt: r["alt"].as_str().map(str::to_owned),
        session: None,
        execution_session: None,
        derived_session: None,
        resolved_model: None,
        usage: Default::default(),
        request_path: String::new(),
        headers,
        caller: Caller {
            principal: principal.into(),
            source: "authorization",
        },
    }
}

/// Downstream result in the fixture's shape.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Downstream {
    pub body: Option<String>,
    pub chunks: Vec<String>,
    pub err_status: Option<u16>,
    pub err_body: Option<String>,
    pub raw: Vec<u8>,
    /// The failure the server records for the attempt's error, if any ([`go_failure`]).
    pub failure: Option<(u16, String)>,
}

/// The failure status and body the server records for an executor error when the
/// executor publishes nothing itself (cpa_server::classify `go_status`, `error_text`):
/// transport faults carry no status, as Go's plain errors.
pub(crate) fn go_failure(e: &cpa_core::exec::ExecError) -> (u16, String) {
    let status = if e.scope == cpa_core::exec::FailureScope::Transport {
        0
    } else {
        e.status
    };
    let text = if e.body.is_empty() {
        format!("status {}", e.status)
    } else {
        String::from_utf8_lossy(&e.body).into_owned()
    };
    (status, text)
}

/// The raw body of scripted response `index` (base64 bodies decoded).
pub(crate) fn response_body(fixture: &Value, index: usize) -> Vec<u8> {
    let r = &fixture["responses"][index];
    match r["body_b64"].as_str() {
        Some(b64) if !b64.is_empty() => STANDARD.decode(b64).unwrap(),
        _ => r["body"].as_str().unwrap_or_default().as_bytes().to_vec(),
    }
}

pub(crate) async fn downstream(result: Result<ExecResponse, cpa_core::exec::ExecError>) -> Downstream {
    match result {
        Err(e) => Downstream {
            err_status: Some(e.status),
            err_body: Some(String::from_utf8_lossy(&e.body).into_owned()),
            failure: Some(go_failure(&e)),
            ..Downstream::default()
        },
        Ok(response) => match response.body {
            ResponseBody::Buffered(b) => Downstream {
                body: Some(String::from_utf8_lossy(&b).into_owned()),
                raw: b.to_vec(),
                ..Downstream::default()
            },
            ResponseBody::Stream(mut s) => {
                let mut out = Downstream::default();
                while let Some(item) = s.next().await {
                    match item {
                        Ok(b) => out.chunks.push(String::from_utf8_lossy(&b).into_owned()),
                        Err(e) => {
                            out.err_status = Some(e.status);
                            out.err_body = Some(String::from_utf8_lossy(&e.body).into_owned());
                            out.failure = Some(go_failure(&e));
                        }
                    }
                }
                out
            }
        },
    }
}

/// `data:` payloads of SSE frames, in order.
pub(crate) fn data_payloads(frames: &[String]) -> Vec<String> {
    frames
        .iter()
        .flat_map(|f| {
            f.lines()
                .filter_map(|l| l.strip_prefix("data:").map(|d| d.trim().to_owned()))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// A translator that echoes events and rejects apply_patch input on a marked line.
pub(crate) struct PatchProbe {
    pub failed: bool,
    pub fail_on_finalize: bool,
    pub log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl cpa_translate::StreamTranslator for PatchProbe {
    fn event(&mut self, event: &[u8]) -> Result<Vec<Bytes>, cpa_translate::Error> {
        let event = String::from_utf8_lossy(event).into_owned();
        self.log.lock().unwrap().push(format!("event {event}"));
        if event.contains("BAD_PATCH") {
            self.failed = true;
        }
        Ok(vec![Bytes::from(format!("frame {event}"))])
    }
    fn finish(&mut self) -> Result<Vec<Bytes>, cpa_translate::Error> {
        self.log.lock().unwrap().push("finish".into());
        Ok(vec![Bytes::from_static(b"finished")])
    }
    fn flush_frames(&mut self) -> Vec<Bytes> {
        vec![Bytes::from_static(b"flushed")]
    }
    fn tool_input_failed(&self) -> bool {
        self.failed
    }
    fn finalize_tool_input(&mut self) -> Vec<Bytes> {
        self.log.lock().unwrap().push("finalize".into());
        if self.fail_on_finalize {
            self.failed = true;
            return vec![Bytes::from_static(b"response.failed")];
        }
        vec![]
    }
}

/// An error Go reports as `<message>: <upstream body>` where Rust withholds the body:
/// Go's text must be Rust's message plus the body, and Rust's must not contain it.
pub(crate) fn assert_go_message_without_body(name: &str, rust: &str, go: &str) {
    let body = go
        .strip_prefix(rust)
        .and_then(|rest| rest.strip_prefix(": "))
        .unwrap_or_else(|| panic!("{name}: {go:?} must be {rust:?} plus the upstream body"));
    assert!(!body.is_empty(), "{name}: Go appends a body");
    assert!(!rust.contains(body), "{name}: {rust:?} must not carry the body");
}

/// Captures every tracing event emitted on the current thread while installed (no
/// subscriber crate needed): each event's fields, formatted.
#[derive(Clone, Default)]
pub(crate) struct LogCapture(pub Arc<Mutex<Vec<String>>>);

/// An installed [`LogCapture`]; capturing ends when it is dropped.
pub(crate) struct LogCaptureGuard {
    _default: tracing::subscriber::DefaultGuard,
    _second: tracing::Dispatch,
}

impl LogCapture {
    /// Installs the capture as this thread's default subscriber. With a single live
    /// dispatcher, tracing-core computes a callsite's cached interest from the
    /// registering thread's default alone (`Rebuilder::JustOne`), so a parallel test
    /// that hits the same callsite first, with no subscriber, caches `never` for
    /// everyone and the event never reaches this capture. A second live dispatcher
    /// makes every registration consult all live dispatchers, this one included.
    pub(crate) fn install(&self) -> LogCaptureGuard {
        let second = tracing::Dispatch::new(LogCapture::default());
        LogCaptureGuard {
            _default: tracing::subscriber::set_default(self.clone()),
            _second: second,
        }
    }
}

impl tracing::Subscriber for LogCapture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{}={value:?} ", field.name()));
            }
        }
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// A `UsageSink` call other than a reported payload (Server 13's additions).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum UsageEvent {
    RequestFor(String, Vec<u8>),
    UpstreamModel(String),
    ResponseModel(String),
    RoundTripStarted,
    FirstByte,
    Token(bool),
    Publish,
    PublishFailure(u16, String),
    UsageRequired,
    Failed,
}

/// Records what an executor reports to its usage sink: the payloads (Server 6) and,
/// in order, every other call.
#[derive(Default)]
pub(crate) struct UsageLog(Mutex<Vec<(&'static str, Format, Vec<u8>)>>, Mutex<Vec<UsageEvent>>);

impl cpa_core::exec::UsageObserver for UsageLog {
    fn response_body(&self, format: Format, body: &[u8]) {
        self.0.lock().unwrap().push(("body", format, body.to_vec()));
    }
    fn response_line(&self, format: Format, line: &[u8]) {
        self.0.lock().unwrap().push(("line", format, line.to_vec()));
    }
    fn request(&self, format: Format, payload: &[u8]) {
        self.0.lock().unwrap().push(("request", format, payload.to_vec()));
    }
    fn request_for(&self, identifier: &str, payload: &[u8]) {
        self.event(UsageEvent::RequestFor(identifier.into(), payload.to_vec()));
    }
    fn upstream_model(&self, model: &str) {
        self.event(UsageEvent::UpstreamModel(model.into()));
    }
    fn response_model(&self, model: &str) {
        self.event(UsageEvent::ResponseModel(model.into()));
    }
    fn round_trip_started(&self) {
        self.event(UsageEvent::RoundTripStarted);
    }
    fn first_byte(&self) {
        self.event(UsageEvent::FirstByte);
    }
    fn token_event(&self, is_token: bool) {
        self.event(UsageEvent::Token(is_token));
    }
    fn publish(&self) {
        self.event(UsageEvent::Publish);
    }
    fn publish_failure(&self, status: u16, body: &str) {
        self.event(UsageEvent::PublishFailure(status, body.into()));
    }
    fn usage_required(&self) {
        self.event(UsageEvent::UsageRequired);
    }
    fn failed(&self) {
        self.event(UsageEvent::Failed);
    }
}

impl UsageLog {
    pub(crate) fn sink(self: &Arc<Self>) -> cpa_core::exec::UsageSink {
        cpa_core::exec::UsageSink::new(self.clone())
    }

    fn event(&self, event: UsageEvent) {
        self.1.lock().unwrap().push(event);
    }

    pub(crate) fn events(&self) -> Vec<UsageEvent> {
        self.1.lock().unwrap().clone()
    }

    /// Whether a TTFT is recorded: the server's `Ttft` (Go `StartResponseTTFT`,
    /// `MarkFirstResponseByte`, `ObserveTokenEvent`, `ttftDuration` > 0).
    fn ttft_set(&self) -> bool {
        let (mut started, mut ttft, mut packet) = (false, false, false);
        for event in self.events() {
            match event {
                UsageEvent::RoundTripStarted if !ttft => started = true,
                UsageEvent::FirstByte if started => (ttft, started) = (true, false),
                UsageEvent::Token(is_token) if started => {
                    packet = true;
                    if is_token {
                        (ttft, started) = (true, false);
                    }
                }
                _ => {}
            }
        }
        ttft || packet
    }

    /// The reported upstream payloads (bodies and lines, `data:` stripped), in order.
    fn payloads(&self) -> Vec<(Format, Vec<u8>)> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(kind, _, _)| *kind != "request")
            .map(|(_, f, p)| {
                let t = p.trim_ascii();
                (*f, t.strip_prefix(b"data:").map_or(t, <[u8]>::trim_ascii).to_vec())
            })
            .collect()
    }
}

/// Token counts in a reported upstream payload, by the payload's format:
/// (input, output, total, cached).
fn reported_tokens(format: Format, payload: &[u8]) -> Option<[i64; 4]> {
    use cpa_common::json as gj;
    let pick = |paths: &[&str]| paths.iter().map(|p| gj::get(payload, p)).find(|r| r.exists());
    let (usage, keys): (_, [&str; 4]) = match format {
        Format::OpenAI => (
            pick(&["usage"]),
            [
                "prompt_tokens",
                "completion_tokens",
                "total_tokens",
                "prompt_tokens_details.cached_tokens",
            ],
        ),
        Format::Codex | Format::OpenAIResponse => (
            pick(&["response.usage", "usage"]),
            [
                "input_tokens",
                "output_tokens",
                "total_tokens",
                "input_tokens_details.cached_tokens",
            ],
        ),
        Format::Interactions => (
            pick(&["interaction.usage", "usage"]),
            [
                "total_input_tokens",
                "total_output_tokens",
                "total_tokens",
                "total_cached_tokens",
            ],
        ),
        _ => (None, ["", "", "", ""]),
    };
    let usage = usage?;
    let mut t = keys.map(|k| usage.get(k).int());
    // Go's usage parsers fill a missing total with input + output.
    if t[2] == 0 {
        t[2] = t[0] + t[1];
    }
    Some(t)
}

/// The response model a reported payload names, by format (Go's extractors need one
/// valid JSON value).
fn reported_model(format: Format, payload: &[u8]) -> Option<String> {
    if !cpa_common::json::valid(payload) {
        return None;
    }
    let paths: &[&str] = match format {
        Format::Codex | Format::OpenAIResponse => &["response.model", "model"],
        Format::Interactions => &["interaction.model", "model"],
        _ => &["model"],
    };
    paths
        .iter()
        .map(|p| cpa_common::json::get(payload, p).str().into_owned())
        .find(|m| !m.is_empty())
}

/// The record the server publishes from these reports (cpa_server usage_record
/// `Tracker::publish`): the executor's first `publish`/`publish_failure`, else the
/// attempt's `failure`, else a success, which `usage_required` drops when no usage was
/// reported. `(failed, status, body)`; `None` when nothing is published.
fn rust_record(log: &UsageLog, failure: Option<&(u16, String)>) -> Option<(bool, u16, String)> {
    let events = log.events();
    let published = events.iter().find_map(|e| match e {
        UsageEvent::Publish => Some((false, 0, String::new())),
        UsageEvent::PublishFailure(status, body) => Some((true, *status, body.trim().to_owned())),
        _ => None,
    });
    if published.is_some() {
        return published;
    }
    if let Some((status, body)) = failure {
        return Some((true, *status, body.trim().to_owned()));
    }
    let usage_reported = events.contains(&UsageEvent::Failed)
        || log.0.lock().unwrap().iter().any(|(kind, format, payload)| {
            *kind == "body" || {
                let t = payload.trim_ascii();
                let p = t.strip_prefix(b"data:").map_or(t, <[u8]>::trim_ascii);
                reported_tokens(*format, p).is_some() || !crate::kimi_http::response_tier(p).is_empty()
            }
        });
    if events.contains(&UsageEvent::UsageRequired) && !usage_reported {
        return None;
    }
    Some((false, 0, String::new()))
}

/// Checks the usage reports against the record Go's `UsageReporter` published for the
/// same fixture (`extra.usage`): whether a record is published at all, its outcome
/// (failure status and body, from `failure` when the executor publishes nothing), the
/// TTFT presence, the reported tokens (none for a failed attempt), service tier and
/// response model, and the translated reasoning effort. `failure` is the attempt's
/// error as [`go_failure`] maps it.
pub(crate) fn assert_usage_like_go(name: &str, fx: &Value, log: &UsageLog, failure: Option<&(u16, String)>) {
    if fx["request"]["count"].as_bool() == Some(true) {
        // Token counting is not a tracked attempt.
        return;
    }
    if fx["extra"].get("usage").is_none() {
        // This generator did not capture usage.
        return;
    }
    let rust = rust_record(log, failure);
    let Some(record) = fx["extra"]["usage"].as_array().and_then(|r| r.first()) else {
        assert_eq!(rust, None, "{name}: Go publishes no usage record");
        return;
    };
    let rust = rust.unwrap_or_else(|| panic!("{name}: Go publishes a usage record"));
    if let Some(status) = record["fail_status"].as_u64() {
        let go = (
            record["failed"].as_bool().unwrap(),
            status as u16,
            record["fail_body"].as_str().unwrap().trim().to_owned(),
        );
        assert_eq!(rust, go, "{name}: record outcome (failed, status, body)");
        assert_eq!(
            log.ttft_set(),
            record["ttft_set"].as_bool().unwrap(),
            "{name}: TTFT recorded"
        );
    }
    let events = log.events();
    // Go's SetUpstreamModel never reaches the record (it feeds the substitution
    // warning); the reported model must be the one Go sent upstream. Kimi and Devin set
    // it on every upstream request, Meta never.
    let upstream_model = events.iter().rev().find_map(|e| match e {
        UsageEvent::UpstreamModel(m) => Some(m.clone()),
        _ => None,
    });
    if let Some(sent) = fx["upstream"].as_array().and_then(|u| u.last()) {
        let sent = Captured::from_fixture(sent).body;
        match (fx["credential"]["type"].as_str(), &upstream_model) {
            (Some("kimi" | "kimi-ai"), Some(model)) => assert_eq!(
                cpa_common::json::get(&sent, "model").str(),
                model.as_str(),
                "{name}: upstream model"
            ),
            (Some("devin"), Some(model)) => assert!(
                !model.is_empty() && sent.windows(model.len()).any(|w| w == model.as_bytes()),
                "{name}: upstream model {model:?} is not in Go's request"
            ),
            (Some("meta"), None) => {}
            (provider, model) => panic!("{name}: {provider:?} reported upstream model {model:?}"),
        }
    }
    let effort = match events.iter().rev().find_map(|e| match e {
        UsageEvent::RequestFor(identifier, payload) => Some((identifier.clone(), payload.clone())),
        _ => None,
    }) {
        Some((identifier, payload)) => cpa_common::thinking::extract_translated_reasoning_effort(&payload, &identifier),
        None => log
            .0
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(kind, _, _)| *kind == "request")
            .map(|(_, f, p)| cpa_common::thinking::extract_translated_reasoning_effort(p, f.as_str()))
            .unwrap_or_default(),
    };
    assert_eq!(
        effort,
        record["reasoning_effort"].as_str().unwrap(),
        "{name}: translated reasoning effort"
    );
    let payloads = log.payloads();
    let tokens = if record["failed"].as_bool() == Some(true) {
        // Go's `PublishFailure` publishes an empty detail: nothing reported may carry tokens.
        let carried: Vec<_> = payloads
            .iter()
            .filter_map(|(f, p)| reported_tokens(*f, p))
            .filter(|t| *t != [0; 4])
            .collect();
        assert!(
            carried.is_empty(),
            "{name}: a failed attempt reported tokens {carried:?}"
        );
        [0; 4]
    } else {
        payloads
            .iter()
            .rev()
            .find_map(|(f, p)| reported_tokens(*f, p))
            .unwrap_or_default()
    };
    let want = ["input_tokens", "output_tokens", "total_tokens", "cached_tokens"].map(|k| record[k].as_i64().unwrap());
    assert_eq!(tokens, want, "{name}: reported tokens (input, output, total, cached)");
    // Go's buffer ends with the last non-blank tier it observed (Observe never clears it).
    if let Some(want) = record["service_tier"].as_str() {
        let tier = payloads
            .iter()
            .rev()
            .map(|(_, p)| crate::kimi_http::response_tier(p))
            .find(|t| !t.is_empty())
            .unwrap_or_default();
        assert_eq!(tier, want, "{name}: reported response service tier");
    }
    // The executors here report a response model either in payloads or through
    // `response_model` (Devin), never both.
    let model = events
        .iter()
        .rev()
        .find_map(|e| match e {
            UsageEvent::ResponseModel(m) => Some(m.trim().to_owned()),
            _ => None,
        })
        .or_else(|| payloads.iter().rev().find_map(|(f, p)| reported_model(*f, p)));
    assert_eq!(
        model.unwrap_or_default(),
        record["response_model"].as_str().unwrap(),
        "{name}: reported response model"
    );
}

#[test]
fn log_capture_sees_callsites_another_thread_registered_first() {
    // Hit first on a thread without a subscriber. With a single live dispatcher tracing
    // would cache `never` for this callsite there and drop the event below.
    fn probe() {
        tracing::warn!("log capture probe");
    }
    let capture = LogCapture::default();
    let guard = capture.install();
    std::thread::spawn(probe).join().unwrap();
    probe();
    drop(guard);
    let logs = capture.0.lock().unwrap().clone();
    assert_eq!(logs.len(), 1, "{logs:?}");
    assert!(logs[0].contains("log capture probe"), "{logs:?}");
}
