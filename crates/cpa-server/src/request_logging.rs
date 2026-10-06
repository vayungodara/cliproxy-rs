//! Go request_logging.go / response_writer.go and request_logger_writer.go.
//! Request bodies in these files are intentionally unredacted, as in Go. Headers
//! and URL queries are masked before writing; management routes are never captured.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use axum::body::{Body, HttpBody};
use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use chrono::{DateTime, Local, SecondsFormat};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::Runtime;
use crate::management::Management;
use crate::observability::{RequestId, hide_key, mask_query};

const SMALL_BODY: u64 = 1 << 20;
const DEFERRED_BODY: u64 = 32 << 20;
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

tokio::task_local! { static CURRENT: RequestLog; }

pub(crate) fn current() -> Option<RequestLog> {
    CURRENT.try_with(Clone::clone).ok()
}

#[derive(Clone)]
struct Captured;

/// Install at the handler boundary, before axum synthesizes Content-Length or
/// suppresses HEAD bodies. The outer middleware still owns request capture.
pub(crate) async fn response(request: Request, next: Next) -> Response {
    let sink = request.extensions().get::<RequestLog>().cloned();
    let head = request.method() == "HEAD";
    let cors = request.extensions().get::<crate::management::Cors>().is_some();
    let mut response = next.run(request).await;
    if cors {
        crate::management::cors_headers(&mut response);
    }
    match sink {
        Some(sink) => capture_response(sink, head, response).await,
        None => response,
    }
}

fn setting<'a>(config: &'a cpa_core::config::Config, key: &str) -> Option<&'a serde_yaml_ng::Value> {
    config.document.get("observability")?.get("logs")?.get(key)
}

#[derive(Clone)]
struct CaptureState {
    rt: Arc<Runtime>,
    dir: PathBuf,
}

/// Install inside access logging so the UUID is already in request extensions.
/// Commercial mode is a construction-time choice, exactly like Go's server.
pub fn router(management: &Arc<Management>, app: axum::Router) -> axum::Router {
    if management
        .rt
        .config()
        .document
        .get("server")
        .and_then(|s| s.get("commercial-mode"))
        .and_then(serde_yaml_ng::Value::as_bool)
        == Some(true)
    {
        return app;
    }
    // Go resolves relative request-log directories against the config directory.
    let dir = if management.log_dir.is_absolute() {
        management.log_dir.clone()
    } else {
        management
            .path
            .parent()
            .unwrap_or(Path::new("."))
            .join(&management.log_dir)
    };
    app.layer(axum::middleware::from_fn_with_state(
        CaptureState {
            rt: management.rt.clone(),
            dir,
        },
        capture,
    ))
}

fn eligible(request: &Request, path: &str) -> bool {
    if ["/v0/management", "/v8/management", "/management"]
        .iter()
        .any(|p| path.starts_with(p))
    {
        return false;
    }
    request.method() != "GET"
        || (matches!(path, "/v1/responses" | "/backend-api/codex/responses")
            && request
                .headers()
                .get("upgrade")
                .is_some_and(|v| trim(v.as_bytes()).eq_ignore_ascii_case(b"websocket")))
}

async fn capture(State(state): State<CaptureState>, mut request: Request, next: Next) -> Response {
    let path = crate::management::percent_decode(request.uri().path());
    if !eligible(&request, &path) {
        return next.run(request).await;
    }
    let config = state.rt.config();
    let enabled = setting(&config, "request-log").and_then(serde_yaml_ng::Value::as_bool) == Some(true);
    let multipart = request.headers().get("content-type").is_some_and(|v| {
        String::from_utf8_lossy(trim(v.as_bytes()))
            .to_lowercase()
            .starts_with("multipart/form-data")
    });
    let length = request
        .headers()
        .get("content-length")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let eager = enabled || (!multipart && length.is_some_and(|n| n > 0 && n <= SMALL_BODY));
    let mut request_body = Vec::new();
    if eager {
        let body = std::mem::replace(request.body_mut(), Body::empty());
        let bytes = match axum::body::to_bytes(body, usize::MAX).await {
            Ok(bytes) => bytes,
            Err(error) => {
                // Preserve the transport error downstream; never turn it into
                // a successful empty body when capture cannot read the input.
                *request.body_mut() = Body::from_stream(futures_util::stream::once(async move {
                    Err::<Bytes, _>(io::Error::other(error.to_string()))
                }));
                return next.run(request).await;
            }
        };
        *request.body_mut() = Body::from(bytes.clone());
        request_body = decode(bytes.to_vec(), request.headers(), true, None).await.0;
    }
    let deferred = if !eager && !multipart && length != Some(0) && !request.body().is_end_stream() {
        Spool::new(&state.dir, "request-body").ok().map(|spool| {
            Arc::new(Mutex::new(Deferred {
                spool,
                length,
                read: 0,
                captured: 0,
                eof: false,
                truncated: false,
                failed: false,
            }))
        })
    } else {
        None
    };
    if let Some(deferred) = &deferred {
        let body = std::mem::replace(request.body_mut(), Body::empty());
        *request.body_mut() = Body::new(RequestBody {
            body,
            deferred: deferred.clone(),
        });
    }
    let query = mask_query(request.uri().query().unwrap_or_default());
    let url = if query.is_empty() {
        path
    } else {
        format!("{path}?{query}")
    };
    let id = request
        .extensions()
        .get::<RequestId>()
        .map(|id| id.0.clone())
        .unwrap_or_default();
    let sink = RequestLog(
        Arc::new(Mutex::new(Some(Record {
            state,
            enabled,
            url,
            method: request.method().to_string(),
            id,
            timestamp: Local::now(),
            headers: request.headers().clone(),
            request_body,
            deferred,
            status: 200,
            response_headers: HeaderMap::new(),
            response: Vec::new(),
            response_spool: None,
            streaming: false,
            streaming_failed: false,
            detached: false,
            metadata_ready: false,
            websocket_done: false,
            timeline: None,
            api_timeline: Section::new("api-websocket-timeline"),
            api_request: Section::new("api-request"),
            deferred_api_request: Vec::new(),
            api_response: Section::new("api-response"),
            api_timestamp: None,
            api_errors: Vec::new(),
            upstream: UpstreamCapture::default(),
            redact_sdp: false,
        }))),
        Arc::new(Finished::default()),
    );
    request.extensions_mut().insert(sink.clone());
    let mut guard = Completion(Some(sink.clone()));
    let head = request.method() == "HEAD";
    let response = CURRENT.scope(sink.clone(), next.run(request)).await;
    guard.0 = None;
    capture_response(sink, head, response).await
}

async fn capture_response(sink: RequestLog, head: bool, mut response: Response) -> Response {
    if response.extensions().get::<Captured>().is_some() {
        return response;
    }
    response.extensions_mut().insert(Captured);
    let mut guard = Completion(Some(sink.clone()));
    let (rt, enabled) = {
        let locked = sink.0.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(record) = locked.as_ref() else { return response };
        (record.state.rt.clone(), record.enabled)
    };
    let current_enabled = setting(&rt.config(), "request-log").and_then(serde_yaml_ng::Value::as_bool) == Some(true);
    if head && (current_enabled || (!enabled && response.status().as_u16() >= 400 && response.status().as_u16() != 499))
    {
        // Axum strips HEAD bodies after route middleware returns, so a lazy
        // response-body tee would never see these handler writes. Gin logs them
        // before net/http suppresses transport output; collect them here.
        if let Some(record) = sink.0.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
            record.status = response.status().as_u16();
            record.response_headers = response.headers().clone();
        }
        let body = std::mem::replace(response.body_mut(), Body::empty());
        if let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await {
            if let Some(record) = sink.0.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
                record.response.extend_from_slice(&bytes);
            }
            *response.body_mut() = Body::from(bytes);
        }
    }
    let mut locked = sink.0.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(record) = locked.as_mut() else {
        return response;
    };
    record.status = response.status().as_u16();
    record.response_headers = response.headers().clone();
    record.metadata_ready = true;
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    record.streaming = current_enabled
        && (content_type.contains("text/event-stream")
            || (content_type.trim().is_empty()
                && [b"\"stream\":true".as_slice(), b"\"stream\": true".as_slice()]
                    .iter()
                    .any(|needle| {
                        record
                            .request_body
                            .windows(needle.len())
                            .any(|window| window == *needle)
                    })));
    let chunks = if record.streaming && !head {
        match Spool::new(&record.state.dir, "response-body") {
            Ok(spool) => match spool.file.try_clone() {
                Ok(file) => {
                    record.response_spool = Some(spool);
                    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(100);
                    tokio::spawn(spool_response(file, rx, sink.clone()));
                    guard.0 = None;
                    Some(tx)
                }
                Err(_) => None,
            },
            Err(_) => None,
        }
    } else {
        None
    };
    // Only the error-only policy is fixed at entry; Go checks current logger
    // enablement on every write, including reloads during a lazy response body.
    let buffer = !enabled && record.status >= 400 && record.status != 499;
    let complete_now = record.detached && record.websocket_done;
    drop(locked);
    if complete_now {
        schedule(sink.clone(), false);
    }
    let (parts, body) = response.into_parts();
    Response::from_parts(
        parts,
        Body::new(ResponseBody {
            body,
            sink,
            guard,
            chunks,
            buffer,
        }),
    )
}

/// Cloneable per-request sink carried in HTTP extensions. Capture helpers must
/// mask upstream credentials before passing preformatted API sections, as in Go.
#[derive(Clone)]
pub struct RequestLog(Arc<Mutex<Option<Record>>>, Arc<Finished>);

#[derive(Default)]
struct Finished {
    done: AtomicBool,
    notify: tokio::sync::Notify,
}

impl RequestLog {
    /// Executors receive this through ExecRequest::capture(); WebSocket owners
    /// can retain it across connection tasks, independently of task-local state.
    pub fn capture_sink(&self) -> cpa_core::exec::CaptureSink {
        cpa_core::exec::CaptureSink::new(Arc::new(self.clone()))
    }

