//! Codex upstream Responses WebSocket (codex_websockets_executor.go, _connection.go,
//! _session.go, _stream.go, _errors.go).
//!
//! One upstream socket per downstream session, keyed by credential, URL and proxy. A
//! single reader task per socket enforces the 5-minute idle deadline (pongs are answered
//! while reading) and hands frames to the turn in progress through a bounded channel;
//! frames that arrive between turns are dropped, as in Go. Turns on one session are
//! serialised. The socket stays pooled after a completed response so the next turn can
//! continue with `previous_response_id` and incremental input.
//!
//! Lifecycle: the reader task ends with the socket; [`Pool::close`] (downstream gone)
//! aborts it and sends a close frame. No task outlives its session.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use bytes::Bytes;
use cpa_core::exec::{
    ExecError, ExecRequest, ExecResponse, ExecSession, ExecStream, FailureScope, ResponseBody, SessionLease,
};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use gjson::Kind;
use http::HeaderMap;
use tokio::sync::{OwnedMutexGuard, mpsc, watch};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};

use deflate::Inflate;

/// The upstream socket: tungstenite over the upgraded connection, with permessage-deflate
/// reads when negotiated.
pub(crate) type WebSocket = WebSocketStream<Inflate<wreq::Upgraded>>;

use crate::codex::CodexExecutor;
use crate::codex_json::{set_raw, set_str};
use crate::codex_quota::{self, QuotaSignals};
use crate::codex_request::{self as request, Call, Settings, View};
use crate::codex_response::{self as response, OutputItems};

/// `codexResponsesWebsocketIdleTimeout`: read deadline renewed before every read.
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// `codexResponsesWebsocketHandshakeTO`.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on one upstream frame write (docs/DIFFERENCES-FROM-GO.md). Go sets no write
/// deadline, so a write into a half-open socket held the turn until the 300 s read
/// deadline. A large frame gets as long as it takes at `rate` bytes per second.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WriteLimit {
    pub(crate) floor: Duration,
    pub(crate) rate: usize,
}

// ponytail: a fixed 128 KiB/s floor rate (about 1 Mbit/s); a slower uplink sending a
// multi-megabyte frame times out. Make it configurable if such a link shows up.
pub(crate) const WRITE_LIMIT: WriteLimit = WriteLimit {
    floor: Duration::from_secs(10),
    rate: 128 << 10,
};

impl WriteLimit {
    fn deadline(self, len: usize) -> Duration {
        self.floor
            .max(Duration::from_secs(len.div_ceil(self.rate.max(1)) as u64))
    }
}
/// Go's per-turn read channel capacity.
const TURN_BUFFER: usize = 4096;
/// Bound on a handshake rejection body.
const MAX_HANDSHAKE_BODY: usize = 64 * 1024;
/// Largest upstream frame or message read.
// ponytail: Go sets no read limit; 64 MiB (the HTTP body cap) keeps one socket's memory
// bounded while fitting image-bearing output items.
const MAX_UPSTREAM_MESSAGE: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) credential: String,
    pub(crate) url: String,
    pub(crate) proxy: crate::proxy::Proxy,
}

pub(crate) enum Read {
    Text(String),
    /// The socket failed; the reader has already invalidated it.
    Failed(ExecError),
}

/// The reader's hand-off to the turn in progress. One lock covers both fields, so a turn
/// that activates after the reader exited sees why instead of waiting forever.
#[derive(Default)]
pub(crate) struct Link {
    /// The turn currently reading, if any.
    active: Option<mpsc::Sender<Read>>,
    /// Why the reader stopped (`upstreamDisconnectError`); set once when it exits.
    pub(crate) lost: Option<ExecError>,
}

pub(crate) struct Upstream {
    pub(crate) target: Target,
    pub(crate) sink: tokio::sync::Mutex<SplitSink<WebSocket, Message>>,
    pub(crate) link: Mutex<Link>,
    pub(crate) reader: Mutex<Option<tokio::task::AbortHandle>>,
    /// The last request that touched the `collaboration` namespace on this socket renamed
    /// it (`multiAgentV2OptimizedConn`): later continuations restore upstream names too.
    multi_agent: std::sync::atomic::AtomicBool,
    /// The Home pick this socket keeps (Go `session.lifecycle`); `true` once the socket
    /// shut down, after which no pick binds.
    lease: Mutex<(bool, Option<SessionLease>)>,
    write: WriteLimit,
}

impl Upstream {
    /// A socket for `target`, not yet published to a session and without a reader.
    pub(crate) fn new(target: Target, sink: SplitSink<WebSocket, Message>, write: WriteLimit) -> Arc<Self> {
        Arc::new(Self {
            target,
            write,
            sink: tokio::sync::Mutex::new(sink),
            link: Mutex::default(),
            reader: Mutex::default(),
            multi_agent: Default::default(),
            lease: Mutex::default(),
        })
    }

    /// `!conflict && (optimized || isMultiAgentV2Optimized(conn))`.
    fn restores(&self, optimized: bool, conflict: bool) -> bool {
        !conflict && (optimized || self.multi_agent.load(std::sync::atomic::Ordering::Acquire))
    }

    /// `setMultiAgentV2Optimized` after a request that optimized or conflicted.
    fn note_multi_agent(&self, optimized: bool, conflict: bool) {
        if optimized || conflict {
            self.multi_agent
                .store(optimized && !conflict, std::sync::atomic::Ordering::Release);
        }
    }

    pub(crate) fn activate(&self) -> mpsc::Receiver<Read> {
        let (tx, rx) = mpsc::channel(TURN_BUFFER);
        let mut link = self.link.lock().expect("link");
        match &link.lost {
            Some(error) => {
                let _ = tx.try_send(Read::Failed(turn_error(error)));
            }
            None => link.active = Some(tx),
        }
        rx
    }

