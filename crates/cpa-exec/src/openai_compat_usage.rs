//! Test support: what the OpenAI-compatible and xAI executors report to their
//! `UsageSink`, and the usage record Go's reporter publishes from the same payloads
//! (helps/usage_helpers.go: SetTranslatedReasoningEffort, ParseOpenAIUsage,
//! ParseCodexUsage, StreamUsageBuffer.ObserveOpenAIStream, extractResponseModelEvent's
//! generic rule). The generators record Go's published record; the tests compare.
//!
//! Also the upstream capture the executors record, checked against the rules of Go's
//! logging_helpers call sites.

use std::sync::Mutex;

use cpa_common::json::{self as gj, Res};
use cpa_core::format::Format;
use serde_json::Value;

/// One report to the sink, in order.
pub(crate) enum Report {
    /// `request` / `request_for`: the identifier Go passes to SetTranslatedReasoningEffort.
    Request(String, Vec<u8>),
    Body(Format, Vec<u8>),
    Line(Format, Vec<u8>),
    UsageRequired,
    Publish,
    /// `publish_failure`: Go's status and the published error text.
    PublishFailure(u16, String),
    Discard,
    /// `round_trip_started` (Go `StartResponseTTFT`).
    RoundTrip,
    /// `first_byte` (Go `MarkFirstResponseByte`).
    FirstByte,
}

#[derive(Default)]
pub(crate) struct Recorder(pub Mutex<Vec<Report>>);

impl cpa_core::exec::UsageObserver for Recorder {
    fn response_body(&self, format: Format, body: &[u8]) {
        self.0.lock().unwrap().push(Report::Body(format, body.to_vec()));
    }
    fn response_line(&self, format: Format, line: &[u8]) {
        self.0.lock().unwrap().push(Report::Line(format, line.to_vec()));
    }
    fn request(&self, format: Format, payload: &[u8]) {
        self.request_for(format.as_str(), payload);
    }
    fn request_for(&self, identifier: &str, payload: &[u8]) {
        self.0
            .lock()
            .unwrap()
            .push(Report::Request(identifier.to_owned(), payload.to_vec()));
    }
    fn usage_required(&self) {
        self.0.lock().unwrap().push(Report::UsageRequired);
    }
    fn publish(&self) {
        self.0.lock().unwrap().push(Report::Publish);
    }
    fn publish_failure(&self, status: u16, body: &str) {
        self.0
            .lock()
            .unwrap()
            .push(Report::PublishFailure(status, body.to_owned()));
    }
    fn discard(&self) {
        self.0.lock().unwrap().push(Report::Discard);
    }
    fn round_trip_started(&self) {
        self.0.lock().unwrap().push(Report::RoundTrip);
    }
    fn first_byte(&self) {
        self.0.lock().unwrap().push(Report::FirstByte);
    }
}

/// Go's TTFT marks for one attempt: `StartResponseTTFT` when a request went upstream,
/// before the first `MarkFirstResponseByte`, which comes only once a body byte (or a
/// non-empty message) arrived.
pub(crate) fn ttft_problem(reports: &[Report], sent: bool, body_arrived: bool) -> Option<String> {
    let started = reports.iter().position(|r| matches!(r, Report::RoundTrip));
    let first = reports.iter().position(|r| matches!(r, Report::FirstByte));
    let ok = started.is_some() == sent
        && first.is_some() == body_arrived
        && match (started, first) {
            (Some(s), Some(f)) => s < f,
            _ => true,
        };
    (!ok).then(|| format!("ttft: started {started:?}, first byte {first:?} (sent {sent}, body {body_arrived})"))
}

/// `parseOpenAIStyleUsageNode` for the fields the fixtures record, when the node has
/// token fields (`hasOpenAIStyleUsageTokenFields`).
fn tokens(node: &Res<'_>) -> Option<[i64; 5]> {
    if !node.is_object() {
        return None;
    }
    let pick = |a: &str, b: &str| {
        let r = node.get(a);
        if r.exists() { r } else { node.get(b) }
    };
    // hasOpenAIStyleUsageTokenFields.
    let fields = [
        "total_tokens",
        "prompt_tokens",
        "input_tokens",
        "completion_tokens",
        "output_tokens",
        "prompt_tokens_details.cached_tokens",
        "input_tokens_details.cached_tokens",
        "prompt_tokens_details.cache_write_tokens",
        "prompt_tokens_details.cache_creation_tokens",
        "input_tokens_details.cache_write_tokens",
        "input_tokens_details.cache_creation_tokens",
        "completion_tokens_details.reasoning_tokens",
        "output_tokens_details.reasoning_tokens",
    ];
    if !fields.iter().any(|f| node.get(*f).exists()) {
        return None;
    }
    Some([
        pick("prompt_tokens", "input_tokens").int(),
        pick("completion_tokens", "output_tokens").int(),
        pick(
            "completion_tokens_details.reasoning_tokens",
            "output_tokens_details.reasoning_tokens",
        )
        .int(),
        pick(
            "prompt_tokens_details.cached_tokens",
            "input_tokens_details.cached_tokens",
        )
        .int(),
        node.get("total_tokens").int(),
    ])
}