    /// Full middleware UUID (the filename uses its trailing eight characters).
    pub fn request_id(&self) -> String {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|r| r.id.clone())
            .unwrap_or_default()
    }

    /// Transfer finalization to the connection task before returning HTTP 101.
    /// Returns None for disabled logging, a second detach, or an already finalized
    /// request. This keeps disabled websocket logging free of disk/body capture.
    pub fn detach_websocket(&self) -> Option<WebsocketLog> {
        let mut locked = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let record = locked.as_mut()?;
        if !record.enabled || record.detached {
            return None;
        }
        record.detached = true;
        Some(WebsocketLog(Some(self.clone())))
    }

    /// Logs this request's bodies, downstream and upstream, without SDP ICE credentials
    /// (`cpa_exec::codex_live::redact_sdp`). The bytes on the wire are unchanged.
    pub fn redact_sdp(&self) {
        if let Some(record) = self.0.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
            record.redact_sdp = true;
        }
    }

    pub fn api_request(&self, bytes: &[u8]) {
        if let Some(record) = self.0.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
            record.api_request.append(&record.state.dir, bytes);
        }
    }

    pub fn api_response(&self, bytes: &[u8], timestamp: DateTime<Local>) {
        if let Some(record) = self.0.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
            record.api_response.append(&record.state.dir, bytes);
            record.api_timestamp.get_or_insert(timestamp);
        }
    }

    pub fn api_websocket_timeline(&self, bytes: &[u8]) {
        if let Some(record) = self.0.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
            record.api_timeline.append(&record.state.dir, bytes);
        }
    }

    /// Go API_RESPONSE_ERROR: actionable upstream failures can force an error
    /// log even when the downstream HTTP response started with 200.
    pub fn api_error(&self, status: u16, message: String) {
        if let Some(record) = self.0.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
            record.api_errors.push((status, message));
        }
    }

    async fn finish(&self, websocket: bool) {
        let record = {
            let mut record = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(record) = record.as_mut() {
                record.websocket_done |= websocket;
            }
            if record
                .as_ref()
                .is_some_and(|r| r.detached && !(r.metadata_ready && r.websocket_done))
            {
                None
            } else {
                record.take()
            }
        };
        if let Some(record) = record {
            record.finish().await;
            self.1.done.store(true, Ordering::Release);
            self.1.notify.notify_waiters();
        }
        if websocket {
            loop {
                let notified = self.1.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.1.done.load(Ordering::Acquire) {
                    break;
                }
                notified.await;
            }
        }
    }
}

#[derive(Default)]
struct UpstreamCapture {
    attempts: usize,
    deferred_requests: usize,
    deferred_bytes: usize,
    intro: bool,
    status: bool,
    headers: bool,
    body: bool,
    event: bool,
    error: bool,
}

fn commercial_mode(config: &cpa_core::config::Config) -> bool {
    config
        .document
        .get("server")
        .and_then(|s| s.get("commercial-mode"))
        .and_then(serde_yaml_ng::Value::as_bool)
        == Some(true)
}

fn request_log_on(config: &cpa_core::config::Config) -> bool {
    setting(config, "request-log").and_then(serde_yaml_ng::Value::as_bool) == Some(true)
}

impl cpa_core::exec::CaptureObserver for RequestLog {
    fn logs_responses(&self) -> bool {
        let locked = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        locked.as_ref().is_some_and(|record| {
            let config = record.state.rt.config();
            !commercial_mode(&config) && request_log_on(&config)
        })
    }

    fn record(&self, event: cpa_core::exec::CaptureEvent<'_>) {
        use cpa_core::exec::CaptureEvent::*;
        let mut locked = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(record) = locked.as_mut() else { return };
        let config = record.state.rt.config();
        if commercial_mode(&config) {
            return;
        }
        let enabled = request_log_on(&config);
        // Upstream bodies are redacted as they enter the log. The live call handler
        // records each body whole, as one event (bounded by its read limits), so an SDP
        // line never spans two events.
        let redacted;
        let event = match event {
            Request(info) if record.redact_sdp => {
                redacted = cpa_exec::codex_live::redact_sdp(info.body);
                Request(cpa_core::exec::UpstreamRequest {
                    body: &redacted,
                    ..info
                })
            }
            ResponseChunk(chunk) if record.redact_sdp => {
                redacted = cpa_exec::codex_live::redact_sdp(chunk);
                ResponseChunk(&redacted)
            }
            event => event,
        };
        match event {
            Request(info) => {
                let capture = &mut record.upstream;
                let (index, limit) = if enabled {
                    let index = capture.attempts + 1;
                    let deferred_requests = capture.deferred_requests;
                    let deferred_bytes = capture.deferred_bytes;
                    *capture = UpstreamCapture {
                        attempts: index,
                        deferred_requests,
                        deferred_bytes,
                        ..Default::default()
                    };
                    (index, info.body.len())
                } else {
                    capture.deferred_requests += 1;
                    let length = info
                        .body
                        .len()
                        .min((DEFERRED_BODY as usize).saturating_sub(capture.deferred_bytes));
                    capture.deferred_bytes += length;
                    (capture.deferred_requests, length)
                };
                let mut head = Vec::new();
                write!(
                    head,
                    "=== API REQUEST {index} ===\nTimestamp: {}\nUpstream URL: {}\n",
                    timestamp(Local::now()),
                    if info.url.is_empty() {
                        String::from("<unknown>")
                    } else {
                        logged_url(info.url)
                    }
                )
                .unwrap();
                if !info.method.is_empty() {
                    writeln!(head, "HTTP Method: {}", info.method).unwrap();
                }
                upstream_auth(&mut head, &info);
                head.extend_from_slice(b"\nHeaders:\n");
                upstream_headers(&mut head, info.headers);
                head.extend_from_slice(b"\nBody:\n");
                let body: &[u8] = if info.body.is_empty() {
                    b"<empty>"
                } else {
                    &info.body[..limit]
                };
                let mut tail = Vec::new();
                if limit < info.body.len() {
                    write!(tail, "\n[API REQUEST BODY TRUNCATED: captured first {limit} bytes]").unwrap();
                }
                tail.extend_from_slice(b"\n\n");
                // The body goes straight to the section, so a large one is not
                // copied into a second buffer first.
                for part in [&head[..], body, &tail[..]] {
                    if enabled {
                        record.api_request.append(&record.state.dir, part);
                    } else {
                        record.deferred_api_request.extend_from_slice(part);
                    }
                }
            }
            _ if !enabled => {}
            ResponseMetadata(status, headers) => {
                upstream_intro(record);
                let mut part = Vec::new();
                if status > 0 && !record.upstream.status {
                    writeln!(part, "Status: {status}").unwrap();
                    record.upstream.status = true;
                }
                if !record.upstream.headers {
                    part.extend_from_slice(b"Headers:\n");
                    upstream_headers(&mut part, headers);
                    part.push(b'\n');
                    record.upstream.headers = true;
                }
                record.api_response.append(&record.state.dir, &part);
            }
            ResponseError(error) => {
                upstream_intro(record);
                let mut part = Vec::new();
                if record.upstream.error {
                    part.push(b'\n');
                }
                writeln!(part, "Error: {error}").unwrap();
                record.api_response.append(&record.state.dir, &part);
                record.upstream.error = true;
            }
            ResponseChunk(chunk) => {
                let chunk = trim(chunk);
                if chunk.is_empty() {
                    return;
                }
                upstream_intro(record);
                let dir = &record.state.dir;
                if !record.upstream.headers {
                    record.api_response.append(dir, b"Headers:\n<none>\n\n");
                    record.upstream.headers = true;
                }
                if !record.upstream.body {
                    record.api_response.append(dir, b"Body:\n");
                } else {
                    record.api_response.append(
                        dir,
                        if record.upstream.event && chunk.starts_with(b"data:") {
                            b"\n"
                        } else {
                            b"\n\n"
                        },
                    );
                }
                record.api_response.append(dir, chunk);
                record.upstream.body = true;
                record.upstream.event = chunk.starts_with(b"event:");
            }
            WebsocketRequest(info) => {
                let mut part = websocket_event("api.websocket.request");
                if !info.url.is_empty() {
                    writeln!(part, "Upstream URL: {}", logged_url(info.url)).unwrap();
                }
                upstream_auth(&mut part, &info);
                part.extend_from_slice(b"Headers:\n");
                upstream_headers(&mut part, info.headers);
                part.extend_from_slice(b"\nBody:\n");
                part.extend_from_slice(if info.body.is_empty() { b"<empty>" } else { info.body });
                upstream_timeline(&mut record.api_timeline, &record.state.dir, &part);
            }
            WebsocketHandshake(status, headers) => {
                let mut part = websocket_event("api.websocket.handshake");
                if status > 0 {
                    writeln!(part, "Status: {status}").unwrap();
                }
                part.extend_from_slice(b"Headers:\n");
                upstream_headers(&mut part, headers);
                upstream_timeline(&mut record.api_timeline, &record.state.dir, &part);
            }
            WebsocketResponse(payload) => {
                let payload = trim(payload);
                if payload.is_empty() {
                    return;
                }
                record.api_timestamp.get_or_insert_with(Local::now);
                let mut part = websocket_event("api.websocket.response");
                part.extend_from_slice(payload);
                upstream_timeline(&mut record.api_timeline, &record.state.dir, &part);
            }
            WebsocketError { stage, error } => {
                record.api_timestamp.get_or_insert_with(Local::now);
                let mut part = websocket_event("api.websocket.error");
                let stage = crate::gojson::trim(stage);
                if !stage.is_empty() {
                    writeln!(part, "Stage: {stage}").unwrap();
                }
                writeln!(part, "Error: {error}").unwrap();
                upstream_timeline(&mut record.api_timeline, &record.state.dir, &part);
            }
        }
    }
}

fn upstream_intro(record: &mut Record) {
    if record.upstream.intro {
        return;
    }
    if record.upstream.attempts == 0 {
        record.upstream.attempts = 1;
        record
            .api_request
            .append(&record.state.dir, b"=== API REQUEST 1 ===\n<missing>\n\n");
    }
    let mut part = Vec::new();
    if !record.api_response.is_empty() {
        part.resize(2usize.saturating_sub(record.api_response.trailing), b'\n');
    }
    write!(
        part,
        "=== API RESPONSE {} ===\nTimestamp: {}\n\n",
        record.upstream.attempts,
        timestamp(Local::now())
    )
    .unwrap();
    record.api_response.append(&record.state.dir, &part);
    record.upstream.intro = true;
}

fn upstream_headers(out: &mut Vec<u8>, headers: &[(String, String)]) {
    if headers.is_empty() {
        out.extend_from_slice(b"<none>\n");
        return;
    }
    let mut headers: Vec<_> = headers.iter().collect();
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, value) in headers {
        write!(out, "{name}: ").unwrap();
        out.extend_from_slice(&masked_header(&name.to_ascii_lowercase(), value.as_bytes()));
        out.push(b'\n');
    }
}

fn upstream_auth(out: &mut Vec<u8>, info: &cpa_core::exec::UpstreamRequest<'_>) {
    let mut parts = Vec::new();
    for (label, value) in [
        ("provider", info.provider),
        ("auth_id", info.auth_id),
        ("label", info.auth_label),
    ] {
        let value = crate::gojson::trim(value);
        if !value.is_empty() {
            parts.push(format!("{label}={value}"));
        }
    }
    let kind = cpa_common::gostr::lower_bytes(crate::gojson::trim(info.auth_type).as_bytes());
    let value = crate::gojson::trim(info.auth_value);
    if !kind.is_empty() {
        let mut text = format!("type={kind}");
        if kind != "oauth" && !value.is_empty() {
            let value = if kind == "api_key" {
                String::from_utf8_lossy(&hide_key(value.as_bytes())).into_owned()
            } else {
                value.into()
            };
            text.push_str(&format!(" value={value}"));
        }
        parts.push(text);
    }
    if !parts.is_empty() {
        writeln!(out, "Auth: {}", parts.join(", ")).unwrap();
    }
}

