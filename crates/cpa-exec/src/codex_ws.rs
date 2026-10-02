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
use cpa_core::config::Config;
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, ExecSession, ExecStream, FailureScope, ResponseBody};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use gjson::Kind;
use http::HeaderMap;
use tokio::sync::{OwnedMutexGuard, mpsc, watch};
use wreq::ws::WebSocket;
use wreq::ws::message::Message;

use crate::codex::CodexExecutor;
use crate::codex_json::{set_raw, set_str};
use crate::codex_quota::{self, QuotaSignals};
use crate::codex_request::{self as request, Call, Settings, View};
use crate::codex_response::{self as response, OutputItems};

/// `codexResponsesWebsocketIdleTimeout`: read deadline renewed before every read.
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// `codexResponsesWebsocketHandshakeTO`.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Go's per-turn read channel capacity.
const TURN_BUFFER: usize = 4096;
/// Bound on a handshake rejection body.
const MAX_HANDSHAKE_BODY: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    credential: String,
    url: String,
    proxy: String,
}

enum Read {
    Text(String),
    /// The socket failed; the reader has already invalidated it.
    Failed(ExecError),
}

struct Upstream {
    target: Target,
    sink: tokio::sync::Mutex<SplitSink<WebSocket, Message>>,
    /// The turn currently reading, if any.
    active: Mutex<Option<mpsc::Sender<Read>>>,
    reader: Mutex<Option<tokio::task::AbortHandle>>,
    /// Why the reader stopped (`upstreamDisconnectError`).
    lost: Mutex<Option<ExecError>>,
}

impl Upstream {
    fn activate(&self) -> mpsc::Receiver<Read> {
        let (tx, rx) = mpsc::channel(TURN_BUFFER);
        *self.active.lock().expect("active turn") = Some(tx);
        rx
    }

    fn deactivate(&self) {
        self.active.lock().expect("active turn").take();
    }

    /// `writeCodexWebsocketMessage` + `mapCodexWebsocketWriteError`: a write after the
    /// upstream closed with 1009 reports the request-scoped 413 instead.
    async fn send(&self, frame: String) -> Result<(), ExecError> {
        let sent = self.sink.lock().await.send(Message::text(frame)).await;
        sent.map_err(|_| {
            self.lost
                .lock()
                .expect("lost")
                .clone()
                .filter(|e| e.status == 413)
                .unwrap_or_else(|| transport("codex websockets executor: write failed"))
        })
    }

    /// Stops the reader and closes the socket. Idempotent.
    fn shutdown(self: &Arc<Self>) {
        if let Some(reader) = self.reader.lock().expect("reader handle").take() {
            reader.abort();
        }
        self.deactivate();
        let this = self.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(1), async {
                let mut sink = this.sink.lock().await;
                let _ = sink.send(Message::close(None)).await;
                let _ = sink.close().await;
            })
            .await;
        });
    }
}

struct Session {
    /// Serialises turns (`reqMu`).
    turn: Arc<tokio::sync::Mutex<()>>,
    conn: Mutex<Option<Arc<Upstream>>>,
    /// Set once when the session's socket is lost (`notifyUpstreamDisconnect`).
    closed: watch::Sender<Option<ExecError>>,
}

impl Session {
    fn current(&self) -> Option<Arc<Upstream>> {
        self.conn.lock().expect("session conn").clone()
    }