    pub(crate) fn deactivate(&self) {
        self.link.lock().expect("link").active.take();
    }

    /// `writeCodexWebsocketMessage` + `mapCodexWebsocketWriteError`: a write after the
    /// upstream closed with 1009 reports the request-scoped 413 instead. A write that
    /// outlasts [`WriteLimit`] fails like any other write: the caller invalidates the
    /// socket, so a partly written frame is never followed by another. Cost: one timer
    /// entry while a write is in flight.
    pub(crate) async fn send(&self, frame: String) -> Result<(), ExecError> {
        let deadline = self.write.deadline(frame.len());
        let sent = tokio::time::timeout(deadline, async {
            self.sink.lock().await.send(Message::text(frame)).await
        })
        .await;
        match sent {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(write_error(
                self.link.lock().expect("link").lost.as_ref(),
                "codex websockets executor: write failed",
            )),
            Err(_) => Err(write_error(
                self.link.lock().expect("link").lost.as_ref(),
                "codex websockets executor: write timed out",
            )),
        }
    }

    /// Go `bindExecutionLifecycle`: the socket keeps the attempt's Home pick until it is
    /// invalidated, replaced or closed, and Home draining the pick invalidates the
    /// socket. A pick bound earlier ends (`target_replaced`).
    pub(crate) fn bind(
        self: &Arc<Self>,
        session: &Arc<Session>,
        lease: Option<&SessionLease>,
    ) -> Result<(), ExecError> {
        let Some(lease) = lease else { return Ok(()) };
        if self
            .lease
            .lock()
            .expect("lease")
            .1
            .as_ref()
            .is_some_and(|l| l.same(lease))
        {
            return Ok(());
        }
        let (weak_session, weak_conn) = (Arc::downgrade(session), Arc::downgrade(self));
        let close = Box::new(move || {
            if let (Some(session), Some(conn)) = (weak_session.upgrade(), weak_conn.upgrade()) {
                session.invalidate(
                    &conn,
                    &transport("codex websockets executor: execution lifecycle ended"),
                    false,
                );
            }
        });
        let unbound = || transport("codex websockets executor: websocket connection closed during lifecycle bind");
        if !lease.0.retain(close) {
            session.invalidate(self, &unbound(), false);
            return Err(unbound());
        }
        let previous = {
            let mut slot = self.lease.lock().expect("lease");
            if slot.0 {
                drop(slot);
                lease.0.end();
                return Err(unbound());
            }
            slot.1.replace(lease.clone())
        };
        if let Some(previous) = previous {
            previous.0.end();
        }
        Ok(())
    }

    /// Stops the reader and closes the socket, ending the Home pick it kept. Idempotent.
    pub(crate) fn shutdown(self: &Arc<Self>) {
        if let Some(reader) = self.reader.lock().expect("reader handle").take() {
            reader.abort();
        }
        self.deactivate();
        let lease = {
            let mut slot = self.lease.lock().expect("lease");
            slot.0 = true;
            slot.1.take()
        };
        if let Some(lease) = lease {
            lease.0.end();
        }
        let this = self.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(1), async {
                let mut sink = this.sink.lock().await;
                let _ = sink.send(Message::Close(None)).await;
                let _ = sink.close().await;
            })
            .await;
        });
    }
}

pub(crate) struct Session {
    /// Serialises turns (`reqMu`).
    pub(crate) turn: Arc<tokio::sync::Mutex<()>>,
    pub(crate) conn: Mutex<Option<Arc<Upstream>>>,
    /// Set once when the session's socket is lost (`notifyUpstreamDisconnect`).
    closed: watch::Sender<Option<ExecError>>,
    /// The downstream connection's client frames when response steering is configured.
    steering: Mutex<Option<Arc<SteeringInput>>>,
}

impl Session {
    pub(crate) fn current(&self) -> Option<Arc<Upstream>> {
        self.conn.lock().expect("session conn").clone()
    }

    fn steering(&self) -> Option<Arc<SteeringInput>> {
        self.steering.lock().expect("session steering").clone()
    }

    /// `invalidateUpstreamConn`: only the session's current socket is dropped, so a stale
    /// reader cannot tear down its replacement. `notify` tells the downstream handler.
    pub(crate) fn invalidate(&self, conn: &Arc<Upstream>, error: &ExecError, notify: bool) {
        {
            let mut current = self.conn.lock().expect("session conn");
            if !current.as_ref().is_some_and(|c| Arc::ptr_eq(c, conn)) {
                return;
            }
            *current = None;
        }
        if notify {
            self.closed.send_if_modified(|slot| {
                let first = slot.is_none();
                if first {
                    *slot = Some(error.clone());
                }
                first
            });
        }
        conn.shutdown();
    }
}

/// Upstream sockets by downstream session id.
pub(crate) struct Pool {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// Read deadline for each upstream application message.
    pub(crate) idle: Duration,
    pub(crate) write: WriteLimit,
}

impl Default for Pool {
    fn default() -> Self {
        Self {
            sessions: Mutex::default(),
            idle: IDLE_TIMEOUT,
            write: WRITE_LIMIT,
        }
    }
}