fn websocket_event(event: &str) -> Vec<u8> {
    format!("Timestamp: {}\nEvent: {event}\n", timestamp(Local::now())).into_bytes()
}

fn upstream_timeline(out: &mut Section, dir: &Path, part: &[u8]) {
    if !out.is_empty() {
        out.append(dir, b"\n\n");
    }
    out.append(dir, trim(part));
}

/// Connection-owned timeline sink. Explicit `close().await` waits for the durable
/// log; dropping the sink also schedules finalization on client disconnect.
pub struct WebsocketLog(Option<RequestLog>);

impl WebsocketLog {
    pub fn append_part(&self, part: &[u8]) -> io::Result<()> {
        if trim(part).is_empty() {
            return Ok(());
        }
        if let Some(sink) = &self.0 {
            let mut record = sink.0.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(record) = record.as_mut() {
                if record.timeline.is_none() {
                    record.timeline = Some(Spool::new(&record.state.dir, "websocket-timeline")?);
                }
                let spool = record.timeline.as_mut().unwrap();
                if spool.file.metadata()?.len() > 0 {
                    spool.file.write_all(b"\n")?;
                }
                spool.file.write_all(part)?;
                if !part.ends_with(b"\n") {
                    spool.file.write_all(b"\n")?;
                }
            } else {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "request log finalized"));
            }
        }
        Ok(())
    }

    pub async fn close(mut self) {
        if let Some(sink) = self.0.take() {
            // Disconnect cancellation must not cancel the final delivery itself.
            let _ = tokio::spawn(async move { sink.finish(true).await }).await;
        }
    }
}

impl Drop for WebsocketLog {
    fn drop(&mut self) {
        if let Some(sink) = self.0.take() {
            schedule(sink, true);
        }
    }
}

fn schedule(sink: RequestLog, websocket: bool) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            sink.finish(websocket).await;
        });
    }
}

/// Drain even after a sticky disk error. Only EOF/drop ends capture; never
/// finalize a live HTTP stream merely because its log file could not be written.
async fn spool_response(file: File, mut rx: tokio::sync::mpsc::Receiver<Bytes>, sink: RequestLog) {
    let mut file = tokio::fs::File::from_std(file);
    let mut failed = false;
    while let Some(bytes) = rx.recv().await {
        if !failed && file.write_all(&bytes).await.is_err() {
            failed = true;
        }
    }
    failed |= file.flush().await.is_err();
    drop(file);
    if let Some(record) = sink.0.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
        record.streaming_failed = failed;
    }
    sink.finish(false).await;
}

struct Completion(Option<RequestLog>);
impl Drop for Completion {
    fn drop(&mut self) {
        if let Some(sink) = self.0.take() {
            if let Some(record) = sink.0.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
                // A cancelled handler has Go's default unwritten 200; release
                // any connection-close waiter even without response headers.
                record.metadata_ready = true;
            }
            schedule(sink, false);
        }
    }
}

struct ResponseBody {
    body: Body,
    sink: RequestLog,
    guard: Completion,
    chunks: Option<tokio::sync::mpsc::Sender<Bytes>>,
    buffer: bool,
}

impl HttpBody for ResponseBody {
    type Data = Bytes;
    type Error = axum::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, axum::Error>>> {
        let result = Pin::new(&mut self.body).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &result
            && let Some(bytes) = frame.data_ref()
        {
            if let Some(chunks) = &self.chunks {
                let _ = chunks.try_send(bytes.clone());
                if let Some(record) = self.sink.0.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
                    record.api_timestamp.get_or_insert_with(Local::now);
                }
            } else if let Some(record) = self.sink.0.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
                let config = record.state.rt.config();
                if self.buffer || setting(&config, "request-log").and_then(serde_yaml_ng::Value::as_bool) == Some(true)
                {
                    record.response.extend_from_slice(bytes);
                }
            }
        }
        if matches!(result, Poll::Ready(None | Some(Err(_)))) {
            self.chunks.take();
            if let Some(sink) = self.guard.0.take() {
                schedule(sink, false);
            }
        }
        result
    }
    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> hyper::body::SizeHint {
        self.body.size_hint()
    }
}

struct RequestBody {
    body: Body,
    deferred: Arc<Mutex<Deferred>>,
}
impl HttpBody for RequestBody {
    type Data = Bytes;
    type Error = axum::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, axum::Error>>> {
        let result = Pin::new(&mut self.body).poll_frame(cx);
        let mut deferred = self.deferred.lock().unwrap_or_else(PoisonError::into_inner);
        if let Poll::Ready(Some(Ok(frame))) = &result
            && let Some(bytes) = frame.data_ref()
        {
            deferred.read += bytes.len() as u64;
            let length = bytes
                .len()
                .min(DEFERRED_BODY.saturating_sub(deferred.captured) as usize);
            deferred.truncated |= length < bytes.len();
            if !deferred.failed {
                match deferred.spool.file.write(&bytes[..length]) {
                    Ok(n) => {
                        deferred.captured += n as u64;
                        deferred.failed = n < length;
                    }
                    Err(_) => deferred.failed = true,
                }
            }
        }
        if matches!(result, Poll::Ready(None)) {
            deferred.eof = true;
        }
        result
    }
    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> hyper::body::SizeHint {
        self.body.size_hint()
    }
}

struct Deferred {
    spool: Spool,
    length: Option<u64>,
    read: u64,
    captured: u64,
    eof: bool,
    truncated: bool,
    failed: bool,
}
impl Deferred {
    fn bytes(&self) -> io::Result<(Vec<u8>, String)> {
        if self.failed {
            return Err(io::Error::other("request body capture failed"));
        }
        let mut markers = Vec::new();
        if self.truncated {
            markers.push(format!(
                "[REQUEST BODY TRUNCATED: captured first {} bytes]",
                self.captured
            ));
        }
        if !(self.eof || self.length.is_some_and(|n| self.read >= n)) {
            markers.push(match self.length {
                Some(n) => format!("[REQUEST BODY CAPTURE INCOMPLETE: consumed {} of {n} bytes]", self.read),
                None => format!(
                    "[REQUEST BODY CAPTURE INCOMPLETE: consumed {} bytes from an unknown-length body]",
                    self.read
                ),
            });
        }
        Ok((std::fs::read(&self.spool.path)?, markers.join("\n")))
    }
}

struct Spool {
    file: File,
    path: PathBuf,
}
impl Spool {
    fn new(dir: &Path, prefix: &str) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{prefix}-{}.tmp", uuid::Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        // Go's os.OpenFile modes; Windows ignores them.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let file = options.open(&path)?;
        Ok(Self { file, path })
    }
}
impl Drop for Spool {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// In-memory bytes an upstream section keeps before it moves to its spool file.
const SECTION_MEMORY: usize = 64 << 10;
/// Long enough for every section header that `section` looks for.
const SECTION_HEAD: usize = 32;

/// An append-only upstream section of the log (API REQUEST, API RESPONSE or
/// API WEBSOCKET TIMELINE). Go appends these to file-backed sources
/// (request_logging.go attachRequestLogSources), so a long stream or WebSocket
/// session does not grow in memory. Here a section stays in memory until it
/// passes SECTION_MEMORY bytes, then the buffered bytes and everything after
/// them go to a spool file in the log directory. If the spool cannot be
/// written, the section keeps the rest in memory, as Go's writeAttemptResponse
/// falls back to a strings.Builder.
struct Section {
    prefix: &'static str,
    memory: Vec<u8>,
    spool: Option<Spool>,
    /// Bytes at the start of the spool file that belong to the section; a
    /// failed write can leave more after them, which are never read.
    spilled: u64,
    spill_failed: bool,
    /// The first SECTION_HEAD bytes of the section.
    head: Vec<u8>,
    /// Newlines at the end of the section.
    trailing: usize,
    /// Every byte so far is whitespace (the section trims to nothing).
    blank: bool,
}

impl Section {
    fn new(prefix: &'static str) -> Self {
        Self {
            prefix,
            memory: Vec::new(),
            spool: None,
            spilled: 0,
            spill_failed: false,
            head: Vec::new(),
            trailing: 0,
            blank: true,
        }
    }

    /// A section that holds `bytes` in memory, for a buffer that is already
    /// in memory as a whole.
    fn from_vec(prefix: &'static str, bytes: Vec<u8>) -> Self {
        let mut section = Self::new(prefix);
        section.track(&bytes);
        section.memory = bytes;
        section
    }

    fn is_empty(&self) -> bool {
        self.spilled == 0 && self.memory.is_empty()
    }

    fn track(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let room = SECTION_HEAD.saturating_sub(self.head.len());
        self.head.extend_from_slice(&bytes[..room.min(bytes.len())]);
        let newlines = bytes.iter().rev().take_while(|&&b| b == b'\n').count();
        self.trailing = if newlines == bytes.len() {
            self.trailing + newlines
        } else {
            newlines
        };
        self.blank &= trim(bytes).is_empty();
    }

    fn append(&mut self, dir: &Path, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.track(bytes);
        if !self.spill_failed && self.memory.len() + bytes.len() > SECTION_MEMORY {
            if self.spill(dir, bytes).is_ok() {
                return;
            }
            self.spill_failed = true;
        }
        self.memory.extend_from_slice(bytes);
    }

    /// Moves the buffered bytes and then `bytes` to the spool file. On error,
    /// whatever did not reach the file is still in memory or in `bytes`.
    fn spill(&mut self, dir: &Path, bytes: &[u8]) -> io::Result<()> {
        if self.spool.is_none() {
            self.spool = Some(Spool::new(dir, self.prefix)?);
        }
        let file = &mut self.spool.as_mut().unwrap().file;
        file.write_all(&self.memory)?;
        self.spilled += self.memory.len() as u64;
        // Appends only reach memory while it stays within SECTION_MEMORY, so
        // the kept allocation is bounded too.
        self.memory.clear();
        file.write_all(bytes)?;
        self.spilled += bytes.len() as u64;
        Ok(())
    }

    fn write_to(&self, out: &mut dyn Write) -> io::Result<()> {
        if let Some(spool) = self.spool.as_ref().filter(|_| self.spilled > 0) {
            let copied = io::copy(&mut File::open(&spool.path)?.take(self.spilled), out)?;
            if copied != self.spilled {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("{} spool is shorter than written", self.prefix),
                ));
            }
        }
        out.write_all(&self.memory)
    }
}

struct Record {
    state: CaptureState,
    enabled: bool,
    url: String,
    method: String,
    id: String,
    timestamp: DateTime<Local>,
    headers: HeaderMap,
    request_body: Vec<u8>,
    deferred: Option<Arc<Mutex<Deferred>>>,
    status: u16,
    response_headers: HeaderMap,
    response: Vec<u8>,
    response_spool: Option<Spool>,
    streaming: bool,
    streaming_failed: bool,
    detached: bool,
    metadata_ready: bool,
    websocket_done: bool,
    timeline: Option<Spool>,
    api_timeline: Section,
    api_request: Section,
    deferred_api_request: Vec<u8>,
    api_response: Section,
    api_timestamp: Option<DateTime<Local>>,
    api_errors: Vec<(u16, String)>,
    upstream: UpstreamCapture,
    /// Set by the live call handler: bodies are logged without SDP ICE credentials,
    /// upstream ones as they are recorded and downstream ones when the log is written.
    redact_sdp: bool,
}