/// A stream line's JSON payload (`jsonPayload`).
fn payload(line: &[u8]) -> Option<&[u8]> {
    let mut trimmed = line.trim_ascii();
    if let Some(rest) = trimmed.strip_prefix(b"data:") {
        trimmed = rest.trim_ascii();
    }
    (trimmed.first() == Some(&b'{')).then_some(trimmed)
}

/// The record Go publishes for one attempt, derived from the reports.
pub(crate) fn derived(reports: &[Report], failed: bool) -> Value {
    record(reports, failed).0
}

/// What the server publishes for the attempt from the executor's reports (the rules of
/// cpa-server's usage_record.rs): nothing for a discarded attempt; the first explicit
/// publish or failure wins with the reports made before it; otherwise the attempt's
/// outcome, and nothing for a success without usage when the executor declared usage
/// required.
pub(crate) fn published(reports: &[Report], failed: bool) -> Option<Value> {
    if reports.iter().any(|r| matches!(r, Report::Discard)) {
        return None;
    }
    if let Some(i) = reports
        .iter()
        .position(|r| matches!(r, Report::Publish | Report::PublishFailure(..)))
    {
        return Some(record(&reports[..i], matches!(reports[i], Report::PublishFailure(..))).0);
    }
    let (value, has_usage) = record(reports, failed);
    let required = reports.iter().any(|r| matches!(r, Report::UsageRequired));
    (failed || has_usage || !required).then_some(value)
}

/// The record and whether any usage was reported.
fn record(reports: &[Report], failed: bool) -> (Value, bool) {
    let mut effort = String::new();
    let mut usage: Option<[i64; 5]> = None;
    let (mut model, mut model_final) = (String::new(), false);
    for report in reports {
        let (kind, format, data) = match report {
            Report::Request(identifier, data) => {
                effort = cpa_common::thinking::extract_translated_reasoning_effort(data, identifier);
                continue;
            }
            Report::Body(format, data) => ("body", *format, data),
            Report::Line(format, data) => ("line", *format, data),
            _ => continue,
        };
        let Some(json) = payload(data) else { continue };
        let root = gj::parse(json);
        let event = root.get("type").str().into_owned();
        let terminal = matches!(
            event.as_str(),
            "response.completed" | "response.incomplete" | "response.done"
        );
        let found = match (kind, format) {
            ("body", _) => tokens(&root.get("usage")),
            // Codex: the first terminal event's usage.
            (_, Format::Codex | Format::OpenAIResponse) if terminal && usage.is_none() => {
                tokens(&root.get("response.usage"))
            }
            // Chat Completions streams: the latest usage chunk.
            (_, Format::OpenAI) => tokens(&root.get("usage")),
            _ => None,
        };
        if found.is_some() {
            usage = found;
        }
        if model_final {
            continue;
        }
        let served = root.get("response.model").str().into_owned();
        if !served.is_empty() {
            model = served;
            model_final = terminal;
            continue;
        }
        let served = root.get("model").str().into_owned();
        if !served.is_empty() {
            model = served;
            let status = root.get("status").str().into_owned();
            let object = root.get("object").str().into_owned();
            let finish = root.get("choices.0.finish_reason").str().into_owned();
            model_final =
                object == "chat.completion" || !finish.is_empty() || status == "completed" || status == "incomplete";
        }
    }
    let has_usage = usage.is_some();
    let mut t = if failed { [0; 5] } else { usage.unwrap_or_default() };
    // EnsureTokenBreakdownForProvider (server-side accounting): OpenAI-style providers
    // count cached input inside input and reasoning inside output, so a missing total is
    // their sum.
    if t[4] == 0 {
        t[4] = t[0] + t[1];
    }
    let value = serde_json::json!({
        "input": t[0], "output": t[1], "reasoning": t[2], "cached": t[3], "total": t[4],
        "effort": effort, "response_model": model, "failed": failed,
    });
    (value, has_usage)
}