impl Pool {
    #[cfg(test)]
    pub fn with_idle(idle: Duration) -> Self {
        Self {
            idle,
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub(crate) fn with_write(write: WriteLimit) -> Self {
        Self {
            write,
            ..Self::default()
        }
    }

    pub(crate) fn session(&self, id: &str) -> Arc<Session> {
        self.sessions
            .lock()
            .expect("sessions")
            .entry(id.to_owned())
            .or_insert_with(|| {
                Arc::new(Session {
                    turn: Arc::default(),
                    conn: Mutex::default(),
                    closed: watch::channel(None).0,
                    steering: Mutex::default(),
                })
            })
            .clone()
    }

    /// `WithWebsocketInput`: binds the downstream connection's frames to its session.
    pub fn attach_steering(&self, id: &str, input: SteeringInput) {
        *self.session(id).steering.lock().expect("session steering") = Some(Arc::new(input));
    }

    /// `CloseExecutionSession`: the downstream connection ended.
    pub fn close(&self, id: &str) {
        let session = self.sessions.lock().expect("sessions").remove(id);
        if let Some(conn) = session.and_then(|s| s.conn.lock().expect("session conn").take()) {
            conn.shutdown();
        }
    }

    /// The error that lost the session's upstream socket, if it was lost.
    pub fn loss(&self, id: &str) -> Option<ExecError> {
        let session = self.sessions.lock().expect("sessions").get(id).cloned()?;
        session.closed.borrow().clone()
    }

    /// Resolves with the error that lost the session's upstream socket.
    pub fn closed(&self, id: &str) -> impl std::future::Future<Output = ExecError> + Send + 'static {
        let mut rx = self.session(id).closed.subscribe();
        async move {
            loop {
                if let Some(error) = rx.borrow_and_update().clone() {
                    return error;
                }
                if rx.changed().await.is_err() {
                    return std::future::pending().await;
                }
            }
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.sessions.lock().expect("sessions").len()
    }
}

fn transport(message: &str) -> ExecError {
    ExecError::local(502, FailureScope::Transport, message)
}

/// gorilla/websocket `CloseError.Error()`: what Go returns for an upstream close.
fn close_error_text(code: u16, text: &str) -> String {
    let name = match code {
        1000 => " (normal)",
        1001 => " (going away)",
        1002 => " (protocol error)",
        1003 => " (unsupported data)",
        1005 => " (no status)",
        1006 => " (abnormal closure)",
        1007 => " (invalid payload data)",
        1008 => " (policy violation)",
        1009 => " (message too big)",
        1010 => " (mandatory extension missing)",
        1011 => " (internal server error)",
        1015 => " (TLS handshake error)",
        _ => "",
    };
    let mut out = format!("websocket: close {code}{name}");
    if !text.is_empty() {
        out.push_str(": ");
        out.push_str(text);
    }
    out
}

/// The session's loss on an upstream close 1009: Go's reader notifies the downstream
/// handler with gorilla's raw close error, so the client sees close 1009 with the
/// upstream's reason ("message too big" when empty).
/// `close_reason` keeps gorilla's `CloseError.Text` verbatim for the handler, which only
/// trims the message of a mapped 413.
fn closed_too_big(reason: &str) -> ExecError {
    let message = if reason.is_empty() { "message too big" } else { reason };
    ExecError::local(
        413,
        FailureScope::Request,
        serde_json::json!({
            "error": {"message": message, "type": "invalid_request_error", "code": "message_too_big"},
            "close_reason": reason,
        })
        .to_string(),
    )
}

/// `mapCodexWebsocketReadError` / `mapCodexWebsocketWriteError`: what a turn reports
/// after the upstream closed with 1009, a fixed request-scoped 413.
fn message_too_big() -> ExecError {
    ExecError::local(
        413,
        FailureScope::Request,
        r#"{"error":{"message":"upstream websocket message too big","type":"invalid_request_error","code":"message_too_big"}}"#,
    )
}

/// The error a turn reads from the session's loss: the mapped 413 for a 1009 close (only
/// [`closed_too_big`] makes a 413 loss), the loss itself otherwise.
/// The reader's error for a binary message (Go's `unexpected_binary` stage).
const UNEXPECTED_BINARY: &str = "codex websockets executor: unexpected binary message";

/// A failed or timed-out write: the request-scoped 413 when the reader already saw the
/// upstream close with 1009 (the frame was too big), else a transport error.
pub(crate) fn write_error(lost: Option<&ExecError>, message: &str) -> ExecError {
    lost.filter(|e| e.status == 413)
        .map_or_else(|| transport(message), turn_error)
}

fn turn_error(lost: &ExecError) -> ExecError {
    if lost.status == 413 {
        message_too_big()
    } else {
        lost.clone()
    }
}

/// `buildCodexResponsesWebsocketURL`.
fn ws_url(http_url: &str) -> Result<String, ExecError> {
    let mut url = url::Url::parse(http_url.trim()).map_err(|_| {
        ExecError::local(
            500,
            FailureScope::Credential,
            "codex websockets executor: invalid base URL",
        )
    })?;
    let scheme = match url.scheme().to_ascii_lowercase().as_str() {
        "http" => "ws",
        "https" => "wss",
        other => {
            return Err(ExecError::local(
                500,
                FailureScope::Credential,
                format!("codex websockets executor: unsupported responses websocket URL scheme {other:?}"),
            ));
        }
    };
    if url.host_str().unwrap_or_default().is_empty() {
        return Err(ExecError::local(
            500,
            FailureScope::Credential,
            "codex websockets executor: responses websocket URL host is empty",
        ));
    }
    url.set_scheme(scheme).expect("ws schemes are valid");
    Ok(url.into())
}

/// `buildCodexWebsocketRequestBody`: every turn is a `response.create`.
fn request_frame(body: String) -> String {
    let body = request::sanitize_input_ids(body);
    set_str(&body, "type", "response.create")
}

/// `buildCodexWebsocketErrorPayload` for an `error` frame with a positive status: the
/// status and the body Go classifies (and clears the reasoning replay with).
fn ws_error_body(payload: &str) -> Option<(u16, String)> {
    if gjson::get(payload, "type").str().trim() != "error" {
        return None;
    }
    let mut status = gjson::get(payload, "status").i64();
    if status == 0 {
        status = gjson::get(payload, "status_code").i64();
    }
    // Go accepts any positive status; larger than u16 cannot be represented here.
    let Ok(status) = u16::try_from(status) else {
        return None;
    };
    if status == 0 {
        return None;
    }
    let mut out = set_raw("{}", "status", &status.to_string());
    let body = gjson::get(payload, "body");
    let error = gjson::get(payload, "error");
    if body.exists() {
        out = set_raw(&out, "body", body.json());
    }
    if body.exists() && body.get("error").exists() {
        out = set_raw(&out, "error", body.get("error").json());
    } else if error.exists() {
        out = set_raw(&out, "error", error.json());
    } else {
        out = set_str(&out, "error.type", "server_error");
        out = set_str(&out, "error.message", response::go_status_text(status));
    }
    Some((status, out))
}

/// A request's reasoning replay (Claude clients only): the executor's cache and the
/// request's scope.
#[derive(Clone, Default)]
pub(super) struct Replay {
    cache: Arc<crate::codex_replay::Cache>,
    scope: crate::codex_replay::Scope,
}

impl Replay {
    /// `applyCodexReasoningReplayCacheRequired`: cached turns inserted into `body`.
    async fn apply(cache: &Arc<crate::codex_replay::Cache>, req: &ExecRequest, body: String) -> (String, Self) {
        let (body, scope) = crate::codex_replay::apply(cache, req, body).await;
        let replay = Self {
            cache: cache.clone(),
            scope,
        };
        (body, replay)
    }

    /// `clearCodexReasoningReplayOnWebsocketError` or `...OnInvalidSignature` for a
    /// rejection event: an invalid thinking signature drops the cached reasoning.
    fn clear(&self, payload: &str, cooling: bool) {
        if let Some((status, body)) = ws_error_body(payload) {
            crate::codex_replay::clear_on_invalid_signature(&self.cache, &self.scope, status, body.as_bytes());
        } else if let Some((error, body)) = response::terminal_failure(payload, cooling) {
            crate::codex_replay::clear_on_invalid_signature(&self.cache, &self.scope, error.status, body.as_bytes());
        }
    }

    /// `cacheCodexReasoningReplayFromCompleted`.
    fn completed(&self, payload: &str) {
        crate::codex_replay::cache_completed(&self.cache, &self.scope, payload.as_bytes());
    }
}

/// `parseCodexWebsocketErrorWithCooling`: an `error` frame that carries an HTTP status.
pub(crate) fn ws_error(payload: &str, model_level_cooling: bool) -> Option<ExecError> {
    let (status, out) = ws_error_body(payload)?;
    let mut headers = HeaderMap::new();
    let raw_headers = gjson::get(payload, "headers");
    if raw_headers.kind() == Kind::Object {
        raw_headers.each(|k, v| {
            let value = match v.kind() {
                Kind::String => v.str().trim().to_owned(),
                Kind::Number | Kind::True | Kind::False => v.json().trim().to_owned(),
                _ => String::new(),
            };
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::try_from(k.str().trim()),
                http::HeaderValue::from_str(&value),
            ) && !value.is_empty()
            {
                headers.insert(name, value);
            }
            true
        });
    }
    let connection_limit = [
        "error.code",
        "error.type",
        "body.error.code",
        "body.error.type",
        "code",
        "error",
    ]
    .iter()
    .any(|p| gjson::get(payload, p).str().trim() == "websocket_connection_limit_reached");
    let mut error = response::status_error_raw(status, &out, headers, model_level_cooling);
    if error.retry_after.is_none() && connection_limit {
        error.retry_after = Some(Duration::ZERO);
    }
    Some(error)
}

/// One upstream message, classified.
enum Frame {
    /// Empty text frame (heartbeat).
    Skip,
    Event {
        out: Bytes,
        raw_len: usize,
        bufferable: bool,
        terminal: bool,
    },
    /// The response failed. `read` means the reader already dropped the socket;
    /// `overload` is a transient capacity rejection another credential may serve.
    Failed {
        error: ExecError,
        overload: bool,
        read: bool,
        status_frame: bool,
    },
}

/// One turn on a pooled socket. Dropping it releases the turn lock and the reader.
struct Turn {
    rx: mpsc::Receiver<Read>,
    conn: Arc<Upstream>,
    session: Arc<Session>,
    _turn: OwnedMutexGuard<()>,
    items: OutputItems,
    saw_delta: bool,
    native: bool,
    cooling: bool,
    quota: Arc<QuotaSignals>,
    credential: String,
    /// Handshake headers plus quota headers from this turn's events.
    observed: HeaderMap,
    /// The request renamed the `collaboration` namespace (multi-agent v2).
    restore: bool,
    /// The attempt's usage record.
    usage: cpa_core::exec::UsageSink,
    /// The upstream model, for per-model quota snapshots.
    model: String,
    replay: Replay,
    /// The attempt's wire capture (Go's `api.websocket.*` timeline events).
    wire: crate::codex_capture::Wire,
    /// Inside the bootstrap window, where Go records an empty incomplete as a
    /// WebSocket error rather than a response error.
    bootstrapping: bool,
}

impl Drop for Turn {
    fn drop(&mut self) {
        self.conn.deactivate();
    }
}

impl Turn {
    async fn frame(&mut self) -> Frame {
        let frame = self.read().await;
        if let Frame::Failed { error, read: true, .. } = &frame {
            let binary = error.body.as_ref() == UNEXPECTED_BINARY.as_bytes();
            self.wire
                .ws_exec_error(if binary { "unexpected_binary" } else { "read" }, error);
        }
        frame
    }