impl Record {
    async fn finish(mut self) {
        if self.streaming_failed {
            return;
        }
        let config = self.state.rt.config();
        let enabled = setting(&config, "request-log").and_then(serde_yaml_ng::Value::as_bool) == Some(true);
        let error = self.api_errors.iter().any(|(status, message)| {
            let lower = message.to_lowercase();
            *status != 499 && !lower.contains("context canceled") && !lower.contains("client closed request")
        }) || (self.status >= 400 && self.status != 499);
        let forced = !self.enabled && error && !enabled;
        if !enabled && !forced {
            return;
        }
        if forced && self.api_request.is_empty() {
            self.api_request = Section::from_vec("api-request", std::mem::take(&mut self.deferred_api_request));
        }
        if let Some(deferred) = &self.deferred {
            let body = deferred.lock().unwrap_or_else(PoisonError::into_inner).bytes();
            if let Ok((body, marker)) = body {
                self.request_body = decode(body, &self.headers, true, Some(DEFERRED_BODY)).await.0;
                if !marker.is_empty() {
                    if !self.request_body.is_empty() && !self.request_body.ends_with(b"\n") {
                        self.request_body.push(b'\n');
                    }
                    self.request_body.extend_from_slice(marker.as_bytes());
                }
            }
        }
        let decompress_error = if !self.streaming {
            let (body, error) = decode(std::mem::take(&mut self.response), &self.response_headers, false, None).await;
            self.response = body;
            error
        } else {
            None
        };
        if self.redact_sdp {
            for body in [&mut self.request_body, &mut self.response] {
                if let std::borrow::Cow::Owned(redacted) = cpa_exec::codex_live::redact_sdp(body) {
                    *body = redacted;
                }
            }
        }
        if enabled && let Some(home) = self.state.rt.remote_dispatch() {
            if home.available() {
                let mut content = Vec::new();
                if self.format(&mut content, decompress_error).is_ok() {
                    let mut payload = serde_json::Map::new();
                    let mut headers: std::collections::BTreeMap<String, Vec<String>> = Default::default();
                    for (name, value) in &self.headers {
                        headers
                            .entry(header_name(name.as_str()))
                            .or_default()
                            .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
                    }
                    if !headers.is_empty() {
                        payload.insert("headers".into(), serde_json::to_value(headers).unwrap());
                    }
                    if !self.id.trim().is_empty() {
                        payload.insert("request_id".into(), self.id.trim().into());
                    }
                    if !content.is_empty() {
                        payload.insert(
                            "request_log".into(),
                            String::from_utf8_lossy(&content).into_owned().into(),
                        );
                    }
                    if let Err(error) = home
                        .request_log(crate::gojson::sorted(&payload.into()).into_bytes())
                        .await
                    {
                        tracing::warn!("failed to forward request log to home: {error}");
                    }
                }
            }
            return;
        }
        let max = setting(&config, "error-logs-max-files")
            .and_then(serde_yaml_ng::Value::as_i64)
            .unwrap_or(10);
        let max = if max < 0 { 10 } else { max };
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(error) = self.write(forced, max, decompress_error) {
                tracing::warn!("failed to write request log: {error}");
            }
        })
        .await;
    }

    fn write(self, forced: bool, max: i64, decompress_error: Option<String>) -> io::Result<()> {
        std::fs::create_dir_all(&self.state.dir)?;
        let filename = filename(&self.url, &self.id, Local::now(), forced);
        let mut file = unique_file(&self.state.dir, &filename)?;
        self.format(&mut file, decompress_error)?;
        drop(file);
        if forced {
            retain_errors(&self.state.dir, max)?;
        }
        Ok(())
    }

    fn format(&self, out: &mut dyn Write, decompress_error: Option<String>) -> io::Result<()> {
        let websocket = self
            .timeline
            .as_ref()
            .is_some_and(|s| s.file.metadata().is_ok_and(|m| m.len() > 0));
        let downstream = if websocket
            || self
                .headers
                .get("upgrade")
                .is_some_and(|v| trim(v.as_bytes()).eq_ignore_ascii_case(b"websocket"))
        {
            "websocket"
        } else {
            "http"
        };
        let http = !self.api_request.blank || !self.api_response.blank;
        let ws = !self.api_timeline.blank;
        let upstream = match (http, ws) {
            (true, true) => "websocket+http",
            (true, false) => "http",
            (false, true) => "websocket",
            _ => "",
        };
        write!(
            out,
            "=== REQUEST INFO ===\nVersion: {}\nURL: {}\nMethod: {}\nDownstream Transport: {downstream}\n",
            concat!("cliproxy-rs-", env!("CARGO_PKG_VERSION")),
            self.url,
            self.method
        )?;
        if !upstream.is_empty() {
            writeln!(out, "Upstream Transport: {upstream}")?;
        }
        write!(out, "Timestamp: {}\n\n\n=== HEADERS ===\n", timestamp(self.timestamp))?;
        headers(out, &self.headers, true)?;
        out.write_all(b"\n\n")?;
        if !websocket {
            out.write_all(b"=== REQUEST BODY ===\n")?;
            out.write_all(&self.request_body)?;
            spacing(out, &self.request_body)?;
        }
        if let Some(spool) = &self.timeline {
            out.write_all(b"=== WEBSOCKET TIMELINE ===\n")?;
            let mut input = File::open(&spool.path)?;
            let mut buffer = [0; 8192];
            let mut trailing = 0;
            loop {
                let n = input.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                out.write_all(&buffer[..n])?;
                let count = buffer[..n].iter().rev().take_while(|&&b| b == b'\n').count();
                trailing = if count == n { trailing + count } else { count };
            }
            for _ in trailing..3 {
                out.write_all(b"\n")?;
            }
        }
        section(out, "API WEBSOCKET TIMELINE", &self.api_timeline, None)?;
        section(out, "API REQUEST", &self.api_request, None)?;
        if !self.streaming {
            for (status, error) in &self.api_errors {
                write!(out, "=== API ERROR RESPONSE ===\nHTTP Status: {status}\n{error}")?;
                spacing(out, error.as_bytes())?;
            }
        }
        section(out, "API RESPONSE", &self.api_response, self.api_timestamp)?;
        if websocket {
            return Ok(());
        }
        write!(out, "=== RESPONSE ===\nStatus: {}\n", self.status)?;
        headers(out, &self.response_headers, false)?;
        if let Some(spool) = &self.response_spool
            && self.redact_sdp
        {
            // A live call's response is at most `live::MAX_BODY`; redacting it whole
            // also covers a marker split across written chunks.
            let body = std::fs::read(&spool.path)?;
            let body = cpa_exec::codex_live::redact_sdp(&body);
            if !(body.starts_with(b"\n") || body.starts_with(b"\r\n")) {
                out.write_all(b"\n")?;
            }
            out.write_all(&body)?;
        } else if let Some(spool) = &self.response_spool {
            let mut input = File::open(&spool.path)?;
            let mut first = [0; 2];
            let n = input.read(&mut first)?;
            if !(first[..n].starts_with(b"\n") || first[..n].starts_with(b"\r\n")) {
                out.write_all(b"\n")?;
            }
            out.write_all(&first[..n])?;
            io::copy(&mut input, out)?;
        } else {
            if !(self.response.starts_with(b"\n") || self.response.starts_with(b"\r\n")) {
                out.write_all(b"\n")?;
            }
            out.write_all(&self.response)?;
            if let Some(error) = decompress_error {
                write!(out, "\n[DECOMPRESSION ERROR: {error}]")?;
            }
        }
        if !self.streaming {
            out.write_all(b"\n")?;
        }
        Ok(())
    }
}

fn trim(bytes: &[u8]) -> &[u8] {
    cpa_core::config::go_trim_space(bytes)
}
pub(crate) fn timestamp(at: DateTime<Local>) -> String {
    let text = at.to_rfc3339_opts(SecondsFormat::Nanos, true);
    let zone = &text[29..];
    let fraction = text[20..29].trim_end_matches('0');
    if fraction.is_empty() {
        format!("{}{zone}", &text[..19])
    } else {
        format!("{}.{fraction}{zone}", &text[..19])
    }
}