// --- upstream capture ---------------------------------------------------------------------

/// One capture event, owned.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Captured {
    Request {
        websocket: bool,
        url: String,
        method: String,
        headers: Vec<(String, String)>,
        body: String,
        provider: String,
        /// Auth ID, label, type and value.
        auth: [String; 4],
    },
    Metadata(u16, Vec<(String, String)>),
    Error(String),
    Chunk(String),
    Handshake(u16, Vec<(String, String)>),
    WsResponse(String),
    WsError(String, String),
}

#[derive(Default)]
pub(crate) struct CaptureLog(pub Mutex<Vec<Captured>>);

impl CaptureLog {
    pub(crate) fn take(&self) -> Vec<Captured> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

impl cpa_core::exec::CaptureObserver for CaptureLog {
    fn record(&self, event: cpa_core::exec::CaptureEvent<'_>) {
        use cpa_core::exec::CaptureEvent as E;
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let request = |websocket, r: cpa_core::exec::UpstreamRequest<'_>| Captured::Request {
            websocket,
            url: r.url.to_owned(),
            method: r.method.to_owned(),
            headers: r.headers.to_vec(),
            body: text(r.body),
            provider: r.provider.to_owned(),
            auth: [r.auth_id, r.auth_label, r.auth_type, r.auth_value].map(str::to_owned),
        };
        let event = match event {
            E::Request(r) => request(false, r),
            E::ResponseMetadata(status, headers) => Captured::Metadata(status, headers.to_vec()),
            E::ResponseError(e) => Captured::Error(e.to_owned()),
            E::ResponseChunk(b) => Captured::Chunk(text(b)),
            E::WebsocketRequest(r) => request(true, r),
            E::WebsocketHandshake(status, headers) => Captured::Handshake(status, headers.to_vec()),
            E::WebsocketResponse(b) => Captured::WsResponse(text(b)),
            E::WebsocketError { stage, error } => Captured::WsError(stage.to_owned(), error.to_owned()),
        };
        self.0.lock().unwrap().push(event);
    }
}

/// A usage sink recording both usage reports and capture events.
pub(crate) fn sinks() -> (
    std::sync::Arc<Recorder>,
    std::sync::Arc<CaptureLog>,
    cpa_core::exec::UsageSink,
) {
    let recorder = std::sync::Arc::new(Recorder::default());
    let log = std::sync::Arc::new(CaptureLog::default());
    let sink =
        cpa_core::exec::UsageSink::new(recorder.clone()).with_capture(cpa_core::exec::CaptureSink::new(log.clone()));
    (recorder, log, sink)
}

/// One HTTP attempt as the upstream saw it, and what Go's call sites log for it.
pub(crate) struct HttpAttempt<'a> {
    /// The raw request the mock upstream received.
    pub raw: &'a [u8],
    /// The URL Go logs.
    pub url: &'a str,
    pub provider: &'a str,
    pub credential: &'a cpa_core::credential::Credential,
    /// The body Go logs when it differs from the one sent (xAI's video poll, a GET that
    /// logs its payload).
    pub logged_body: Option<&'a str>,
    /// The scripted answer: status, headers, body (before any gzip).
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: &'a str,
    pub logged: Logged,
}

/// How Go logs the response body.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Logged {
    /// One chunk after `io.ReadAll`.
    Whole,
    /// One chunk per `bufio.Scanner` line.
    Lines,
    /// One chunk per raw read.
    Reads,
}

/// Headers the Go transport adds after the executor logs `httpReq.Header`.
const TRANSPORT_HEADERS: [&str; 4] = ["host", "content-length", "accept-encoding", "connection"];