    async fn read(&mut self) -> Frame {
        let text = match self.rx.recv().await {
            Some(Read::Text(text)) => text,
            Some(Read::Failed(error)) => {
                return Frame::Failed {
                    error,
                    overload: false,
                    read: true,
                    status_frame: false,
                };
            }
            // A full channel can drop the reader's terminal error; the session still
            // recorded the loss (Go's reader waits to deliver it).
            None => {
                return Frame::Failed {
                    error: self.loss().map_or_else(
                        || transport("codex websockets executor: session read channel closed"),
                        |l| turn_error(&l),
                    ),
                    overload: false,
                    read: true,
                    status_frame: false,
                };
            }
        };
        let payload = text.trim();
        if payload.is_empty() {
            return Frame::Skip;
        }
        self.observe(payload);
        self.wire.ws_response(payload.as_bytes());
        let raw_len = payload.len();
        let payload = response::restore(payload, self.restore);
        let payload = payload.as_ref();
        if let Some(error) = ws_error(payload, self.cooling) {
            self.replay.clear(payload, self.cooling);
            self.wire.ws_exec_error("upstream_error", &error);
            return Frame::Failed {
                error,
                overload: false,
                read: false,
                status_frame: true,
            };
        }
        if let Some((error, body)) = response::terminal_failure(payload, self.cooling) {
            self.replay.clear(payload, self.cooling);
            self.wire.ws_exec_error("upstream_error", &error);
            return Frame::Failed {
                error,
                overload: response::is_overload(&body),
                read: false,
                status_frame: false,
            };
        }
        let kind = gjson::get(payload, "type");
        let kind = kind.str().to_owned();
        if response::meaningful_delta(payload) {
            self.saw_delta = true;
        }
        if response::empty_incomplete(payload, self.items.len(), self.saw_delta) {
            let error = response::request_scoped(502, response::EMPTY_INCOMPLETE_MESSAGE);
            if self.bootstrapping {
                self.wire.ws_exec_error("upstream_error", &error);
            } else {
                self.wire.exec_error(&error);
            }
            return Frame::Failed {
                error,
                overload: false,
                read: false,
                status_frame: false,
            };
        }
        let bufferable = response::bufferable(payload);
        let mut out = payload.to_owned();
        let terminal = matches!(
            kind.as_str(),
            "response.completed" | "response.done" | "response.incomplete"
        );
        if kind == "response.output_item.done" {
            self.items.collect(payload);
        }
        if terminal {
            out = response::normalize_completion(out);
            if !self.native {
                out = self.items.patch(out);
            }
            if kind != "response.incomplete" {
                self.replay.completed(&out);
            }
        }
        Frame::Event {
            raw_len,
            out: Bytes::from(response::ensure_usage_details(out)),
            bufferable,
            terminal,
        }
    }