fn header_name(name: &str) -> String {
    name.split('-')
        .map(|part| {
            let mut bytes = part.as_bytes().to_owned();
            if let Some(first) = bytes.first_mut() {
                first.make_ascii_uppercase();
            }
            String::from_utf8(bytes).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// Written in place of a cookie header value or URL userinfo.
const REDACTED: &str = "<redacted>";

/// Deliberate difference: Go logs Cookie and Set-Cookie values unchanged
/// (util.MaskSensitiveHeaderValue). A session cookie is a credential, so the
/// whole value of any header whose lowercase name contains "cookie" is
/// replaced, on both the masked and the unmasked header sections.
fn cookie(name: &str) -> bool {
    name.contains("cookie")
}

/// Deliberate difference: Go prints upstream URLs unchanged, and a configured
/// base-url can carry credentials. The logged copy replaces any userinfo and
/// masks sensitive query values the way the downstream URL is masked; the
/// request itself still goes to the URL as configured.
fn logged_url(url: &str) -> String {
    let mut out = String::new();
    let mut rest = url;
    if let Some(i) = url.find("://")
        && i > 0
        && url[..i]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    {
        out.push_str(&url[..i + 3]);
        rest = &url[i + 3..];
        // Look for '@' up to the first '/', even past a '?' or '#': a
        // password with those characters unescaped still counts as userinfo,
        // and dropping too much is safer than printing a secret.
        let authority = &rest[..rest.find('/').unwrap_or(rest.len())];
        if let Some(at) = authority.rfind('@') {
            out.push_str(REDACTED);
            out.push('@');
            rest = &rest[at + 1..];
        }
    }
    let (rest, fragment) = rest.split_at(rest.find('#').unwrap_or(rest.len()));
    match rest.split_once('?') {
        Some((base, query)) => {
            out.push_str(base);
            out.push('?');
            out.push_str(&mask_query(query));
        }
        None => out.push_str(rest),
    }
    out.push_str(fragment);
    out
}

fn masked_header(name: &str, value: &[u8]) -> Vec<u8> {
    if cookie(name) {
        return REDACTED.as_bytes().to_vec();
    }
    if name.contains("authorization") {
        let trimmed = trim(value);
        if let Some(i) = trimmed.iter().position(|b| *b == b' ') {
            return [&trimmed[..i + 1], &hide_key(&trimmed[i + 1..])].concat();
        }
        return hide_key(value);
    }
    if ["api-key", "apikey", "token", "secret"]
        .iter()
        .any(|key| name.contains(key))
    {
        hide_key(value)
    } else {
        value.to_vec()
    }
}

fn headers(out: &mut dyn Write, headers: &HeaderMap, mask: bool) -> io::Result<()> {
    for (name, value) in headers {
        write!(out, "{}: ", header_name(name.as_str()))?;
        if mask {
            out.write_all(&masked_header(name.as_str(), value.as_bytes()))?;
        } else if cookie(name.as_str()) {
            out.write_all(REDACTED.as_bytes())?;
        } else {
            out.write_all(value.as_bytes())?;
        }
        out.write_all(b"\n")?;
    }
    Ok(())
}

fn spacing(out: &mut dyn Write, bytes: &[u8]) -> io::Result<()> {
    pad(
        out,
        if bytes.is_empty() {
            1
        } else {
            bytes.iter().rev().take_while(|&&b| b == b'\n').count()
        },
    )
}

fn pad(out: &mut dyn Write, trailing: usize) -> io::Result<()> {
    for _ in trailing..3 {
        out.write_all(b"\n")?;
    }
    Ok(())
}

fn section(out: &mut dyn Write, name: &str, bytes: &Section, at: Option<DateTime<Local>>) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    if !bytes.head.starts_with(format!("=== {name}").as_bytes()) {
        writeln!(out, "=== {name} ===")?;
        if let Some(at) = at {
            writeln!(out, "Timestamp: {}", timestamp(at))?;
        }
    }
    bytes.write_to(out)?;
    pad(out, bytes.trailing)
}

fn filename(url: &str, id: &str, at: DateTime<Local>, error: bool) -> String {
    let path = url
        .split('?')
        .next()
        .unwrap_or_default()
        .strip_prefix('/')
        .unwrap_or(url.split('?').next().unwrap_or_default());
    let mut sanitized = String::new();
    for ch in path.chars() {
        let ch = if matches!(
            ch,
            '/' | ':' | '<' | '>' | '"' | '|' | '?' | '*' | ' ' | '\t' | '\n' | '\r' | '\x0c'
        ) {
            '-'
        } else {
            ch
        };
        if ch != '-' || !sanitized.ends_with('-') {
            sanitized.push(ch);
        }
    }
    let path = sanitized.trim_matches('-');
    let path = if path.is_empty() { "root" } else { path };
    let id = if id.is_empty() {
        NEXT_ID.fetch_add(1, Ordering::Relaxed).wrapping_add(1).to_string()
    } else {
        id[id.len().saturating_sub(8)..].to_owned()
    };
    format!(
        "{}{path}-{}-{id}.log",
        if error { "error-" } else { "" },
        at.format("%Y-%m-%dT%H%M%S")
    )
}

fn unique_file(dir: &Path, filename: &str) -> io::Result<File> {
    let (prefix, id) = filename
        .trim_end_matches(".log")
        .rsplit_once('-')
        .unwrap_or((filename, ""));
    for index in 0..=1000 {
        let name = if index == 0 {
            filename.to_owned()
        } else {
            format!("{prefix}_{index}-{id}.log")
        };
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o644);
        match options.open(dir.join(name)) {
            Ok(file) => return Ok(file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::other(format!(
        "too many conflicting log files for {filename}"
    )))
}

fn retain_errors(dir: &Path, max: i64) -> io::Result<()> {
    if max <= 0 {
        return Ok(());
    }
    let mut files = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("error-") || !name.ends_with(".log") {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            if metadata.is_dir() {
                return None;
            }
            Some((metadata.modified().ok(), entry.path()))
        })
        .collect::<Vec<_>>();
    files.sort_by_key(|file| std::cmp::Reverse(file.0));
    for (_, path) in files.into_iter().skip(max as usize) {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

async fn decode(raw: Vec<u8>, headers: &HeaderMap, request: bool, limit: Option<u64>) -> (Vec<u8>, Option<String>) {
    use async_compression::tokio::bufread::{BrotliDecoder, DeflateDecoder, GzipDecoder, ZstdDecoder};
    let encoding = headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let encoding = if request { encoding.trim() } else { encoding };
    if raw.is_empty() || encoding.is_empty() {
        return (raw, None);
    }
    // Go's response logger accepts one exact encoding, not stacked encodings.
    if !request
        && !matches!(
            encoding.to_ascii_lowercase().as_str(),
            "gzip" | "deflate" | "br" | "zstd"
        )
    {
        return (raw, None);
    }
    let mut body = raw.clone();
    for encoding in encoding.split(',').rev().map(|s| s.trim().to_ascii_lowercase()) {
        let mut reader: Pin<Box<dyn AsyncRead + Send + '_>> = match encoding.as_str() {
            "" | "identity" => continue,
            "zstd" => {
                let mut decoder = ZstdDecoder::new(body.as_slice());
                decoder.multiple_members(true);
                Box::pin(decoder)
            }
            "gzip" if !request => {
                let mut decoder = GzipDecoder::new(body.as_slice());
                decoder.multiple_members(true);
                Box::pin(decoder)
            }
            "deflate" if !request => Box::pin(DeflateDecoder::new(body.as_slice())),
            "br" if !request => Box::pin(BrotliDecoder::new(body.as_slice())),
            _ => return (raw, None),
        };
        let mut output = Vec::new();
        let result = if let Some(limit) = limit {
            reader.as_mut().take(limit + 1).read_to_end(&mut output).await
        } else {
            reader.read_to_end(&mut output).await
        };
        drop(reader);
        if let Err(error) = result {
            // ponytail: M4-0254 accepted partial: malformed Brotli can withhold
            // partial decoded bytes that Go accepts, and codec diagnostic text
            // differs. Keep the complete raw fallback; port decoder edge behavior
            // and Go's error classifier only if exact corrupt-stream logs are needed.
            return (
                raw,
                (!request).then(|| format!("failed to decompress {encoding} data: {error}")),
            );
        }
        if limit.is_some_and(|limit| output.len() as u64 > limit) {
            output.truncate(limit.unwrap() as usize);
            if !output.ends_with(b"\n") {
                output.push(b'\n');
            }
            output.extend_from_slice(b"[DECOMPRESSED REQUEST BODY TRUNCATED]");
            return (output, None);
        }
        body = output;
    }
    (body, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::response::IntoResponse;
    use tower_service::Service;

    /// `capture_go.json`, recorded with the Go generator's `buildinfo.Version` pinned to
    /// `cliproxy-rs-0.1.0` (tests/reference/capture/main.go). The version of this build
    /// replaces it, so a release bump needs no re-recording.
    /// A file every write to fails, on any OS (Linux-only `/dev/full` is not needed).
    fn unwritable(dir: &Path) -> File {
        let path = dir.join("unwritable");
        File::create(&path).unwrap();
        File::options().read(true).open(path).unwrap()
    }

    fn capture_go() -> serde_json::Value {
        let text = include_str!("../tests/fixtures/capture_go.json")
            .replace("cliproxy-rs-0.1.0", concat!("cliproxy-rs-", env!("CARGO_PKG_VERSION")));
        serde_json::from_str(&text).unwrap()
    }

    fn management(dir: &Path, enabled: bool, commercial: bool) -> Arc<Management> {
        let config = cpa_core::config::Config::parse(&format!(
            "observability: {{logs: {{request-log: {enabled}}}}}\nserver: {{commercial-mode: {commercial}}}\n"
        ))
        .unwrap();
        let rt = Arc::new(crate::testing::runtime(
            config,
            Vec::new(),
            cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        Management::with_options(
            rt,
            dir.join("config.yaml"),
            crate::management::Options {
                log_dir: Some(dir.to_owned()),
                management_password: Some(String::new()),
                ..Default::default()
            },
        )
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cpa-request-log-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn canonical(raw: &str) -> String {
        let mut lines = raw.split('\n').map(str::to_owned).collect::<Vec<_>>();
        for i in 0..lines.len() {
            if lines[i].starts_with("Timestamp: ") {
                lines[i] = "Timestamp: <time>".into();
            }
            if lines[i] == "=== HEADERS ===" || lines[i] == "=== RESPONSE ===" {
                let mut start = i + 1;
                if lines.get(start).is_some_and(|s| s.starts_with("Status: ")) {
                    start += 1;
                }
                let end = (start..lines.len())
                    .find(|&j| lines[j].is_empty())
                    .unwrap_or(lines.len());
                lines[start..end].sort();
            }
        }
        lines.join("\n")
    }

    fn contents(section: &Section) -> Vec<u8> {
        let mut out = Vec::new();
        section.write_to(&mut out).unwrap();
        out
    }

    fn logs(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "log"))
            .collect()
    }

    #[tokio::test]
    async fn upstream_contract_matches_real_go() {
        use cpa_core::exec::{CaptureEvent::*, UpstreamRequest};
        assert!(!cpa_core::exec::CaptureSink::default().enabled());
        assert!(!cpa_core::exec::CaptureSink::default().logs_responses());
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/upstream_capture_go.json")).unwrap();
        for case in fixture.as_array().unwrap() {
            let case = case.clone();
            let dir = scratch();
            let state = management(&dir, case["kind"] != "disabled" && case["kind"] != "reload", false);
            let rt = state.rt.clone();
            let app = axum::Router::new().fallback(move |axum::Extension(log): axum::Extension<RequestLog>| {
                let case = case.clone();
                let rt = rt.clone();
                async move {
                    let sink = log.capture_sink();
                    let headers = [
                        ("Authorization".into(), "Bearer 1234567890".into()),
                        ("X-Token".into(), "abcdefghi".into()),
                        ("Z-Header".into(), "first".into()),
                        ("Z-Header".into(), "second".into()),
                    ];
                    let response_headers = [("X-Secret".into(), "1234567890".into())];
                    let info = |oauth, empty| UpstreamRequest {
                        url: "https://fixture.invalid/private",
                        method: "POST",
                        headers: &headers,
                        body: if empty { b"" } else { b"request body" },
                        provider: " codex ",
                        auth_id: " fixture-id ",
                        auth_label: " fixture-label ",
                        auth_type: if oauth { "oauth" } else { " API_KEY " },
                        auth_value: if oauth { "never-print-oauth" } else { " 1234567890 " },
                    };
                    let kind = case["kind"].as_str().unwrap();
                    // Executors skip buffering response-only log data on this signal.
                    let logging = !matches!(kind, "disabled" | "reload");
                    assert_eq!(sink.logs_responses(), logging, "{kind}: responses logged");
                    if !matches!(kind, "missing" | "websocket") {
                        sink.record(Request(info(kind == "oauth", false)));
                    }
                    if kind == "websocket" {
                        sink.record(WebsocketRequest(info(false, false)));
                        sink.record(WebsocketHandshake(101, &response_headers));
                        sink.record(WebsocketResponse(b"  {\"type\":\"response.created\"} \n"));
                        sink.record(WebsocketError {
                            stage: " read ",
                            error: "fixture disconnect",
                        });
                    } else {
                        if kind == "reload" {
                            rt.publish_config(
                                cpa_core::config::Config::parse("observability: {logs: {request-log: true}}\n")
                                    .unwrap(),
                            );
                            assert!(sink.logs_responses(), "reload: responses logged");
                        }
                        sink.record(ResponseMetadata(201, &response_headers));
                        sink.record(ResponseMetadata(202, &[("Ignored".into(), "yes".into())]));
                        sink.record(ResponseChunk(b"  event: first\n"));
                        sink.record(ResponseChunk(b"data: {\"a\":1}\n"));
                        sink.record(ResponseChunk(b"\n data: second \n"));
                        sink.record(ResponseError("fixture error"));
                        sink.record(ResponseError("second error"));
                        if kind == "http" {
                            sink.record(Request(info(true, true)));
                            sink.record(ResponseError("failed before response"));
                        }
                    }
                    let locked = log.0.lock().unwrap();
                    let record = locked.as_ref().unwrap();
                    for (name, bytes) in [
                        (
                            "request",
                            if kind == "disabled" {
                                record.deferred_api_request.clone()
                            } else {
                                contents(&record.api_request)
                            },
                        ),
                        ("response", contents(&record.api_response)),
                        ("timeline", contents(&record.api_timeline)),
                    ] {
                        assert_eq!(
                            canonical(std::str::from_utf8(&bytes).unwrap()).trim_end_matches('\n'),
                            case[name].as_str().unwrap().trim_end_matches('\n'),
                            "{kind} {name}"
                        );
                        assert!(!String::from_utf8_lossy(&bytes).contains("never-print-oauth"));
                    }
                    "ok"
                }
            });
            let mut app = router(&state, app);
            let response = app
                .call(
                    HttpRequest::builder()
                        .method("POST")
                        .uri("/fixture")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    struct HomeLogs {
        available: bool,
        logs: Mutex<Vec<Vec<u8>>>,
        gate: Option<Arc<(tokio::sync::Notify, tokio::sync::Notify)>>,
    }
    impl crate::remote::RemoteDispatch for HomeLogs {
        fn available(&self) -> bool {
            self.available
        }
        fn dispatch(
            &self,
            _: crate::remote::RemoteRequest,
        ) -> futures_util::future::BoxFuture<'_, Result<crate::remote::RemoteGrant, crate::remote::RemoteError>>
        {
            Box::pin(async { panic!("no dispatch") })
        }
        fn models(
            &self,
            _: Vec<(String, String)>,
            _: Vec<(String, String)>,
        ) -> futures_util::future::BoxFuture<'_, Result<Vec<u8>, crate::remote::ModelsError>> {
            Box::pin(async { Err(crate::remote::ModelsError::Unavailable) })
        }
        fn request_log(&self, payload: Vec<u8>) -> futures_util::future::BoxFuture<'_, Result<(), String>> {
            Box::pin(async move {
                if let Some(gate) = &self.gate {
                    gate.0.notify_one();
                    gate.1.notified().await;
                }
                self.logs.lock().unwrap().push(payload);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn home_forwards_full_logs_but_forced_errors_remain_local() {
        for (enabled, available) in [(true, true), (true, false), (false, true)] {
            let dir = scratch();
            let state = management(&dir, enabled, false);
            let home = Arc::new(HomeLogs {
                available,
                logs: Mutex::new(Vec::new()),
                gate: None,
            });
            state.rt.set_remote_dispatch(Some(home.clone()));
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let app = axum::Router::new().fallback(move |axum::Extension(log): axum::Extension<RequestLog>| {
                let _ = tx.send(log);
                async { (StatusCode::INTERNAL_SERVER_ERROR, "fixture error") }
            });
            let mut app = router(&state, app);
            let mut request = HttpRequest::builder()
                .method("POST")
                .uri("/fixture")
                .header("authorization", "Bearer 1234567890")
                .body(Body::empty())
                .unwrap();
            request.extensions_mut().insert(RequestId("fixture-request-id".into()));
            let response = app.call(request).await.unwrap();
            axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            // The handler ran inside `app.call`, so its log is already queued.
            let log = rx.try_recv().expect("handler sent its RequestLog");
            // The log is finalized in a spawned task after the body ends: forwarded to
            // Home, or written on the blocking pool. Wait for that task, not a fixed time,
            // so the counts below are final.
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
            while !log.1.done.load(Ordering::Acquire) {
                assert!(tokio::time::Instant::now() < deadline, "request log never finalized");
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let captured = home.logs.lock().unwrap();
            assert_eq!(captured.len(), usize::from(enabled && available));
            assert_eq!(logs(&dir).len(), usize::from(!enabled));
            if let Some(payload) = captured.first() {
                let payload: serde_json::Value = serde_json::from_slice(payload).unwrap();
                assert_eq!(payload["request_id"], "fixture-request-id");
                assert_eq!(payload["headers"]["Authorization"][0], "Bearer 1234567890");
                let content = payload["request_log"].as_str().unwrap();
                assert!(content.contains("Authorization: Bearer 1234...7890"));
                assert!(content.contains(
                    "=== RESPONSE ===\nStatus: 500\nContent-Type: text/plain; charset=utf-8\n\nfixture error\n"
                ));
            }
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[tokio::test]
    async fn served_composition_captures_before_head_stripping_and_length_inference() {
        let dir = scratch();
        let state = management(&dir, true, false);
        let rest = axum::Router::new().route(
            "/fixture",
            axum::routing::post(|| async { ([("content-type", "application/json")], "outbound\n") }).head(|| async {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [("content-length", "9")],
                    "outbound\n",
                )
            }),
        );
        let mut app = router(
            &state,
            crate::app(state.rt.clone(), rest).layer(axum::middleware::from_fn(crate::management::cors)),
        );
        for method in ["POST", "HEAD"] {
            let response = app
                .call(
                    HttpRequest::builder()
                        .method(method)
                        .uri("/fixture")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert_eq!(body, if method == "HEAD" { "" } else { "outbound\n" });
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let contents: Vec<_> = logs(&dir).iter().map(|p| std::fs::read_to_string(p).unwrap()).collect();
            if contents.len() == 2 && contents.iter().all(|s| s.ends_with("outbound\n\n")) {
                for content in contents {
                    assert!(content.contains("Access-Control-Allow-Origin: *"));
                    assert_eq!(content.contains("Content-Length: 9"), content.contains("Method: HEAD"));
                }
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "missing pre-framing capture: {contents:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn cancelling_websocket_close_does_not_cancel_final_delivery() {
        let dir = scratch();
        let state = management(&dir, true, false);
        let gate = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
        let home = Arc::new(HomeLogs {
            available: true,
            logs: Mutex::new(Vec::new()),
            gate: Some(gate.clone()),
        });
        state.rt.set_remote_dispatch(Some(home.clone()));
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let app = axum::Router::new().fallback(move |axum::Extension(log): axum::Extension<RequestLog>| {
            let tx = tx.clone();
            async move {
                tx.send((log.clone(), log.detach_websocket().unwrap())).await.unwrap();
                StatusCode::SWITCHING_PROTOCOLS
            }
        });
        let mut app = router(&state, app);
        let response = app
            .call(
                HttpRequest::builder()
                    .uri("/v1/responses")
                    .header("upgrade", "websocket")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        drop(response);
        let (log, socket) = rx.recv().await.unwrap();
        socket
            .append_part(b"Timestamp: fixture\nEvent: websocket.disconnect\nbye\n")
            .unwrap();
        let caller = tokio::spawn(socket.close());
        gate.0.notified().await;
        caller.abort();
        gate.1.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !log.1.done.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(home.logs.lock().unwrap().len(), 1);
        assert!(logs(&dir).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn go_middleware_goldens() {
        let fixture: serde_json::Value = capture_go();
        let date = regex::Regex::new(r"\d{4}-\d\d-\d\dT\d{6}").unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let dir = scratch();
            let state = management(&dir, case["enabled"].as_bool().unwrap(), false);
            let status = StatusCode::from_u16(case["status"].as_u64().unwrap() as u16).unwrap();
            let consume = case["consume"].as_i64().unwrap();
            let streaming = case["stream"].as_bool().unwrap();
            let app = axum::Router::new().fallback(move |request: Request| async move {
                if consume < 0 {
                    let _ = axum::body::to_bytes(request.into_body(), usize::MAX).await.unwrap();
                } else {
                    use futures_util::StreamExt;
                    let mut body = request.into_body().into_data_stream();
                    assert_eq!(body.next().await.unwrap().unwrap().len(), consume as usize);
                    // Do not drain the unread rest, just like the Go fixture.
                }
                let mut response = Response::new(Body::from("outbound\n"));
                *response.status_mut() = status;
                response.headers_mut().insert(
                    "content-type",
                    if streaming {
                        "text/event-stream"
                    } else {
                        "application/json"
                    }
                    .parse()
                    .unwrap(),
                );
                response
            });
            let mut app = router(&state, app);
            let length = case["body_length"].as_u64().unwrap() as usize;
            let body = if consume >= 0 {
                Body::from_stream(futures_util::stream::iter([
                    Ok::<_, io::Error>(Bytes::from(vec![b'x'; consume as usize])),
                    Ok(Bytes::from(vec![b'x'; length - consume as usize])),
                ]))
            } else {
                Body::from(case["body"].as_str().unwrap().to_owned())
            };
            let mut request = HttpRequest::builder()
                .method(case["method"].as_str().unwrap())
                .uri(case["path"].as_str().unwrap())
                .header("authorization", "Bearer 1234567890")
                .header("content-length", length.to_string())
                .body(body)
                .unwrap();
            request.extensions_mut().insert(RequestId("0198-aaaa-a1b2c3d4".into()));
            let response = app.call(request).await.unwrap();
            assert_eq!(response.status(), status);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert_eq!(body, if case["method"] == "HEAD" { "" } else { "outbound\n" });
            let expected = case["content"].as_str().unwrap();
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let files = logs(&dir);
                let actual = files
                    .first()
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    .unwrap_or_default();
                if canonical(&actual) == expected {
                    break;
                }
                assert!(tokio::time::Instant::now() < deadline, "case {case}: got {actual}");
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            // Wait for scheduled cleanup too, so discarded error-only capture
            // cannot leave files behind and an unexpected later write fails.
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            let files = logs(&dir);
            assert_eq!(files.len(), usize::from(!expected.is_empty()));
            if let Some(file) = files.first() {
                assert_eq!(
                    date.replace_all(file.file_name().unwrap().to_str().unwrap(), "<date>"),
                    case["filename"].as_str().unwrap()
                );
            }
            assert!(
                std::fs::read_dir(&dir)
                    .unwrap()
                    .flatten()
                    .all(|e| e.path().extension().is_none_or(|e| e != "tmp"))
            );
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[tokio::test]
    async fn websocket_sink_stays_open_after_upgrade_and_matches_go() {
        use base64::Engine;
        let fixture: serde_json::Value = capture_go();
        let dir = scratch();
        let state = management(&dir, true, false);
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let app = axum::Router::new().fallback(move |axum::Extension(log): axum::Extension<RequestLog>| {
            let tx = tx.clone();
            async move {
                assert_eq!(log.request_id(), "0198-aaaa-a1b2c3d4");
                tx.send(log.detach_websocket().unwrap()).await.unwrap();
                StatusCode::SWITCHING_PROTOCOLS.into_response()
            }
        });
        let mut app = router(&state, app);
        let mut request = HttpRequest::builder()
            .uri("/v1/responses")
            .header("upgrade", "websocket")
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(RequestId("0198-aaaa-a1b2c3d4".into()));
        let response = app.call(request).await.unwrap();
        drop(response);
        let sink = rx.recv().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(logs(&dir).is_empty(), "HTTP 101 must not flush the transcript");
        for part in fixture["websocket"]["parts"].as_array().unwrap() {
            sink.append_part(
                &base64::engine::general_purpose::STANDARD
                    .decode(part.as_str().unwrap())
                    .unwrap(),
            )
            .unwrap();
        }
        sink.close().await;
        let files = logs(&dir);
        assert_eq!(files.len(), 1);
        let content = std::fs::read_to_string(&files[0]).unwrap();
        assert_eq!(canonical(&content), fixture["websocket"]["content"]);
        assert!(!content.contains("REQUEST BODY") && !content.contains("=== RESPONSE ==="));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn go_masking_collisions_and_retention() {
        let fixture: serde_json::Value = capture_go();
        for case in fixture["masks"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap().to_ascii_lowercase();
            let value = case["value"].as_str().unwrap();
            let mut want = case["out"].as_str().unwrap().to_owned();
            // Deliberate difference: Go leaves a masked two-byte value in the clear.
            if value == "ab" && masked_header(&name, b"abc") != b"abc" {
                want = "...".into();
            }
            // Deliberate difference: Go logs cookie values unchanged.
            if cookie(&name) {
                want = REDACTED.into();
            }
            assert_eq!(String::from_utf8(masked_header(&name, value.as_bytes())).unwrap(), want);
        }
        let dir = scratch();
        let name = "error-v1-responses-2026-10-03T040506-a1b2c3d4.log";
        unique_file(&dir, name).unwrap().write_all(b"original").unwrap();
        unique_file(&dir, name).unwrap().write_all(b"second").unwrap();
        assert_eq!(std::fs::read(dir.join(name)).unwrap(), b"original");
        let second = dir.join("error-v1-responses-2026-10-03T040506_1-a1b2c3d4.log");
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
        File::options()
            .write(true)
            .open(&second)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();
        std::fs::write(dir.join("main.log"), b"protected").unwrap();
        retain_errors(&dir, 0).unwrap();
        assert_eq!(logs(&dir).len(), 3);
        retain_errors(&dir, 1).unwrap();
        assert!(!second.exists());
        assert!(dir.join(name).exists() && dir.join("main.log").exists());
        let at = DateTime::parse_from_rfc3339("2026-10-03T04:05:06.12001Z")
            .unwrap()
            .with_timezone(&Local);
        assert!(timestamp(at).contains(".12001"));
        assert_eq!(
            filename("/v1//x:  y?secret=not-in-name", "0198-aaaa-a1b2c3d4", at, true),
            format!("error-v1-x-y-{}-a1b2c3d4.log", at.format("%Y-%m-%dT%H%M%S"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn commercial_mode_disables_only_request_capture_and_is_startup_only() {
        for commercial in [false, true] {
            let dir = scratch();
            let state = management(&dir, true, commercial);
            let app = axum::Router::new().fallback(move |request: Request| async move {
                assert_eq!(request.extensions().get::<RequestLog>().is_none(), commercial);
                StatusCode::BAD_REQUEST
            });
            let mut app = router(&state, app);
            let new = cpa_core::config::Config::parse(&format!(
                "server: {{commercial-mode: {}}}\nobservability: {{logs: {{request-log: true}}}}\n",
                !commercial
            ))
            .unwrap();
            state.rt.publish_config(new);
            let response = app
                .call(
                    HttpRequest::builder()
                        .method("POST")
                        .uri("/v1/responses")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            drop(response);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert_eq!(logs(&dir).len(), usize::from(!commercial));
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[tokio::test]
    async fn capture_preserves_transport_errors_and_classifies_cancellation() {
        let dir = scratch();
        let state = management(&dir, true, false);
        let app = axum::Router::new().fallback(|request: Request| async move {
            assert!(axum::body::to_bytes(request.into_body(), usize::MAX).await.is_err());
            StatusCode::BAD_REQUEST
        });
        let mut app = router(&state, app);
        let body = Body::from_stream(futures_util::stream::iter([
            Ok(Bytes::from_static(b"partial")),
            Err(io::Error::other("broken transport")),
        ]));
        let response = app
            .call(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        drop(response);
        assert!(logs(&dir).is_empty(), "capture failure bypasses logging as in Go");
        std::fs::remove_dir_all(dir).unwrap();

        for (status, error, expected) in [
            (500, "context CANCELED", false),
            (500, "client closed request", false),
            (499, "gone", false),
            (502, "upstream failure", true),
        ] {
            let dir = scratch();
            let state = management(&dir, false, false);
            let app =
                axum::Router::new().fallback(move |axum::Extension(log): axum::Extension<RequestLog>| async move {
                    assert!(
                        log.detach_websocket().is_none(),
                        "disabled capture has no timeline work"
                    );
                    log.api_error(status, error.into());
                    StatusCode::OK
                });
            let mut app = router(&state, app);
            let response = app
                .call(
                    HttpRequest::builder()
                        .method("POST")
                        .uri("/v1/responses")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            drop(response);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert_eq!(logs(&dir).len(), usize::from(expected));
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[tokio::test]
    async fn detached_close_waits_for_http_metadata_and_drop_finalizes_once() {
        for cancel in [false, true] {
            let dir = scratch();
            let state = management(&dir, true, false);
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
            let app = axum::Router::new().fallback(move |axum::Extension(log): axum::Extension<RequestLog>| {
                let tx = tx.clone();
                async move {
                    let sink = log.detach_websocket().unwrap();
                    assert!(log.detach_websocket().is_none(), "only one connection owner");
                    if cancel {
                        drop(sink);
                        tx.send(None).await.unwrap();
                        std::future::pending::<()>().await;
                    } else {
                        let close = tokio::spawn(sink.close());
                        tokio::task::yield_now().await;
                        assert!(!close.is_finished(), "must wait for HTTP status");
                        tx.send(Some(close)).await.unwrap();
                    }
                    StatusCode::BAD_REQUEST
                }
            });
            let mut app = router(&state, app);
            let request = HttpRequest::builder()
                .method("POST")
                .uri("/v1/responses")
                .body(Body::empty())
                .unwrap();
            if cancel {
                let mut response = Box::pin(app.call(request));
                assert!(futures_util::poll!(&mut response).is_pending());
                rx.recv().await.unwrap();
                drop(response);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            } else {
                let response = app.call(request).await.unwrap();
                let close = rx.recv().await.unwrap().unwrap();
                tokio::time::timeout(std::time::Duration::from_secs(5), close)
                    .await
                    .unwrap()
                    .unwrap();
                drop(response);
            }
            let files = logs(&dir);
            assert_eq!(files.len(), 1);
            let content = std::fs::read_to_string(&files[0]).unwrap();
            assert!(content.contains(if cancel { "Status: 200\n" } else { "Status: 400\n" }));
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[tokio::test]
    async fn deferred_body_boundaries_are_transparent_and_disk_errors_are_sticky() {
        for size in [32 << 20, (32 << 20) + 1] {
            let dir = scratch();
            let capture = Arc::new(Mutex::new(Deferred {
                spool: Spool::new(&dir, "request-body").unwrap(),
                length: None,
                read: 0,
                captured: 0,
                eof: false,
                truncated: false,
                failed: false,
            }));
            let body = Body::new(RequestBody {
                body: Body::from(vec![b'x'; size]),
                deferred: capture.clone(),
            });
            assert_eq!(axum::body::to_bytes(body, usize::MAX).await.unwrap().len(), size);
            let (bytes, marker) = capture.lock().unwrap().bytes().unwrap();
            assert_eq!(bytes.len(), 32 << 20);
            assert_eq!(
                marker,
                if size == 32 << 20 {
                    ""
                } else {
                    "[REQUEST BODY TRUNCATED: captured first 33554432 bytes]"
                }
            );
            drop(capture);
            assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
            std::fs::remove_dir_all(dir).unwrap();
        }
        let dir = scratch();
        let mut spool = Spool::new(&dir, "request-body").unwrap();
        spool.file = unwritable(&dir);
        let capture = Arc::new(Mutex::new(Deferred {
            spool,
            length: None,
            read: 0,
            captured: 0,
            eof: false,
            truncated: false,
            failed: false,
        }));
        let body = Body::new(RequestBody {
            body: Body::from("still forwarded"),
            deferred: capture.clone(),
        });
        assert_eq!(axum::body::to_bytes(body, usize::MAX).await.unwrap(), "still forwarded");
        assert!(capture.lock().unwrap().bytes().is_err());
        assert_eq!(capture.lock().unwrap().captured, 0);
        drop(capture);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn response_disk_failure_does_not_finalize_early_and_queue_is_nonblocking() {
        let dir = scratch();
        let state = management(&dir, true, false);
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let app = axum::Router::new().fallback(move |axum::Extension(log): axum::Extension<RequestLog>| {
            let sender = sender.clone();
            async move {
                sender.send(log).await.unwrap();
                Body::from_stream(futures_util::stream::pending::<io::Result<Bytes>>())
            }
        });
        let mut app = router(&state, app);
        let response = app
            .call(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let sink = receiver.recv().await.unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(100);
        let body = ResponseBody {
            body: Body::from_stream(futures_util::stream::iter(
                (0..102).map(|_| Ok::<_, io::Error>(Bytes::from_static(b"x"))),
            )),
            sink: sink.clone(),
            guard: Completion(None),
            chunks: Some(tx.clone()),
            buffer: false,
        };
        let forwarded = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            axum::body::to_bytes(Body::new(body), usize::MAX),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(forwarded.len(), 102, "stalled logger must not delay/drop client output");
        assert_eq!(rx.len(), 100, "logger alone drops chunks above its queue limit");
        let file = unwritable(&dir);
        let worker = tokio::spawn(spool_response(file, rx, sink.clone()));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!worker.is_finished(), "disk failure must wait for HTTP completion");
        assert!(sink.0.lock().unwrap().is_some());
        assert!(logs(&dir).is_empty());
        drop(tx);
        worker.await.unwrap();
        assert!(sink.0.lock().unwrap().is_none());
        assert!(
            logs(&dir).is_empty(),
            "failed stream is not published as a complete log"
        );
        drop(response);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn hot_enable_captures_only_frames_written_while_enabled_like_go() {
        use futures_util::StreamExt;
        let fixture: serde_json::Value = capture_go();
        for case in fixture["reloads"].as_array().unwrap() {
            let dir = scratch();
            let state = management(&dir, false, false);
            let app = axum::Router::new().fallback(|| async {
                Response::builder()
                    .header("content-type", "application/json")
                    .body(Body::from_stream(futures_util::stream::iter([
                        Ok::<_, io::Error>(Bytes::from_static(b"first\n")),
                        Ok(Bytes::from_static(b"second\n")),
                    ])))
                    .unwrap()
            });
            let mut app = router(&state, app);
            let response = app
                .call(
                    HttpRequest::builder()
                        .method("POST")
                        .uri("/v1/responses")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let enable = || {
                state.rt.publish_config(
                    cpa_core::config::Config::parse("observability: {logs: {request-log: true}}\n").unwrap(),
                )
            };
            if case["before_first"] == true {
                enable();
            }
            let mut body = response.into_body().into_data_stream();
            assert_eq!(body.next().await.unwrap().unwrap(), "first\n");
            enable();
            assert_eq!(body.next().await.unwrap().unwrap(), "second\n");
            assert!(body.next().await.is_none());
            let expected = case["content"].as_str().unwrap();
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let actual = logs(&dir)
                    .first()
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    .unwrap_or_default();
                if canonical(&actual) == expected {
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "got {actual}, expected {expected}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[tokio::test]
    async fn compression_body_bytes_match_real_go_including_raw_error_fallback() {
        use base64::Engine;
        let fixture: serde_json::Value = capture_go();
        for case in fixture["brotli_prefixes"].as_array().unwrap() {
            let input = base64::engine::general_purpose::STANDARD
                .decode(case["input"].as_str().unwrap())
                .unwrap();
            let mut headers = HeaderMap::new();
            headers.insert("content-encoding", "br".parse().unwrap());
            let (output, error) = decode(input.clone(), &headers, false, None).await;
            if error.is_some() {
                assert_eq!(output, input, "never discard bytes on codec failure");
            } else {
                assert!(!case["error"].as_bool().unwrap());
                assert_eq!(
                    output,
                    base64::engine::general_purpose::STANDARD
                        .decode(case["output"].as_str().unwrap())
                        .unwrap()
                );
            }
        }
        for case in fixture["compression"].as_array().unwrap() {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(case["input"].as_str().unwrap())
                .unwrap();
            let log = base64::engine::general_purpose::STANDARD
                .decode(case["raw"].as_str().unwrap())
                .unwrap();
            let section = b"=== RESPONSE ===\n";
            let start = log.windows(section.len()).position(|b| b == section).unwrap() + section.len();
            let start = start + log[start..].windows(2).position(|b| b == b"\n\n").unwrap() + 2;
            let logged_body = &log[start..log.len() - 1];
            let mut headers = HeaderMap::new();
            headers.insert("content-encoding", case["encoding"].as_str().unwrap().parse().unwrap());
            let (body, error) = decode(raw.clone(), &headers, false, None).await;
            if logged_body
                .windows(b"\n[DECOMPRESSION ERROR: ".len())
                .any(|bytes| bytes == b"\n[DECOMPRESSION ERROR: ")
            {
                assert_eq!(body, raw);
                assert!(logged_body.starts_with(&body));
                assert!(logged_body[body.len()..].starts_with(b"\n[DECOMPRESSION ERROR: "));
                assert!(error.is_some());
                // Exact malformed-codec diagnostic wording remains an explicit
                // parity gap; raw bytes and annotation presence are verified here.
            } else if case["invalid"] == true && case["encoding"] == "br" {
                // Accepted corrupt-Brotli difference: Go permits partial output;
                // Rust reports failure and preserves the complete raw input.
                assert_eq!(body, raw);
                assert!(error.is_some());
            } else {
                assert_eq!(body, logged_body);
                assert!(error.is_none());
            }
            if case["encoding"] == "zstd" {
                let (request_body, error) = decode(raw.clone(), &headers, true, None).await;
                assert_eq!(request_body, if case["invalid"] == true { raw } else { body });
                assert!(error.is_none(), "Go silently retains undecodable inbound bodies");
            }
        }
    }

    /// Serves one request (sending a Cookie) whose handler feeds `events` to the
    /// capture sink and answers `status` with a Set-Cookie, then returns the
    /// persisted log file.
    async fn persisted(
        enabled: bool,
        status: StatusCode,
        events: impl Fn(&RequestLog) + Clone + Send + Sync + 'static,
    ) -> Vec<u8> {
        let dir = scratch();
        let state = management(&dir, enabled, false);
        let app = axum::Router::new().fallback(move |axum::Extension(log): axum::Extension<RequestLog>| {
            let events = events.clone();
            async move {
                events(&log);
                (status, [("set-cookie", "down=c00kie-down-set")], "done")
            }
        });
        let mut app = router(&state, app);
        let request = HttpRequest::builder()
            .method("POST")
            .uri("/fixture")
            .header("cookie", "client=c00kie-client")
            .body(Body::empty())
            .unwrap();
        let response = app.call(request).await.unwrap();
        axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let mut content = Vec::new();
        for _ in 0..500 {
            if let [log] = &logs(&dir)[..] {
                content = std::fs::read(log).unwrap();
                if content.trim_ascii_end().ends_with(b"\ndone") {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(content.trim_ascii_end().ends_with(b"\ndone"), "no complete log");
        drop(app);
        drop(state);
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|e| e == "tmp"))
            .collect();
        assert!(leftovers.is_empty(), "spool files left behind: {leftovers:?}");
        std::fs::remove_dir_all(dir).unwrap();
        content
    }

    fn upstream_request<'a>(
        url: &'a str,
        headers: &'a [(String, String)],
        body: &'a [u8],
    ) -> cpa_core::exec::UpstreamRequest<'a> {
        cpa_core::exec::UpstreamRequest {
            url,
            method: "POST",
            headers,
            body,
            provider: "codex",
            auth_id: "",
            auth_label: "",
            auth_type: "",
            auth_value: "",
        }
    }

    #[tokio::test]
    async fn cookie_values_never_reach_persisted_logs() {
        use cpa_core::exec::CaptureEvent::*;
        // Request logging on, and off with a forced error log.
        for enabled in [true, false] {
            let content = persisted(enabled, StatusCode::BAD_GATEWAY, |log| {
                let sink = log.capture_sink();
                let headers = [
                    ("Cookie".into(), "up=c00kie-up".into()),
                    ("Content-Type".into(), "application/json".into()),
                ];
                sink.record(Request(upstream_request("https://fixture.invalid/v1", &headers, b"{}")));
                sink.record(ResponseMetadata(
                    502,
                    &[("Set-Cookie".into(), "up-set=c00kie-up-set".into())],
                ));
                sink.record(ResponseChunk(b"upstream failed"));
            })
            .await;
            let content = String::from_utf8(content).unwrap();
            assert!(!content.contains("c00kie-"), "{content}");
            // Downstream request and upstream request.
            assert_eq!(content.matches("\nCookie: <redacted>\n").count(), 2, "{content}");
            // Downstream response, and the upstream response when it is captured.
            assert_eq!(
                content.matches("\nSet-Cookie: <redacted>\n").count(),
                if enabled { 2 } else { 1 },
                "{content}"
            );
            assert!(content.contains("\nContent-Type: application/json\n"));
        }
    }

    #[tokio::test]
    async fn upstream_url_credentials_are_redacted_in_logs() {
        use cpa_core::exec::CaptureEvent::*;
        let content = persisted(true, StatusCode::OK, |log| {
            let sink = log.capture_sink();
            sink.record(Request(upstream_request(
                "https://alice:pw-h1dden@proxy.example/v1/models?alt=sse&key=AIza-h1dden-query-123&access_token=tok-x-h1dden-x-456#frag",
                &[],
                b"",
            )));
            sink.record(WebsocketRequest(upstream_request(
                "wss://bob:pw2-h1dden@ws.example/realtime?client_secret=cs-xx-h1dden-xx-789&model=gpt",
                &[],
                b"",
            )));
        })
        .await;
        let content = String::from_utf8(content).unwrap();
        assert!(!content.contains("h1dden"), "{content}");
        assert!(!content.contains("alice") && !content.contains("bob:"), "{content}");
        let line = |prefix: &str| {
            content
                .lines()
                .find(|l| l.starts_with(prefix))
                .unwrap_or_else(|| panic!("no {prefix} in {content}"))
                .to_owned()
        };
        let http = line("Upstream URL: https://<redacted>@proxy.example/v1/models?alt=sse&key=");
        assert!(http.contains("&access_token=") && http.ends_with("#frag"), "{http}");
        let ws = line("Upstream URL: wss://<redacted>@ws.example/realtime?client_secret=");
        assert!(ws.ends_with("&model=gpt"), "{ws}");

        for url in [
            "https://host.example/v1/users/a@b.example?alt=sse",
            "http://127.0.0.1:8080/v1#a@b",
            "https://host.example/v1?model=gpt",
        ] {
            assert_eq!(logged_url(url), url);
        }
        assert_eq!(
            logged_url("https://u:p@host.example"),
            "https://<redacted>@host.example"
        );
        assert_eq!(
            logged_url("https://user@host.example/x"),
            "https://<redacted>@host.example/x"
        );
    }

    #[tokio::test]
    async fn large_upstream_sections_stay_bounded_in_memory() {
        use cpa_core::exec::CaptureEvent::*;
        let body = vec![b'q'; 20 << 20];
        let chunk: Vec<u8> = (0..64 << 10).map(|i| b'a' + (i % 26) as u8).collect();
        const CHUNKS: usize = 640;
        let events = {
            let body = body.clone();
            let chunk = chunk.clone();
            move |log: &RequestLog| {
                let sink = log.capture_sink();
                sink.record(Request(upstream_request("https://fixture.invalid/v1", &[], &body)));
                sink.record(ResponseMetadata(200, &[]));
                for _ in 0..CHUNKS {
                    sink.record(ResponseChunk(&chunk));
                }
                let locked = log.0.lock().unwrap();
                let record = locked.as_ref().unwrap();
                for section in [&record.api_request, &record.api_response] {
                    assert!(section.spool.is_some());
                    assert!(
                        section.memory.capacity() <= 2 * SECTION_MEMORY,
                        "{}",
                        section.memory.capacity()
                    );
                }
            }
        };
        let content = persisted(true, StatusCode::OK, events).await;
        let find = |from: usize, needle: &[u8]| {
            from + content[from..]
                .windows(needle.len())
                .position(|w| w == needle)
                .unwrap_or_else(|| panic!("missing {}", String::from_utf8_lossy(needle)))
        };
        let expect = |at: usize, want: &[u8]| {
            assert!(content.len() >= at + want.len() && &content[at..at + want.len()] == want);
        };
        let request = find(0, b"=== API REQUEST 1 ===\n");
        let start = find(request, b"\nBody:\n") + b"\nBody:\n".len();
        expect(start, &[&body[..], b"\n\n\n=== API RESPONSE 1 ===\n"].concat());
        let response = find(start + body.len(), b"=== API RESPONSE 1 ===\n");
        let start = find(response, b"\nBody:\n") + b"\nBody:\n".len();
        let mut stream = vec![chunk.clone(); CHUNKS].join(&b"\n\n"[..]);
        stream.extend_from_slice(b"\n\n\n=== RESPONSE ===\nStatus: 200\n");
        expect(start, &stream);
    }
}