    /// `invalidateUpstreamConn`: only the session's current socket is dropped, so a stale
    /// reader cannot tear down its replacement. `notify` tells the downstream handler.
    fn invalidate(&self, conn: &Arc<Upstream>, error: &ExecError, notify: bool) {
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
#[derive(Default)]
pub(crate) struct Pool {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

impl Pool {
    fn session(&self, id: &str) -> Arc<Session> {
        self.sessions
            .lock()
            .expect("sessions")
            .entry(id.to_owned())
            .or_insert_with(|| {
                Arc::new(Session {
                    turn: Arc::default(),
                    conn: Mutex::default(),
                    closed: watch::channel(None).0,
                })
            })
            .clone()
    }

    /// `CloseExecutionSession`: the downstream connection ended.
    pub fn close(&self, id: &str) {
        let session = self.sessions.lock().expect("sessions").remove(id);
        if let Some(conn) = session.and_then(|s| s.conn.lock().expect("session conn").take()) {
            conn.shutdown();
        }
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

/// Close 1009 from upstream: the client must shrink the request. Go's reader notifies
/// the downstream handler with the raw close error, so the client sees close 1009 with
/// the upstream's reason ("message too big" when empty); the request-scoped 413 keeps the
/// same reason (`mapCodexWebsocketReadError`).
fn message_too_big(reason: &str) -> ExecError {
    let reason = if reason.is_empty() { "message too big" } else { reason };
    ExecError::local(
        413,
        FailureScope::Request,
        serde_json::json!({"error": {"message": reason, "type": "invalid_request_error", "code": "message_too_big"}})
            .to_string(),
    )
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

/// `executionProxyURL`: credential proxy, then the global `requests.proxy-url`.
// ponytail: adapter for the proxy-aware client (crates/cpa-exec/src/proxy.rs, Claude
// thread). Only `direct`/`none` and plain proxy URLs are understood; no SOCKS auth
// redaction or transport cache.
fn proxy_url(view: &View<'_>, cfg: &Config) -> String {
    let attr = view.attr("proxy_url").trim();
    if !attr.is_empty() {
        return attr.to_owned();
    }
    ["requests", "proxy-url"]
        .iter()
        .try_fold(&cfg.document, |v, k| v.get(*k))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// `buildCodexWebsocketRequestBody`: every turn is a `response.create`.
fn request_frame(body: String) -> String {
    let body = request::sanitize_input_ids(body);
    set_str(&body, "type", "response.create")
}

/// `parseCodexWebsocketErrorWithCooling`: an `error` frame that carries an HTTP status.
pub(crate) fn ws_error(payload: &str, model_level_cooling: bool) -> Option<ExecError> {
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
}

impl Drop for Turn {
    fn drop(&mut self) {
        self.conn.deactivate();
    }
}

impl Turn {
    async fn frame(&mut self) -> Frame {
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
            None => {
                return Frame::Failed {
                    error: transport("codex websockets executor: session read channel closed"),
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
        if let Some(headers) = codex_quota::event_headers(payload) {
            codex_quota::merge(&mut self.observed, &headers);
            self.quota.observe(&self.credential, &self.observed);
        }
        if let Some(error) = ws_error(payload, self.cooling) {
            return Frame::Failed {
                error,
                overload: false,
                read: false,
                status_frame: true,
            };
        }
        if let Some((error, body)) = response::terminal_failure(payload, self.cooling) {
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
            return Frame::Failed {
                error: response::request_scoped(502, response::EMPTY_INCOMPLETE_MESSAGE),
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
        }
        Frame::Event {
            raw_len: payload.len(),
            out: Bytes::from(response::ensure_usage_details(out)),
            bufferable,
            terminal,
        }
    }

    fn invalidate(&self, error: &ExecError, notify: bool) {
        self.session.invalidate(&self.conn, error, notify);
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
    Ok(futures_util::stream::iter(held).chain(rest(turn)).boxed())
}

impl CodexExecutor {
    /// Opens (or reuses) the session's socket for `target`. Returns the handshake
    /// headers when a new socket was dialed (`ensureUpstreamConn`).
    async fn ensure(
        &self,
        session: &Arc<Session>,
        target: &Target,
        headers: &HeaderMap,
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
        let (socket, handshake) = self.dial(target, headers).await?;
        let (sink, stream) = socket.split();
        let conn = Arc::new(Upstream {
            target: target.clone(),
            sink: tokio::sync::Mutex::new(sink),
            active: Mutex::default(),
            reader: Mutex::default(),
            lost: Mutex::default(),
        });
        let reader = tokio::spawn(read_loop(stream, Arc::downgrade(session), conn.clone()));
        *conn.reader.lock().expect("reader handle") = Some(reader.abort_handle());
        *session.conn.lock().expect("session conn") = Some(conn.clone());
        Ok((conn, Some(handshake)))
    }

    async fn dial(&self, target: &Target, headers: &HeaderMap) -> Result<(WebSocket, HeaderMap), ExecError> {
        let mut builder = self.client.websocket(&target.url).headers(headers.clone());
        match target.proxy.as_str() {
            "" => {}
            p if p.eq_ignore_ascii_case("direct") || p.eq_ignore_ascii_case("none") => {}
            p => {
                let proxy =
                    wreq::Proxy::all(p).map_err(|_| transport("codex websockets executor: invalid proxy URL"))?;
                builder = builder.proxy(proxy);
            }
        }
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
                self.quota_observe(&target.credential, &handshake);
                if status == 426 {
                    // Go returns a plain statusErr for 426 on downstream WebSockets.
                    let text = String::from_utf8_lossy(&body).into_owned();
                    return Err(response::status_error_raw(status, &text, handshake, false));
                }
                return Err(response::status_error(status, &body, handshake, false));
            }
            let socket = res
                .into_websocket()
                .await
                .map_err(|_| transport("codex websockets executor: websocket handshake failed"))?;
            Ok((socket, handshake))
        };
        tokio::time::timeout(HANDSHAKE_TIMEOUT, attempt)
            .await
            .unwrap_or_else(|_| Err(transport("codex websockets executor: handshake timed out")))
    }

    fn quota_observe(&self, credential: &str, headers: &HeaderMap) {
        self.quota().observe(credential, headers);
    }

    /// One Responses turn over the session's upstream socket (`ExecuteStream` on
    /// `CodexWebsocketsExecutor` with a downstream WebSocket).
    pub(crate) async fn stream_ws(
        &self,
        view: &View<'_>,
        settings: &Settings,
        cfg: &Config,
        req: ExecRequest,
        exec_session: &ExecSession,
    ) -> Result<ExecResponse, ExecError> {
        let model = request::base_model(&req.model).to_owned();
        let body = request::shape(&req, view, settings, Call::Websocket)?;
        let (body, cache) = request::prompt_cache(&req, body, Some(&exec_session.id), true);
        let native = request::is_native(&req);
        let headers = request::ws_headers(view, settings, &req.headers, &body, &model, cache.as_deref(), native);
        let target = Target {
            credential: view.credential.id.clone(),
            url: ws_url(&format!("{}/responses", view.base_url))?,
            proxy: proxy_url(view, cfg),
        };
        let session = self.ws.session(&exec_session.id);
        let guard = session.turn.clone().lock_owned().await;
        let (mut conn, mut handshake) = if exec_session.continuation {
            match session.current().filter(|c| c.target == target) {
                Some(conn) => (conn, None),
                None => return Err(ExecError::replay_required()),
            }
        } else {
            self.ensure(&session, &target, &headers).await?
        };
        let frame = request_frame(body);
        let started = Instant::now();
        let mut rx = conn.activate();
        if let Err(error) = conn.send(frame.clone()).await {
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
            let (fresh, fresh_handshake) = self.ensure(&session, &target, &headers).await?;
            rx = fresh.activate();
            if let Err(error) = fresh.send(frame).await {
                session.invalidate(&fresh, &error, true);
                return Err(error);
            }
            conn = fresh;
            handshake = fresh_handshake;
        }
        let observed = handshake.clone().unwrap_or_default();
        if !observed.is_empty() {
            self.quota_observe(&view.credential.id, &observed);
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
        };
        let stream = if settings.bootstrap_buffering {
            bootstrap(turn, settings.bootstrap_timeout, started).await?
        } else {
            rest(turn)
        };
        Ok(ExecResponse {
            status: 200,
            headers: handshake.unwrap_or_default(),
            body: ResponseBody::Stream(stream),
        })
    }
}

/// The socket's single reader. Exits on error, idle timeout, or abort.
async fn read_loop(mut stream: SplitStream<WebSocket>, session: Weak<Session>, conn: Arc<Upstream>) {
    let error = loop {
        let next = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
            Err(_) => break transport("codex websockets executor: read idle timeout"),
            Ok(None) => break transport("codex websockets executor: upstream closed the connection"),
            Ok(Some(Err(_))) => break transport("codex websockets executor: read failed"),
            Ok(Some(Ok(message))) => message,
        };
        let text = match next {
            Message::Text(text) => text.as_str().to_owned(),
            Message::Binary(_) => break transport("codex websockets executor: unexpected binary message"),
            Message::Close(frame) => {
                if let Some(frame) = frame.filter(|f| u16::from(f.code.clone()) == 1009) {
                    break message_too_big(frame.reason.as_str());
                }
                break transport("codex websockets executor: upstream closed the connection");
            }
            Message::Ping(_) | Message::Pong(_) => continue,
        };
        let active = conn.active.lock().expect("active turn").clone();
        if let Some(tx) = active {
            // Waits while the turn is busy: backpressure instead of unbounded queueing.
            let _ = tx.send(Read::Text(text)).await;
        }
    };
    *conn.lost.lock().expect("lost") = Some(error.clone());
    let active = conn.active.lock().expect("active turn").clone();
    if let Some(tx) = active {
        let _ = tx.try_send(Read::Failed(error.clone()));
    }
    match session.upgrade() {
        Some(session) => session.invalidate(&conn, &error, true),
        None => conn.deactivate(),
    }
}

#[cfg(test)]
#[path = "codex_ws_tests.rs"]
mod tests;