    /// Quota headers and usage from one upstream event, observed before collaboration
    /// names are restored (as Go does).
    fn observe(&mut self, payload: &str) {
        if let Some(headers) = codex_quota::event_headers(payload) {
            codex_quota::merge(&mut self.observed, &headers);
            self.quota.observe(&self.credential, &self.model, &self.observed);
        }
        if self.usage.enabled() {
            self.usage
                .response_line(cpa_core::format::Format::Codex, payload.as_bytes());
        }
    }

    fn invalidate(&self, error: &ExecError, notify: bool) {
        self.session.invalidate(&self.conn, error, notify);
    }

    /// The raw error that lost the session's socket, as the reader recorded it (the
    /// duplex wraps gorilla's close error unmapped).
    fn loss(&self) -> Option<ExecError> {
        self.session.closed.borrow().clone()
    }
}

/// The rest of a turn as a stream; ends after the terminal event or the first error.
fn rest(turn: Turn) -> ExecStream {
    futures_util::stream::unfold(Some(turn), |turn| async move {
        let mut turn = turn?;
        loop {
            match turn.frame().await {
                Frame::Skip => continue,
                Frame::Event { out, terminal, .. } => {
                    return Some((Ok(out), (!terminal).then_some(turn)));
                }
                Frame::Failed { error, read, .. } => {
                    if !read {
                        turn.invalidate(&error, true);
                    }
                    return Some((Err(error), None));
                }
            }
        }
    })
    .boxed()
}

/// Bootstrap buffering over WebSocket messages (codex_websockets_stream.go): every
/// message read counts toward the 48-frame window, held frames are bounded by 1 MiB.
async fn bootstrap(mut turn: Turn, timeout: Option<Duration>, started: Instant) -> Result<ExecStream, ExecError> {
    turn.bootstrapping = true;
    let mut held: Vec<Result<Bytes, ExecError>> = Vec::new();
    let (mut frames, mut bytes) = (0usize, 0usize);
    loop {
        let frame = turn.frame().await;
        frames += 1;
        let timed_out = timeout.is_some_and(|t| started.elapsed() >= t);
        let window_open = frames <= response::BOOTSTRAP_MAX_FRAMES && !timed_out;
        match frame {
            Frame::Skip if window_open => continue,
            Frame::Skip => break,
            Frame::Failed { error, read: true, .. } => return Err(error),
            Frame::Failed {
                error,
                status_frame: true,
                ..
            } => {
                turn.invalidate(&error, true);
                if !timed_out {
                    return Err(error);
                }
                held.push(Err(error));
                return Ok(futures_util::stream::iter(held).boxed());
            }
            Frame::Failed { error, overload, .. } => {
                if overload && !timed_out {
                    // Fail over before anything was shown; the downstream session survives.
                    turn.invalidate(&error, false);
                    let body = String::from_utf8_lossy(&error.body).into_owned();
                    return Err(response::bootstrap_overload(&body));
                }
                turn.invalidate(&error, true);
                held.push(Err(error));
                return Ok(futures_util::stream::iter(held).boxed());
            }
            Frame::Event {
                out,
                raw_len,
                bufferable,
                terminal,
            } => {
                let size = raw_len + out.len();
                if window_open && bufferable && !terminal && bytes + size <= response::BOOTSTRAP_MAX_BYTES {
                    bytes += size;
                    held.push(Ok(out));
                    continue;
                }
                held.push(Ok(out));
                if terminal {
                    return Ok(futures_util::stream::iter(held).boxed());
                }
                break;
            }
        }
    }
    turn.bootstrapping = false;
    Ok(futures_util::stream::iter(held).chain(rest(turn)).boxed())
}

impl CodexExecutor {
    /// Opens (or reuses) the session's socket for `target`. Returns the handshake
    /// headers when a new socket was dialed (`ensureUpstreamConn`).
    /// `wire` records a refused upgrade and a dial failure (the first dial of a turn;
    /// Go's retry records only `dial_retry`).
    async fn ensure(
        &self,
        session: &Arc<Session>,
        target: &Target,
        headers: &HeaderMap,
        model: &str,
        model_level_cooling: bool,
        wire: Option<&crate::codex_capture::Wire>,
    ) -> Result<(Arc<Upstream>, Option<HeaderMap>), ExecError> {
        if let Some(current) = session.current() {
            if current.target == *target {
                return Ok((current, None));
            }
            // target_changed: another credential, URL or proxy now serves this session.
            let mut slot = session.conn.lock().expect("session conn");
            if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, &current)) {
                *slot = None;
            }
            drop(slot);
            current.shutdown();
        }
        let (socket, handshake) = self.dial(target, headers, model, model_level_cooling, wire).await?;
        let (sink, stream) = socket.split();
        let conn = Upstream::new(target.clone(), sink, self.ws.write);
        // Publish before the reader runs, so a reader that fails at once still finds its
        // socket current and invalidates it; holding the handle slot keeps a concurrent
        // shutdown from missing the abort handle.
        let mut reader = conn.reader.lock().expect("reader handle");
        *session.conn.lock().expect("session conn") = Some(conn.clone());
        let task = tokio::spawn(read_loop(stream, Arc::downgrade(session), conn.clone(), self.ws.idle));
        *reader = Some(task.abort_handle());
        drop(reader);
        Ok((conn, Some(handshake)))
    }

    async fn dial(
        &self,
        target: &Target,
        request: &HeaderMap,
        model: &str,
        model_level_cooling: bool,
        wire: Option<&crate::codex_capture::Wire>,
    ) -> Result<(WebSocket, HeaderMap), ExecError> {
        // `newProxyAwareWebsocketDialer`: Go's standard dialer, environment proxies
        // included when none is configured, with `EnableCompression`.
        let mut headers = request.clone();
        let key = offer_compression(&mut headers);
        let builder = self
            .transport
            .standard(&target.proxy)
            .websocket(&target.url)
            .headers(headers)
            .accept_key(key.clone());
        let mut rejected = false;
        let attempt = async {
            let mut res = builder
                .send()
                .await
                .map_err(|_| transport("codex websockets executor: dial failed"))?;
            let status = res.status().as_u16();
            let handshake = res.headers().clone();
            if status != 101 {
                let inner = std::mem::replace(&mut *res, wreq::Response::from(http::Response::new(Vec::<u8>::new())));
                let mut body = Vec::new();
                let mut stream = inner.bytes_stream();
                while let Some(Ok(chunk)) = stream.next().await {
                    body.extend_from_slice(&chunk[..chunk.len().min(MAX_HANDSHAKE_BODY - body.len())]);
                    if body.len() >= MAX_HANDSHAKE_BODY {
                        break;
                    }
                }
                self.quota_observe(&target.credential, model, &handshake);
                rejected = true;
                if let Some(wire) = wire {
                    wire.upgrade_rejection(&target.url, request, status, &handshake, &body);
                }
                if status == 426 {
                    // Go returns a plain statusErr for 426 on downstream WebSockets.
                    let text = String::from_utf8_lossy(&body).into_owned();
                    return Err(response::status_error_raw(status, &text, handshake, false));
                }
                return Err(response::status_error(status, &body, handshake, model_level_cooling));
            }
            // ponytail: Go reports a failed negotiation (and a bad handshake) as a status
            // error carrying 101; here both are transport failures.
            let socket = upgrade(&mut res, &key, &handshake).await.map_err(|e| match e {
                UpgradeError::Handshake => transport("codex websockets executor: websocket handshake failed"),
                UpgradeError::Compression => {
                    transport("codex websockets executor: websocket: invalid compression negotiation")
                }
            })?;
            Ok((socket, handshake))
        };
        let dialed = tokio::time::timeout(HANDSHAKE_TIMEOUT, attempt)
            .await
            .unwrap_or_else(|_| Err(transport("codex websockets executor: handshake timed out")));
        // A refused upgrade was recorded as an HTTP attempt; anything else is a dial error.
        if let (Err(error), Some(wire)) = (&dialed, wire)
            && !rejected
        {
            wire.ws_exec_error("dial", error);
        }
        dialed
    }

    fn quota_observe(&self, credential: &str, model: &str, headers: &HeaderMap) {
        self.quota().observe(credential, model, headers);
    }

    /// One Responses turn over the session's upstream socket (`ExecuteStream` on
    /// `CodexWebsocketsExecutor` with a downstream WebSocket).
    pub(crate) async fn stream_ws(
        &self,
        view: &View<'_>,
        settings: &Settings,
        req: ExecRequest,
        exec_session: &ExecSession,
    ) -> Result<ExecResponse, ExecError> {
        let model = request::base_model(&req.model).to_owned();
        let request::Shaped {
            body,
            optimized,
            conflict,
        } = request::shape(&req, view, settings, Call::Websocket)?;
        let (body, replay) = Replay::apply(&self.replay, &req, body).await;
        crate::codex::report_request(&req, cpa_core::format::Format::Codex, &body);
        let (body, cache) = request::prompt_cache(&req, body, Some(&exec_session.id), true);
        let native = request::is_native(&req);
        let headers = request::ws_headers(view, settings, &req.headers, &body, &model, cache.as_deref(), native);
        let target = Target {
            credential: view.credential.id.clone(),
            url: ws_url(&format!("{}/responses", view.base_url))?,
            proxy: view.proxy.clone(),
        };
        let session = self.ws.session(&exec_session.id);
        // `WebsocketInputFromContext(ctx) != nil && cfg.Codex.ResponseSteering`.
        let steering = settings
            .response_steering
            .then(|| session.steering())
            .flatten()
            .map(|input| {
                let initial =
                    duplex::Prepared::new(&body, &req.original_body, optimized, conflict, native, replay.clone());
                (input, initial)
            });
        let guard = session.turn.clone().lock_owned().await;
        let frame = request_frame(body);
        let wire = crate::codex_capture::Wire::new(req.capture(), view.credential);
        wire.ws_request(&target.url, &headers, frame.as_bytes());
        let (mut conn, mut handshake) = if exec_session.continuation {
            match session.current().filter(|c| c.target == target) {
                Some(conn) => (conn, None),
                None => return Err(ExecError::replay_required()),
            }
        } else {
            self.ensure(
                &session,
                &target,
                &headers,
                &model,
                settings.model_level_cooling,
                Some(&wire),
            )
            .await?
        };
        conn.bind(&session, exec_session.lease.as_ref())?;
        if let Some(handshake) = &handshake {
            wire.ws_handshake(101, handshake);
        }
        let started = Instant::now();
        let mut rx = conn.activate();
        let mut restore = conn.restores(optimized, conflict);
        if let Err(error) = conn.send(frame.clone()).await {
            wire.ws_exec_error("send", &error);
            // `shouldRetryCodexWebsocketSend`: request-scoped failures (413) never retry.
            let retry = error.scope != FailureScope::Request;
            if exec_session.continuation {
                session.invalidate(&conn, &error, false);
                return Err(if retry { ExecError::replay_required() } else { error });
            }
            session.invalidate(&conn, &error, true);
            if !retry {
                return Err(error);
            }
            // Retry once on a fresh socket: upstream may have closed it between turns.
            let (fresh, fresh_handshake) = match self
                .ensure(&session, &target, &headers, &model, settings.model_level_cooling, None)
                .await
            {
                Ok(fresh) => fresh,
                Err(error) => {
                    wire.ws_exec_error("dial_retry", &error);
                    return Err(error);
                }
            };
            fresh.bind(&session, exec_session.lease.as_ref())?;
            rx = fresh.activate();
            restore = fresh.restores(optimized, conflict);
            wire.ws_request(&target.url, &headers, frame.as_bytes());
            if let Some(handshake) = &fresh_handshake {
                wire.ws_handshake(101, handshake);
            }
            if let Err(error) = fresh.send(frame).await {
                wire.ws_exec_error("send_retry", &error);
                session.invalidate(&fresh, &error, true);
                return Err(error);
            }
            conn = fresh;
            handshake = fresh_handshake;
        }
        conn.note_multi_agent(optimized, conflict);
        let observed = handshake.clone().unwrap_or_default();
        if !observed.is_empty() {
            self.quota_observe(&view.credential.id, &model, &observed);
        }
        let turn = Turn {
            rx,
            conn,
            session,
            _turn: guard,
            items: OutputItems::default(),
            saw_delta: false,
            native,
            cooling: settings.model_level_cooling,
            quota: self.quota_handle(),
            credential: view.credential.id.clone(),
            observed,
            restore,
            usage: req.usage.clone(),
            model: model.clone(),
            replay,
            wire,
            bootstrapping: false,
        };
        if let Some((input, initial)) = steering {
            // The socket now belongs to this connection; bootstrap buffering does not apply.
            let stream = duplex::Duplex::start(turn, input, initial, req, view, settings, &exec_session.id).await;
            return Ok(ExecResponse {
                status: 200,
                headers: handshake.unwrap_or_default(),
                body: ResponseBody::Stream(stream),
            });
        }
        // The response waits for the Home replay writes it caused, as on HTTP.
        let writes = turn.replay.scope.writes.clone();
        let stream = if settings.bootstrap_buffering {
            match bootstrap(turn, settings.bootstrap_timeout, started).await {
                Ok(stream) => stream,
                Err(error) => {
                    writes.settle().await;
                    return Err(error);
                }
            }
        } else {
            rest(turn)
        };
        Ok(ExecResponse {
            status: 200,
            headers: handshake.unwrap_or_default(),
            body: ResponseBody::Stream(writes.gate(stream)),
        })
    }
}