/// The rules of Go's HTTP call sites (`RecordAPIRequest` just before the send with the
/// executor's headers and body, `RecordAPIResponseMetadata` after the headers, then the
/// whole body or each scanned line via `AppendAPIResponseChunk`, and a stream failure via
/// `RecordAPIResponseError`) for one physical attempt.
pub(crate) fn http_capture_problems(events: &[Captured], a: &HttpAttempt<'_>) -> Vec<String> {
    let mut problems = Vec::new();
    let raw = String::from_utf8_lossy(a.raw);
    let (head, sent_body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let mut head_lines = head.split("\r\n");
    let method = head_lines
        .next()
        .unwrap_or_default()
        .split(' ')
        .next()
        .unwrap_or_default();
    let raw_headers: Vec<(String, String)> = head_lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let (auth_type, auth_value) = crate::openai_compat_http::account_info(a.credential);
    let requests = events.iter().filter(|e| matches!(e, Captured::Request { .. })).count();
    if requests != 1 {
        problems.push(format!("capture: {requests} request events for one attempt"));
    }
    match events.first() {
        Some(Captured::Request {
            websocket: false,
            url,
            method: logged_method,
            headers,
            body,
            provider,
            auth,
        }) => {
            if url != a.url {
                problems.push(format!("capture url: {url} != {}", a.url));
            }
            // Each call site logs the method it sends (Go logs POST for every xAI request,
            // including the GET video poll; see docs/DIFFERENCES-FROM-GO.md).
            if logged_method != method {
                problems.push(format!("capture method: {logged_method} (sent {method})"));
            }
            let want_body = a.logged_body.unwrap_or(sent_body);
            if body != want_body {
                problems.push(format!("capture body:\n  logged {body:?}\n  want   {want_body:?}"));
            }
            if provider != a.provider {
                problems.push(format!("capture provider: {provider} != {}", a.provider));
            }
            let want_auth = [
                a.credential.id.clone(),
                a.credential.label.clone(),
                auth_type.to_owned(),
                auth_value,
            ];
            if auth != &want_auth {
                problems.push(format!("capture auth: {auth:?} != {want_auth:?}"));
            }
            for (name, value) in headers {
                if !raw_headers
                    .iter()
                    .any(|(n, v)| *n == name.to_ascii_lowercase() && v == value.trim())
                {
                    problems.push(format!("capture header not sent: {name}: {value}"));
                }
            }
            for (name, value) in &raw_headers {
                // The transport's default User-Agent is not in the request's header map.
                if TRANSPORT_HEADERS.contains(&name.as_str()) || (name == "user-agent" && value == "Go-http-client/1.1")
                {
                    continue;
                }
                if !headers
                    .iter()
                    .any(|(n, v)| n.to_ascii_lowercase() == *name && v.trim() == value)
                {
                    problems.push(format!("sent header not captured: {name}: {value}"));
                }
            }
        }
        other => problems.push(format!("capture: first event {other:?}")),
    }
    match events.get(1) {
        Some(Captured::Metadata(status, headers)) => {
            if *status != a.status {
                problems.push(format!("capture status: {status} != {}", a.status));
            }
            for (name, value) in &a.headers {
                if !headers.iter().any(|(n, v)| n.eq_ignore_ascii_case(name) && v == value) {
                    problems.push(format!("capture response header missing: {name}: {value}"));
                }
            }
        }
        other => problems.push(format!("capture: second event {other:?}")),
    }
    let rest = events.get(2..).unwrap_or_default();
    if a.logged == Logged::Whole {
        if rest != [Captured::Chunk(a.body.to_owned())] {
            problems.push(format!("capture body events: {rest:?}"));
        }
        return problems;
    }
    if a.logged == Logged::Reads {
        let joined: String = rest
            .iter()
            .map(|e| match e {
                Captured::Chunk(c) => c.as_str(),
                _ => "<not a chunk>",
            })
            .collect();
        if joined != a.body {
            problems.push(format!("capture reads: {rest:?}"));
        }
        return problems;
    }
    // bufio.ScanLines: lines without their newline or a trailing CR; the scan stops at
    // Go's terminal frame, so the chunks are a prefix of the lines, then at most one
    // error.
    let lines: Vec<&str> = a
        .body
        .strip_suffix('\n')
        .unwrap_or(a.body)
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let (chunks, tail) = match rest.iter().position(|e| !matches!(e, Captured::Chunk(_))) {
        Some(i) => rest.split_at(i),
        None => (rest, &[][..]),
    };
    for (i, chunk) in chunks.iter().enumerate() {
        if *chunk != Captured::Chunk(lines.get(i).copied().unwrap_or("<none>").to_owned()) {
            problems.push(format!("capture line {i}: {chunk:?} != {:?}", lines.get(i)));
            break;
        }
    }
    if !matches!(tail, [] | [Captured::Error(_)]) {
        problems.push(format!("capture stream tail: {tail:?}"));
    }
    problems
}