/// tungstenite's reports of a peer that went away without a close frame, which
/// gorilla/websocket reads as close 1006 "unexpected EOF".
fn ends_without_close(error: &tokio_tungstenite::tungstenite::Error) -> bool {
    use tokio_tungstenite::tungstenite::Error;
    use tokio_tungstenite::tungstenite::error::ProtocolError;
    match error {
        Error::Protocol(ProtocolError::ResetWithoutClosingHandshake) => true,
        Error::Io(io) => io.kind() == std::io::ErrorKind::UnexpectedEof,
        _ => false,
    }
}

/// The socket's single reader. Exits on error, idle timeout, or abort.
///
/// Like Go's read deadline, `idle` starts when the read for the next application message
/// begins; control frames answered meanwhile do not extend it.
pub(crate) async fn read_loop(
    mut stream: SplitStream<WebSocket>,
    session: Weak<Session>,
    conn: Arc<Upstream>,
    idle: Duration,
) {
    let error = 'read: loop {
        let deadline = tokio::time::Instant::now() + idle;
        let text = loop {
            let message = match tokio::time::timeout_at(deadline, stream.next()).await {
                Err(_) => break 'read transport("codex websockets executor: read idle timeout"),
                // gorilla/websocket reports a connection that ends without a close frame
                // as close 1006; Go surfaces its text, which marks a lifecycle failure.
                Ok(None) => break 'read transport(&close_error_text(1006, "unexpected EOF")),
                Ok(Some(Err(error))) if ends_without_close(&error) => {
                    break 'read transport(&close_error_text(1006, "unexpected EOF"));
                }
                Ok(Some(Err(_))) => break 'read transport("codex websockets executor: read failed"),
                Ok(Some(Ok(message))) => message,
            };
            match message {
                Message::Text(text) => break text.as_str().to_owned(),
                Message::Binary(_) => break 'read transport(UNEXPECTED_BINARY),
                Message::Close(frame) => {
                    if let Some(frame) = frame.as_ref().filter(|f| u16::from(f.code) == 1009) {
                        break 'read closed_too_big(frame.reason.as_str());
                    }
                    let text = match frame {
                        Some(frame) => close_error_text(u16::from(frame.code), frame.reason.as_str()),
                        None => close_error_text(1005, ""),
                    };
                    break 'read transport(&text);
                }
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            }
        };
        let active = conn.link.lock().expect("link").active.clone();
        if let Some(tx) = active {
            // Waits while the turn is busy: backpressure instead of unbounded queueing.
            let _ = tx.send(Read::Text(text)).await;
        }
    };
    let active = {
        let mut link = conn.link.lock().expect("link");
        link.lost = Some(error.clone());
        link.active.clone()
    };
    // The session records the loss before the turn reads its error, so a handler that
    // sees the turn fail also finds the loss (Go's notifier closes the client first).
    match session.upgrade() {
        Some(session) => session.invalidate(&conn, &error, true),
        None => conn.deactivate(),
    }
    if let Some(tx) = active {
        let _ = tx.try_send(Read::Failed(turn_error(&error)));
    }
}

/// Why [`upgrade`] refused a 101 answer.
pub(crate) enum UpgradeError {
    /// gorilla `Dial`'s checks failed (Upgrade, Connection, Sec-WebSocket-Accept) or the
    /// connection could not be taken over.
    Handshake,
    /// The permessage-deflate answer does not match the offer.
    Compression,
}

/// gorilla `Dialer` with `EnableCompression`: offers permessage-deflate on the request
/// and returns the `Sec-WebSocket-Key` to send with it. Shared by the Codex and xAI
/// WebSocket executors (Go's xAI dialer also enables compression).
pub(crate) fn offer_compression(headers: &mut HeaderMap) -> String {
    headers.insert(
        http::header::SEC_WEBSOCKET_EXTENSIONS,
        http::HeaderValue::from_static(deflate::OFFER),
    );
    tokio_tungstenite::tungstenite::handshake::client::generate_key()
}

/// gorilla `Dial`'s checks on a 101 answer, then its permessage-deflate agreement;
/// wraps the upgraded connection for [`Upstream`] and [`read_loop`].
pub(crate) async fn upgrade(
    res: &mut wreq::ws::WebSocketResponse,
    key: &str,
    handshake: &HeaderMap,
) -> Result<WebSocket, UpgradeError> {
    let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
    if !token_list_contains(handshake, http::header::UPGRADE, "websocket")
        || !token_list_contains(handshake, http::header::CONNECTION, "upgrade")
        || handshake.get(http::header::SEC_WEBSOCKET_ACCEPT).map(|v| v.as_bytes()) != Some(accept.as_bytes())
    {
        return Err(UpgradeError::Handshake);
    }
    let compressed = deflate::negotiated(handshake).map_err(|()| UpgradeError::Compression)?;
    let response = std::mem::replace(&mut **res, wreq::Response::from(http::Response::new(Vec::<u8>::new())));
    let upgraded = response.upgrade().await.map_err(|_| UpgradeError::Handshake)?;
    let config = WebSocketConfig::default()
        .max_frame_size(Some(MAX_UPSTREAM_MESSAGE))
        .max_message_size(Some(MAX_UPSTREAM_MESSAGE));
    Ok(WebSocketStream::from_raw_socket(
        Inflate::new(upgraded, compressed, MAX_UPSTREAM_MESSAGE),
        Role::Client,
        Some(config),
    )
    .await)
}

/// gorilla `tokenListContainsValue`: a comma-separated token list holds `value`
/// (ASCII case-insensitive).
fn token_list_contains(headers: &HeaderMap, name: http::HeaderName, value: &str) -> bool {
    headers.get_all(name).iter().any(|v| {
        v.to_str().is_ok_and(|v| {
            v.split(',')
                .any(|t| t.trim_matches([' ', '\t']).eq_ignore_ascii_case(value))
        })
    })
}

#[path = "codex_ws_deflate.rs"]
pub(crate) mod deflate;
#[path = "codex_duplex.rs"]
mod duplex;
pub use duplex::{ClientFrames, SteeringInput};

#[cfg(test)]
#[path = "codex_ws_tests.rs"]
mod tests;

#[cfg(test)]
mod close_text_tests {
    use super::close_error_text;

    /// gorilla/websocket `CloseError.Error()` texts, which Go returns unchanged.
    #[test]
    fn close_errors_read_like_gorilla() {
        assert_eq!(close_error_text(1000, ""), "websocket: close 1000 (normal)");
        assert_eq!(close_error_text(1001, "bye"), "websocket: close 1001 (going away): bye");
        assert_eq!(
            close_error_text(1006, "unexpected EOF"),
            "websocket: close 1006 (abnormal closure): unexpected EOF"
        );
        assert_eq!(close_error_text(4000, "x"), "websocket: close 4000: x");
    }
}
