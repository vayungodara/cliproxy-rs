//! Responses WebSocket: `GET /v1/responses` and `GET /backend-api/codex/responses`
//! (sdk/api/handlers/openai/openai_responses_websocket.go, _forward.go).
//!
//! One task per downstream connection. It reads one request, runs it to completion and
//! only then reads the next, so a connection holds at most one turn, one upstream stream
//! and the transcript of its last turn. Nothing polls: between turns the task waits on the
//! client and on the executor's upstream-loss signal; during a turn on the stream and an
//! optional keep-alive deadline. When the client goes away the task ends and releases the
//! executor session (`Executors::close_session`).
//!
//! Turns go upstream in one of two modes, decided by the credential that served the last
//! turn:
//! - WebSocket (Codex credential with `websockets: true`): the client's frames pass
//!   through with `previous_response_id` and incremental input, pinned to that credential's
//!   socket. A continuation that cannot run there closes with 1012 so the client replays.
//! - HTTP: the handler keeps the transcript and sends each turn as a full request.
//!
//! Response steering (`codex.response-steering`): a turn served by a Codex credential in
//! WebSocket mode runs full duplex. The executor keeps the upstream socket for the rest of
//! the connection and, after the first `response.created`, takes the client's later frames
//! (`response.steer`, more creates) from the connection's frame queue. That turn ends only
//! with the connection. With steering configured, one reader task fills that queue for
//! the whole connection (`readResponsesWebsocketInput`): this task takes requests from it
//! between turns, and a client that goes away cancels the turn in flight.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Extension;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::{Caller, ExecError, ExecSession, ExecStream, Operation};
use cpa_core::format::Format;
use cpa_exec::codex::{CodexExecutor, SteeringInput};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{FutureExt, SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};

use crate::registry::provider_key;
use crate::runtime::Runtime;
use crate::scheduler::canonical_model;
use crate::websocket_requests::{
    self as requests, APPEND, CREATE, Turn, WsError, delete, error_payload, field, payloads_from_chunk,
};
use crate::websocket_tools::{self as tools, Retained, TurnCache};
use crate::{classify, dispatch};

pub fn routes() -> Router<Arc<Runtime>> {
    Router::new()
        .route("/v1/responses", get(upgrade))
        .route("/backend-api/codex/responses", get(upgrade))
}

/// `wsCloseReasonMaxBytes`.
const CLOSE_REASON_MAX: usize = 123;
/// Close code 1012 (service restart): the client must replay over HTTP.
const CLOSE_SERVICE_RESTART: u16 = 1012;
/// Close code 1009 (message too big).
const CLOSE_MESSAGE_TOO_BIG: u16 = 1009;
const TURN_STATE: &str = "x-codex-turn-state";
// ponytail: gorilla reads messages of any size; 64 MiB (the HTTP body cap) bounds
// per-connection memory instead.
const MAX_MESSAGE: usize = crate::MAX_REQUEST_BYTES;
/// Bound on a terminal close or error write. Go closes the socket at once when a data
/// write is still in flight (`closeForUpstreamError`); here the write may have been
/// interrupted with bytes still queued, and a client that stopped reading would hold the
/// flush, the connection task and its executor session forever.
// ponytail: one fixed best-effort bound instead of Go's in-flight-writer check.
const TERMINAL_WRITE: Duration = Duration::from_secs(1);
/// `readResponsesWebsocketInput`'s queue: steering frames backpressure the client.
const STEERING_QUEUE: usize = 16;

/// The connection's write half (reads go through [`Client`]) and its request-log
/// timeline (Go `websocketTimelineLog`).
struct Sink {
    inner: SplitSink<WebSocket, Message>,
    timeline: Option<crate::request_logging::WebsocketLog>,
    /// Go's `wsTerminateErr`: why the connection ended, the final `websocket.disconnect`.
    terminated: Option<String>,
}

/// gorilla's text for the error after this side closed the connection.
const CLOSE_SENT: &str = "websocket: close sent";

impl Sink {
    /// Go `websocketTimelineLog.Append`: one `formatWebsocketTimelineEvent` part.
    fn event(&self, kind: &str, payload: &[u8]) {
        let Some(timeline) = &self.timeline else {
            return;
        };
        let payload = payload.trim_ascii();
        if payload.is_empty() {
            return;
        }
        let mut part = format!(
            "Timestamp: {}\nEvent: websocket.{kind}\n",
            crate::request_logging::timestamp(chrono::Local::now())
        )
        .into_bytes();
        part.extend_from_slice(payload);
        part.push(b'\n');
        // Go only warns when a timeline part cannot be written.
        let _ = timeline.append_part(&part);
    }

    fn terminate(&mut self, reason: &str) {
        self.terminated = Some(reason.to_owned());
    }

    /// The connection's end: the deferred `appendWebsocketTimelineDisconnect`, the socket
    /// closed at once (a pending close reply gets `TERMINAL_WRITE`, gorilla's bounded
    /// control write), and only then the timeline's delivery, which can wait on disk or
    /// Home forwarding (Go closes TCP before its deferred log write).
    async fn finish(mut self, client: Client) {
        if let Some(reason) = self.terminated.take() {
            self.event("disconnect", reason.as_bytes());
        }
        let Self {
            mut inner, timeline, ..
        } = self;
        let _ = tokio::time::timeout(TERMINAL_WRITE, inner.flush()).await;
        drop(inner);
        drop(client);
        if let Some(timeline) = timeline {
            timeline.close().await;
        }
    }
}

/// gorilla `returnError` for a request that is not a websocket handshake: plain status
/// text and the supported version.
pub(crate) fn upgrade_rejected() -> Response {
    let mut response = (StatusCode::BAD_REQUEST, "Bad Request\n").into_response();
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert(header::SEC_WEBSOCKET_VERSION, HeaderValue::from_static("13"));
    response
}

async fn upgrade(
    State(rt): State<Arc<Runtime>>,
    Extension(caller): Extension<Caller>,
    peer: dispatch::Peer,
    matched: axum::extract::MatchedPath,
    uri: axum::http::Uri,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let Ok(ws) = ws else {
        return upgrade_rejected();
    };
    // `websocketUpgradeHeaders`: keep sticky turn state across reconnects.
    let turn_state = headers
        .get(TURN_STATE)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .and_then(|v| HeaderValue::from_str(v).ok());
    let mut connection = Connection::new(rt, caller, headers);
    // Go `request_path` metadata: gin FullPath of the upgrade route.
    connection.request_path = matched.as_str().to_owned();
    connection.query = uri.query().unwrap_or_default().to_owned();
    connection.peer = dispatch::peer(peer);
    // The upgrade request's log: Go writes the WebSocket timeline and every turn's
    // upstream capture into the gin context of the upgrade.
    if let Some(log) = crate::request_logging::current() {
        connection.capture = log.capture_sink();
        connection.request_id = Some(log.request_id()).filter(|id| !id.is_empty());
        connection.timeline = log.detach_websocket();
    }
    let mut response = ws
        .max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| connection.run(socket));
    if let Some(turn_state) = turn_state {
        response.headers_mut().insert(TURN_STATE, turn_state);
    }
    response
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Unknown,
    Websocket,
    Http,
}

/// How a turn ended.
enum Flow {
    Next,
    End,
}

/// A terminal turn failure and how the downstream connection learns about it.
struct Failure {
    status: u16,
    text: String,
    /// The raw upstream `error` event, sent as-is when exposed.
    payload: Option<String>,
    replay: bool,
}

impl Failure {
    fn from_exec(e: &ExecError) -> Self {
        Self {
            status: e.status,
            text: String::from_utf8_lossy(&e.body).into_owned(),
            payload: None,
            replay: e.is_replay_required(),
        }
    }

    fn from_dispatch(failure: &dispatch::Failure) -> Self {
        match failure {
            dispatch::Failure::Exec(e) => Self {
                status: classify::response_status(e),
                text: classify::error_text(e),
                payload: None,
                replay: e.is_replay_required(),
            },
            other => Self::local(other.status(), &other.text()),
        }
    }

    fn local(status: u16, text: &str) -> Self {
        Self {
            status,
            text: text.into(),
            payload: None,
            replay: false,
        }
    }
}

struct Connection {
    rt: Arc<Runtime>,
    caller: Caller,
    headers: HeaderMap,
    /// Go `request_path` metadata for payload rules.
    request_path: String,
    /// The upgrade request's raw query (Go reads it from the retained gin context).
    query: String,
    /// The downstream peer (usage records).
    peer: Option<std::net::SocketAddr>,
    /// Execution session id (`passthroughSessionID`).
    session: String,
    /// Tool-cache key (`websocketDownstreamSessionKey`).
    tool_key: String,
    last_request: String,
    last_output: String,
    last_id: String,
    /// Remains set until a generating request commits after a local warm-up.
    pending_prewarm: String,
    pending_calls: Vec<String>,
    pinned: String,
    /// Upstream credential affinity per provider: (credential id, model key).
    pinned_by_provider: HashMap<String, (String, String)>,
    passthrough_model: String,
    mode: Mode,
    upstream_auth: String,
    /// Response steering is configured: the client's frames go through the shared queue.
    steering: bool,
    /// The turn's selected credential runs full duplex (`codexDuplexStream`); its stream
    /// owns the connection's closure.
    duplex: Arc<AtomicBool>,
    /// The turn is a native continuation that attempted its pinned credential, so a
    /// 401/429 makes the client replay over HTTP (1012) rather than close on the loss.
    replays: Arc<AtomicBool>,
    /// The upgrade request's log: upstream capture for every turn and its middleware ID.
    capture: cpa_core::exec::CaptureSink,
    request_id: Option<String>,
    /// The upgrade request's WebSocket timeline, detached before the 101 (None when
    /// request logging is off).
    timeline: Option<crate::request_logging::WebsocketLog>,
    /// The Home pick the session's pooled upstream socket keeps between turns.
    home: Arc<crate::home_session::SessionHome>,
}

/// The downstream reader.
enum Client {
    /// No steering: the connection task reads between turns only (Go's
    /// `conn.ReadMessage` loop), so a turn does not notice the client leaving. The
    /// second field is gorilla's text for how the read ended.
    Direct(SplitStream<WebSocket>, Option<String>),
    /// Steering configured: a reader task fills the queue the duplex shares
    /// (`readResponsesWebsocketInput`). It holds `alive`'s sender, so its exit (the client
    /// went away) is Go's request cancellation.
    Queued {
        frames: cpa_exec::codex::ClientFrames,
        alive: watch::Receiver<()>,
        reader: tokio::task::AbortHandle,
    },
}

impl Client {
    fn queued(stream: SplitStream<WebSocket>, tx: mpsc::Sender<Bytes>, frames: cpa_exec::codex::ClientFrames) -> Self {
        let (alive_tx, alive) = watch::channel(());
        let reader = tokio::spawn(read_input(stream, tx, alive_tx)).abort_handle();
        Self::Queued { frames, alive, reader }
    }

    /// The next text or binary frame; `None` once the client is gone.
    async fn next(&mut self) -> Option<Bytes> {
        match self {
            Self::Direct(stream, ended) => loop {
                let message = stream.next().await;
                // gorilla's `ReadMessage` error: the peer's close frame, else an
                // abnormal closure.
                let close = |code, reason: &str| Some(gorilla_close(code, reason));
                match message {
                    Some(Ok(Message::Text(text))) => return Some(Bytes::from(text)),
                    Some(Ok(Message::Binary(bytes))) => return Some(bytes),
                    // The close ends the read at once, as gorilla's does; the reply is sent
                    // within `TERMINAL_WRITE` when the connection ends.
                    Some(Ok(Message::Close(Some(frame)))) => {
                        *ended = close(frame.code, frame.reason.as_str());
                        return None;
                    }
                    Some(Ok(Message::Close(None))) => {
                        *ended = close(1005, "");
                        return None;
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => {
                        if ended.is_none() {
                            *ended = close(1006, "unexpected EOF");
                        }
                        return None;
                    }
                }
            },
            // A frame queued before the client left would start a turn that the
            // cancellation ends at once; skip it.
            Self::Queued { frames, alive, .. } => tokio::select! {
                biased;
                () = gone(alive) => None,
                frame = async { frames.lock().await.recv().await } => frame,
            },
        }
    }

    /// Why the direct read ended (Go's `wsTerminateErr`); a steering queue that closed
    /// ends the connection without one.
    fn close_text(&self) -> Option<String> {
        match self {
            Self::Direct(_, ended) => ended.clone(),
            Self::Queued { .. } => None,
        }
    }

    /// Resolves once the client went away; never without the reader task.
    async fn gone(&mut self) {
        match self {
            Self::Direct(..) => std::future::pending().await,
            Self::Queued { alive, .. } => gone(alive).await,
        }
    }

    fn stop(&self) {
        if let Self::Queued { reader, .. } = self {
            reader.abort();
        }
    }
}

async fn gone(alive: &mut watch::Receiver<()>) {
    // Nothing is ever sent: `changed` fails once the reader dropped the sender.
    while alive.changed().await.is_ok() {}
}

/// `readResponsesWebsocketInput`: the only downstream reader with steering configured.
/// The bounded queue backpressures the client instead of retaining unlimited input.
async fn read_input(mut stream: SplitStream<WebSocket>, frames: mpsc::Sender<Bytes>, _alive: watch::Sender<()>) {
    while let Some(Ok(message)) = stream.next().await {
        let frame = match message {
            Message::Text(text) => Bytes::from(text),
            Message::Binary(bytes) => bytes,
            // gorilla's `ReadMessage` returns the close error at once: the client is gone,
            // even while its close reply cannot be flushed (the connection task sends it
            // within `TERMINAL_WRITE`).
            Message::Close(_) => return,
            _ => continue,
        };
        if frames.send(frame).await.is_err() {
            return;
        }
    }
}

/// A turn's stream once its first event arrived.
struct Started {
    first: Option<Bytes>,
    stream: ExecStream,
}

/// Per-turn facts the attempt and forward steps share.
struct TurnCtx {
    native: bool,
    requires_current: bool,
    native_request: bool,
    pinned_attempted: bool,
    last_attempted: String,
    attempted_mode: Mode,
    preserve_output: bool,
    tool_turn: Option<TurnCache>,
}

impl TurnCtx {
    /// `replayPinnedAuthFailure`: a continuation cannot rotate credentials in place.
    fn suppress(&self, status: u16) -> bool {
        self.native && self.requires_current && self.pinned_attempted && matches!(status, 401 | 429)
    }
}

enum Forwarded {
    Completed {
        output: String,
        id: String,
        pending: Vec<String>,
    },
    Suppressed,
    End,
}

impl Connection {
    fn new(rt: Arc<Runtime>, caller: Caller, headers: HeaderMap) -> Self {
        Self {
            tool_key: tools::session_key(&headers),
            rt,
            caller,
            headers,
            request_path: String::new(),
            query: String::new(),
            peer: None,
            session: uuid::Uuid::new_v4().to_string(),
            last_request: String::new(),
            last_output: "[]".into(),
            last_id: String::new(),
            pending_prewarm: String::new(),
            pending_calls: Vec::new(),
            pinned: String::new(),
            pinned_by_provider: HashMap::new(),
            passthrough_model: String::new(),
            mode: Mode::Unknown,
            upstream_auth: String::new(),
            steering: false,
            duplex: Arc::default(),
            replays: Arc::default(),
            capture: Default::default(),
            request_id: None,
            timeline: None,
            home: Arc::default(),
        }
    }

    async fn run(mut self, socket: WebSocket) {
        let _tools = Retained::new(self.tool_key.clone());
        let (inner, stream) = socket.split();
        let mut sink = Sink {
            inner,
            timeline: self.timeline.take(),
            terminated: None,
        };
        let mut client = if CodexExecutor::response_steering_configured(&self.rt.config()) {
            let (tx, rx) = mpsc::channel(STEERING_QUEUE);
            let frames = Arc::new(tokio::sync::Mutex::new(rx));
            let store = self.rt.store().clone();
            // `WithWebsocketAuthCheck`. ponytail: Go falls back to the execution session's
            // snapshot of a credential removed since; a missing credential counts as enabled.
            let enabled = move |id: &str| store.get(id).is_none_or(|c| !c.disabled);
            self.rt
                .executors
                .codex
                .attach_steering(&self.session, SteeringInput::new(frames.clone(), enabled));
            self.steering = true;
            Client::queued(stream, tx, frames)
        } else {
            Client::Direct(stream, None)
        };
        let (duplex, replays) = (self.duplex.clone(), self.replays.clone());
        // Fused: a loss ignored during a duplex turn never fires again.
        let mut lost = Box::pin(self.rt.executors.session_closed(&self.session).fuse());
        loop {
            let frame = tokio::select! {
                frame = client.next() => frame,
                error = &mut lost => {
                    close_for_upstream_loss(&mut sink, &error).await;
                    break;
                }
            };
            let Some(frame) = frame else {
                // Go's read error ends the connection; with steering the queue just closes.
                if let Some(reason) = client.close_text() {
                    sink.terminate(&reason);
                }
                break;
            };
            sink.event("request", &frame);
            let payload = String::from_utf8_lossy(&frame).into_owned();
            // The turn wins ties so frames already received reach the client before a
            // simultaneous upstream-loss signal closes the connection. A duplex turn owns
            // closure: it drains acknowledgements and pending events in order first. A
            // client that went away cancels the turn (Go's reader cancels the request).
            let flow = {
                let turn = self.turn(&mut sink, payload);
                tokio::pin!(turn);
                tokio::select! {
                    biased;
                    flow = &mut turn => Ok(flow),
                    error = &mut lost => {
                        // A pinned continuation's own 401/429 lost the socket: the turn
                        // sends the replay close (1012) instead.
                        let replay = replays.load(Ordering::Acquire) && matches!(error.status, 401 | 429);
                        if duplex.load(Ordering::Acquire) || replay {
                            tokio::select! {
                                biased;
                                flow = &mut turn => Ok(flow),
                                () = client.gone() => Err(None),
                            }
                        } else {
                            Err(Some(error))
                        }
                    }
                    () = client.gone() => Err(None),
                }
            };
            // A finished turn wakes idle work such as the heap trim (one atomic load).
            cpa_common::idle::activity();
            match flow {
                Ok(Flow::Next) => {}
                Ok(Flow::End) => break,
                Err(Some(error)) => {
                    close_for_upstream_loss(&mut sink, &error).await;
                    break;
                }
                // The client left mid-turn: Go's forward returns the cancelled context.
                Err(None) => {
                    sink.terminate("context canceled");
                    break;
                }
            }
        }
        // The reader holds the other half: stop it so the socket closes now.
        client.stop();
        self.rt.executors.close_session(&self.session);
        // The kept Home pick ends with the connection, before timeline delivery can wait.
        self.home.close();
        sink.finish(client).await;
    }

    async fn turn(&mut self, socket: &mut Sink, payload: String) -> Flow {
        let cfg = self.rt.config();
        let explicit_model = field(&payload, "model");
        let mut request_model = explicit_model.clone();
        if request_model.is_empty() {
            request_model.clone_from(&self.passthrough_model);
        }
        if request_model.is_empty() {
            request_model = field(&self.last_request, "model");
        }
        self.revalidate_pin(&request_model);

        let mut use_upstream_ws = self.uses_upstream_websocket(&request_model);
        if let Some(pinned) = self.credential(&self.pinned)
            && CodexExecutor::upstream_websocket(&pinned)
        {
            use_upstream_ws = matches!(pinned.provider.to_ascii_lowercase().as_str(), "codex" | "xai");
        }
        let native = self.mode == Mode::Websocket
            && use_upstream_ws
            && !self.pinned.is_empty()
            && self.pinned == self.upstream_auth;
        let previous_id = field(&payload, "previous_response_id");
        let requires_current = !previous_id.is_empty() || field(&payload, "type") == APPEND;
        if self.mode == Mode::Websocket && !native && requires_current {
            // A continuation of upstream state cannot move to another transport.
            close_with_code(socket, CLOSE_SERVICE_RESTART, "upstream requires HTTP replay").await;
            socket.terminate(&replay_text());
            return Flow::End;
        }
        if !explicit_model.is_empty() && !use_upstream_ws {
            self.passthrough_model.clear();
        }
        // ponytail: Go also tracks observed compaction per route/plugin; with only Codex
        // credentials on this route the pinned/all-Codex rule below gives the same answer.
        let allow_bypass = !native
            && match self.credential(&self.pinned) {
                Some(pinned) => pinned.provider.eq_ignore_ascii_case("codex"),
                None => {
                    let available = self.available(&request_model);
                    !available.is_empty() && available.iter().all(|c| c.provider.eq_ignore_ascii_case("codex"))
                }
            };

        let is_prewarm = !use_upstream_ws && requests::is_local_prewarm(&payload);
        let kind = field(&payload, "type");
        let replacement = |payload: &str, last: &str| {
            let input = gjson::get(payload, "input");
            if input.exists() && input.kind() != gjson::Kind::Array {
                return Err(WsError::bad("websocket request requires array field: input"));
            }
            requests::normalize_create(&requests::transcript_replacement(payload, last))
        };
        let normalized = if !self.pending_prewarm.is_empty() && !previous_id.is_empty() {
            if previous_id != self.pending_prewarm {
                Err(WsError::previous_not_found())
            } else {
                requests::prewarm_followup(&payload, &self.last_request)
            }
        } else if (is_prewarm && previous_id.is_empty()) || (!self.pending_prewarm.is_empty() && kind == CREATE) {
            replacement(&payload, &self.last_request)
        } else if native {
            requests::normalize_passthrough(&payload, &request_model).map(|r| (r, String::new()))
        } else if self.last_request.is_empty() && !previous_id.is_empty() {
            Err(WsError::previous_not_found())
        } else {
            requests::normalize(
                &payload,
                &self.last_request,
                &self.last_output,
                &self.last_id,
                &self.pending_calls,
                false,
                allow_bypass,
            )
        };
        let (mut request, updated) = match normalized {
            Ok(pair) => pair,
            Err(error) => {
                // Request-shape errors keep the connection open.
                let frame = error_payload(error.status, &error.message);
                return match send_text(socket, frame).await {
                    true => Flow::Next,
                    false => Flow::End,
                };
            }
        };
        // Go prepares multi-agent v2 tools and orphan delegation outputs here, before the
        // prewarm and dispatch decisions.
        let client = cpa_common::codex_client::Settings::for_responses_handler(&cfg);
        // The spawn_agent model list comes from the Codex client catalog.
        // Cost: one header check, then one `Once` check, per turn.
        if cpa_common::codex_client::multi_agent_client(&self.headers, client.optimize_multi_agent_v2) {
            crate::model_updater::codex_client_catalog_wanted(&self.rt);
        }
        if client.optimize_multi_agent_v2 || client.orphan_delegation {
            let prepared =
                cpa_common::codex_client::prepare_responses_request(&self.headers, request.as_bytes(), &client);
            request = String::from_utf8(prepared).unwrap_or(request);
        }

        if is_prewarm {
            request = delete(&request, "generate");
            self.last_request = delete(&updated, "generate");
            self.last_output = "[]".into();
            self.last_id.clear();
            self.pending_calls.clear();
            let id = format!("resp_prewarm_{}", uuid::Uuid::new_v4());
            let created_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            for frame in requests::prewarm_payloads(&request, &id, created_at) {
                if !send_text(socket, frame).await {
                    return Flow::End;
                }
            }
            self.pending_prewarm = id;
            return Flow::Next;
        }

        let mut ctx = TurnCtx {
            native,
            requires_current,
            native_request: lite_request(&payload, &self.headers),
            pinned_attempted: false,
            last_attempted: self.pinned.clone(),
            attempted_mode: Mode::Http,
            preserve_output: false,
            tool_turn: None,
        };
        let next_last_request = if native {
            let model = field(&request, "model");
            if !model.is_empty() {
                self.passthrough_model = model;
            }
            self.last_request.clone()
        } else {
            let (repaired, tool_turn) = tools::prepare_fallback_turn(&self.tool_key, request);
            request = repaired;
            ctx.tool_turn = tool_turn;
            request.clone()
        };
        let model = gjson::get(&request, "model").str().to_owned();
        let started = self.attempt(&request, &model, &mut ctx).await;
        let forwarded = match started {
            Ok(started) => self.forward(socket, started, &cfg, &mut ctx).await,
            Err(error) => {
                let failure = Failure::from_dispatch(&error);
                // A pinned continuation's credential failure also lost the session's
                // socket; the client still needs the replay signal, not the loss close.
                if ctx.suppress(failure.status) {
                    Forwarded::Suppressed
                } else if let Some(loss) = self.upstream_loss() {
                    close_for_upstream_loss(socket, &loss).await;
                    Forwarded::End
                } else {
                    close_for_failure(socket, &failure).await;
                    Forwarded::End
                }
            }
        };
        let (output, id, pending) = match forwarded {
            Forwarded::Completed { output, id, pending } => (output, id, pending),
            Forwarded::Suppressed => {
                close_with_code(socket, CLOSE_SERVICE_RESTART, "upstream requires HTTP replay").await;
                socket.terminate(&replay_text());
                return Flow::End;
            }
            Forwarded::End => return Flow::End,
        };

        if let Some(tool_turn) = ctx.tool_turn.take() {
            tool_turn.commit();
        }
        self.pending_prewarm.clear();
        self.mode = ctx.attempted_mode;
        if self.mode == Mode::Websocket {
            self.upstream_auth.clone_from(&ctx.last_attempted);
            if !ctx.last_attempted.is_empty() {
                self.remember_pin(&ctx.last_attempted, &model);
            }
            self.passthrough_model = model;
            self.last_request.clear();
            self.last_output = "[]".into();
            self.last_id.clear();
            self.pending_calls.clear();
        } else {
            self.upstream_auth.clear();
            self.last_request = next_last_request;
            self.last_output = output;
            self.last_id = id.trim().to_owned();
            self.pending_calls = pending;
        }
        Flow::Next
    }

    fn credential(&self, id: &str) -> Option<Arc<Credential>> {
        if id.is_empty() {
            return None;
        }
        self.rt.store().get(id)
    }

    /// `responsesWebsocketProviderSetForModel`: providers that registered the model
    /// (`auto` resolved, thinking suffix stripped) and the model key.
    fn providers(&self, model: &str) -> (Vec<String>, String) {
        let registry = self.rt.registry();
        let resolved = resolve_auto(&self.rt, &registry, model);
        let key = canonical_model(&resolved).to_owned();
        let mut providers = registry.providers(&key);
        if providers.is_empty() && key != resolved {
            providers = registry.providers(&resolved);
        }
        let key = if key.is_empty() {
            resolved.trim().to_owned()
        } else {
            key
        };
        (providers, key)
    }

    /// Go's disconnect notifier closes the client as soon as the session's upstream socket
    /// is lost, ahead of the turn's own error (whose 1009 body is a fixed message rather
    /// than the upstream's close reason). A duplex stream owns its closure instead.
    fn upstream_loss(&self) -> Option<ExecError> {
        if self.duplex.load(Ordering::Acquire) {
            return None;
        }
        self.rt.executors.codex.session_loss(&self.session)
    }

    /// `responsesWebsocketAuthAvailableForModel`: enabled and not cooling for the model.
    fn usable(&self, credential: &Credential, key: &str) -> bool {
        !credential.disabled && !self.rt.store().blocked(credential, key)
    }

    /// `responsesWebsocketAvailableAuthsForModel`.
    fn available(&self, model: &str) -> Vec<Arc<Credential>> {
        let (providers, key) = self.providers(model);
        if providers.is_empty() {
            return Vec::new();
        }
        let registry = self.rt.registry();
        self.rt
            .store()
            .snapshot()
            .into_iter()
            .filter(|c| {
                providers.contains(&provider_key(c))
                    && (key.is_empty() || registry.client_supports(&c.id, &key))
                    && self.usable(c, &key)
            })
            .collect()
    }

    /// `responsesWebsocketUsesUpstreamWebsocketPassthrough`: every credential for the
    /// model keeps upstream state on a socket.
    fn uses_upstream_websocket(&self, model: &str) -> bool {
        if model.trim().is_empty() {
            return false;
        }
        let available = self.available(model);
        !available.is_empty() && available.iter().all(|c| self.rt.executors.session_upstream(c))
    }

    /// `responsesWebsocketPinnedAuthMatchesModel` for credentials the store still holds.
    fn pin_matches(&self, credential: &Credential, model: &str) -> bool {
        let (providers, key) = self.providers(model);
        providers.contains(&provider_key(credential))
            && self.usable(credential, &key)
            && self.rt.registry().client_supports(&credential.id, &key)
    }

    /// Drops a pin whose credential no longer serves the model, or restores the pin
    /// remembered for the model's only provider.
    fn revalidate_pin(&mut self, model: &str) {
        if let Some(pinned) = self.credential(&self.pinned) {
            let current = self
                .pinned_by_provider
                .get(&provider_key(&pinned))
                .is_some_and(|(id, _)| *id == self.pinned);
            if !current || !self.pin_matches(&pinned, model) {
                self.pinned.clear();
            }
        } else {
            self.pinned.clear();
        }
        if !self.pinned.is_empty() {
            return;
        }
        if let [provider] = self.providers(model).0.as_slice() {
            let remembered = self.pinned_by_provider.get(provider).cloned();
            match remembered.and_then(|(id, _)| self.credential(&id).map(|c| (id, c))) {
                Some((id, c)) if self.pin_matches(&c, model) => self.pinned = id,
                _ => {
                    self.pinned_by_provider.remove(provider);
                }
            }
        }
    }

    /// `rememberPinnedAuth`.
    fn remember_pin(&mut self, id: &str, model: &str) {
        let Some(credential) = self.credential(id) else {
            return;
        };
        self.pinned = id.to_owned();
        let provider = provider_key(&credential);
        if !provider.is_empty() {
            let key = self.providers(model).1;
            self.pinned_by_provider.insert(provider, (id.to_owned(), key));
        }
    }

    /// One turn through the shared dispatch loop (`ExecuteStreamWithAuthManager` with the
    /// pinned auth, the selected-auth callback and the execution session), including
    /// bootstrap retries. Returns once the stream yielded its first event.
    async fn attempt(&self, request: &str, model: &str, ctx: &mut TurnCtx) -> Result<Started, dispatch::Failure> {
        /// What `WithSelectedAuthIDCallback` learned; the last call is the serving one.
        struct Selected {
            last: String,
            pinned_attempted: bool,
            mode: Mode,
            preserve_output: bool,
        }
        let selected = Arc::new(std::sync::Mutex::new(Selected {
            last: ctx.last_attempted.clone(),
            pinned_attempted: false,
            mode: ctx.attempted_mode,
            preserve_output: ctx.preserve_output,
        }));
        self.duplex.store(false, Ordering::Release);
        self.replays.store(false, Ordering::Release);
        let on_selected: dispatch::OnSelected = {
            let (selected, rt, pinned) = (selected.clone(), self.rt.clone(), self.pinned.clone());
            let native_request = ctx.native_request;
            let continuation = ctx.native && ctx.requires_current;
            let (duplex, replays, steering) = (self.duplex.clone(), self.replays.clone(), self.steering);
            Box::new(move |credential: &Credential| {
                let mut s = selected.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                s.last.clone_from(&credential.id);
                s.pinned_attempted |= !pinned.is_empty() && credential.id == pinned;
                replays.store(continuation && s.pinned_attempted, Ordering::Release);
                s.mode = if rt.executors.session_upstream(credential) {
                    Mode::Websocket
                } else {
                    Mode::Http
                };
                let codex = credential.provider.eq_ignore_ascii_case("codex");
                s.preserve_output = native_request && codex;
                // OAuth-only steering leaves API keys in normal mode.
                let full_duplex = steering
                    && codex
                    && s.mode == Mode::Websocket
                    && CodexExecutor::response_steering(credential, &rt.config());
                duplex.store(full_duplex, Ordering::Release);
            })
        };
        let turn = Arc::new(dispatch::SessionTurn {
            session: ExecSession {
                id: self.session.clone(),
                continuation: ctx.native && ctx.requires_current,
                lease: None,
            },
            pinned: (!self.pinned.is_empty()).then(|| self.pinned.clone()),
            on_selected: Some(on_selected),
            home: Some(self.home.clone()),
        });
        let call = dispatch::Call {
            entry: Format::OpenAIResponse,
            response: Format::OpenAIResponse,
            operation: Operation::Generate,
            model: model.to_owned(),
            body: Bytes::from(request.to_owned()),
            stream: true,
            alt: None,
            headers: self.headers.clone(),
            caller: self.caller.clone(),
            forced_provider: None,
            selection_model: None,
            execution_session: Some(self.session.clone()),
            request_path: self.request_path.clone(),
            peer: self.peer,
            turn: Some(turn),
            media: None,
        };
        // The upgrade's log captures every turn's upstream traffic under its request ID.
        let trace = dispatch::Trace::with_request_id(self.request_id.clone()).with_capture(self.capture.clone());
        let result = crate::plugins::execution::with_query(
            &self.query,
            dispatch::run_with_bootstrap_retries(&self.rt, call, &trace),
        )
        .await;
        {
            let s = selected.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            ctx.last_attempted.clone_from(&s.last);
            ctx.pinned_attempted = s.pinned_attempted;
            ctx.attempted_mode = s.mode;
            ctx.preserve_output = s.preserve_output;
        }
        match result? {
            dispatch::Done::Stream { first, rest, .. } => Ok(Started { first, stream: rest }),
            dispatch::Done::Buffered { body, .. } => Ok(Started {
                first: Some(body),
                stream: futures_util::stream::empty().boxed(),
            }),
        }
    }

    /// `forwardResponsesWebsocket`.
    async fn forward(&self, socket: &mut Sink, started: Started, cfg: &Config, ctx: &mut TurnCtx) -> Forwarded {
        let Started { first, stream } = started;
        // `stream` completes its lease as it ends (dispatch).
        let mut stream = futures_util::stream::iter(first.map(Ok)).chain(stream).boxed();
        let keepalive = keepalive_interval(cfg);
        let mut deadline = keepalive.map(|k| tokio::time::Instant::now() + k);
        let mut turn = Turn::default();
        let (mut completed, mut completed_output, mut completed_id) = (false, String::from("[]"), String::new());
        // A duplex stream takes the client's later frames from the shared queue and owns
        // the connection's end.
        let duplex = self.duplex.load(Ordering::Acquire);
        let mut response_started = false;
        loop {
            let item = tokio::select! {
                biased;
                item = stream.next() => item,
                () = sleep_until(deadline) => {
                    if let Err(error) = socket.inner.send(Message::Ping(Bytes::new())).await {
                        socket.terminate(&error.to_string());
                        return Forwarded::End;
                    }
                    deadline = keepalive.map(|k| tokio::time::Instant::now() + k);
                    continue;
                }
            };
            let chunk = match item {
                // A duplex stream ends with its socket, not with a response: close the
                // connection without an error.
                None if duplex => {
                    socket.terminate(CLOSE_SENT);
                    return Forwarded::End;
                }
                None if completed => {
                    return Forwarded::Completed {
                        output: completed_output,
                        id: completed_id,
                        pending: turn.pending(),
                    };
                }
                // `stream closed before response.completed`: 408, closed silently.
                // Closed without an error event or a timeline reason.
                None => {
                    socket.terminate(CLOSE_SENT);
                    return Forwarded::End;
                }
                Some(Err(error)) => {
                    let failure = Failure::from_exec(&error);
                    // Replay before the session-loss close, as at the turn's start.
                    if ctx.suppress(failure.status) {
                        return Forwarded::Suppressed;
                    }
                    if let Some(loss) = self.upstream_loss() {
                        close_for_upstream_loss(socket, &loss).await;
                        return Forwarded::End;
                    }
                    close_for_failure(socket, &failure).await;
                    return Forwarded::End;
                }
                Some(Ok(chunk)) => chunk,
            };
            deadline = keepalive.map(|k| tokio::time::Instant::now() + k);
            for mut payload in payloads_from_chunk(&chunk) {
                let kind = gjson::get(&payload, "type").str().to_owned();
                if kind == "response.created" {
                    response_started = true;
                    completed = false;
                    turn.reset();
                }
                turn.collect(&payload);
                let completion = kind == "response.completed" || kind == "response.done";
                if completion && !ctx.preserve_output {
                    payload = turn.restore_completion(payload);
                }
                match ctx.tool_turn.as_mut() {
                    Some(tool_turn) => tool_turn.record_response(&payload),
                    None => tools::record_calls(&self.tool_key, &payload),
                }
                turn.track_pending(&payload);
                // In duplex mode an error event after `response.created` is recoverable:
                // the client may correct the request on this socket. Stream errors still
                // close it.
                if kind == "error" && !(response_started && duplex) {
                    // `responsesWebsocketErrorMessageFromPayload`.
                    let mut status = gjson::get(&payload, "status").i64();
                    if status <= 0 {
                        status = gjson::get(&payload, "status_code").i64();
                    }
                    let status = u16::try_from(status).ok().filter(|s| *s > 0).unwrap_or(500);
                    if ctx.suppress(status) {
                        return Forwarded::Suppressed;
                    }
                    let failure = Failure {
                        status,
                        text: payload.trim().to_owned(),
                        payload: Some(payload.trim().to_owned()),
                        replay: false,
                    };
                    close_for_failure(socket, &failure).await;
                    return Forwarded::End;
                }
                if completion {
                    completed = true;
                    completed_output = turn.completed_output(&payload);
                    completed_id = field(&payload, "response.id");
                }
                if !send_text(socket, payload).await {
                    return Forwarded::End;
                }
            }
        }
    }
}

/// Go `getRequestDetails` `auto` resolution, keeping the thinking suffix.
fn resolve_auto(rt: &Runtime, registry: &crate::registry::Registry, model: &str) -> String {
    let base = match model.rfind('(') {
        Some(i) if model.ends_with(')') => &model[..i],
        _ => model,
    };
    if base != "auto" {
        return model.to_owned();
    }
    let first = registry
        .resolve_auto(|client, m| rt.suspension(client, m))
        .unwrap_or_else(|| "auto".into());
    format!("{first}{}", &model[base.len()..])
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// `requests.streaming.keepalive-seconds`: WebSocket pings while a turn streams; off by
/// default.
fn keepalive_interval(cfg: &Config) -> Option<Duration> {
    let seconds = ["requests", "streaming", "keepalive-seconds"]
        .iter()
        .try_fold(&cfg.document, |v, k| v.get(*k))
        .and_then(serde_yaml_ng::Value::as_i64)
        .unwrap_or(0);
    (seconds > 0).then(|| Duration::from_secs(seconds as u64))
}

/// `util.IsCodexResponsesLiteRequest`.
fn lite_request(payload: &str, headers: &HeaderMap) -> bool {
    let header = headers
        .get("x-openai-internal-codex-responses-lite")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if header.trim().eq_ignore_ascii_case("true") {
        return true;
    }
    let value = gjson::get(
        payload,
        "client_metadata.ws_request_header_x_openai_internal_codex_responses_lite",
    );
    value.kind() == gjson::Kind::True
        || (value.kind() == gjson::Kind::String && value.str().trim().eq_ignore_ascii_case("true"))
}

/// `writeResponsesWebsocketPayload`: the frame joins the timeline, then goes out.
async fn send_text(socket: &mut Sink, payload: String) -> bool {
    socket.event("response", payload.as_bytes());
    match socket.inner.send(Message::Text(payload.into())).await {
        Ok(()) => true,
        Err(error) => {
            // ponytail: the write error's text is axum's, not net's.
            socket.terminate(&error.to_string());
            false
        }
    }
}

async fn close_with_code(socket: &mut Sink, code: u16, reason: &str) {
    let reason = requests::truncate_reason(reason, CLOSE_REASON_MAX);
    let close = Message::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    }));
    let _ = tokio::time::timeout(TERMINAL_WRITE, socket.inner.send(close)).await;
}

/// `websocketClosePayloadForUpstreamError`: replay and message-too-big failures map to
/// close codes the client acts on.
fn close_code(failure: &Failure) -> Option<(u16, String)> {
    if failure.replay {
        return Some((CLOSE_SERVICE_RESTART, "upstream requires HTTP replay".into()));
    }
    if failure.status == 413 && gjson::get(&failure.text, "error.code").str() == "message_too_big" {
        // An upstream close keeps gorilla's `CloseError.Text` verbatim; a mapped 413's
        // message is trimmed.
        let raw = gjson::get(&failure.text, "close_reason");
        let reason = if raw.kind() == gjson::Kind::String {
            raw.str().to_owned()
        } else {
            gjson::get(&failure.text, "error.message").str().trim().to_owned()
        };
        let reason = if reason.is_empty() {
            "message too big".into()
        } else {
            reason
        };
        return Some((CLOSE_MESSAGE_TOO_BIG, reason));
    }
    None
}

/// `shouldExposeResponsesUpstreamError`: only request faults reach the client; credential,
/// quota and transport failures close silently so the client reconnects and resends.
// ponytail: Go also exposes terminal upstream-auth errors; no executor reports them yet.
fn exposed(failure: &Failure) -> bool {
    classify::is_request_fault(failure.status, &failure.text)
}

/// `closeForUpstreamError` then `writeResponsesWebsocketTerminalError`: a close code,
/// the error event and a TCP close, or a bare TCP close. The socket is dropped by the
/// caller right after. The timeline gets the exposed event, or the hidden reason as a
/// disconnect, and the connection ends with gorilla's "close sent".
async fn close_for_failure(socket: &mut Sink, failure: &Failure) {
    if let Some((code, reason)) = close_code(failure) {
        close_with_code(socket, code, &reason).await;
    } else if exposed(failure) {
        let payload = failure
            .payload
            .clone()
            .unwrap_or_else(|| error_payload(failure.status, &failure.text));
        socket.event("response", payload.as_bytes());
        let _ = tokio::time::timeout(TERMINAL_WRITE, socket.inner.send(Message::Text(payload.into()))).await;
    } else {
        // Keeps the upstream reason although the client only sees the connection close.
        socket.event("disconnect", failure.text.as_bytes());
    }
    socket.terminate(CLOSE_SENT);
}

/// `closeForUpstreamDisconnect`: the session's upstream socket was lost. Go's notifier
/// writes nothing to the timeline.
// ponytail: Go's final disconnect is whatever its blocked read returns once the notifier
// closed the socket; this logs gorilla's "close sent".
async fn close_for_upstream_loss(socket: &mut Sink, error: &ExecError) {
    let failure = Failure::from_exec(error);
    if let Some((code, reason)) = close_code(&failure) {
        close_with_code(socket, code, &reason).await;
    } else if exposed(&failure) {
        let payload = error_payload(failure.status, &failure.text);
        let _ = tokio::time::timeout(TERMINAL_WRITE, socket.inner.send(Message::Text(payload.into()))).await;
    }
    socket.terminate(CLOSE_SENT);
}

/// Go's `UpstreamWebsocketReplayRequiredError` text, the reason a replay close ends the
/// connection.
fn replay_text() -> String {
    String::from_utf8_lossy(&ExecError::replay_required().body).into_owned()
}

/// gorilla `CloseError.Error()`.
fn gorilla_close(code: u16, reason: &str) -> String {
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
    let mut text = format!("websocket: close {code}{name}");
    if !reason.is_empty() {
        text.push_str(": ");
        text.push_str(reason);
    }
    text
}

#[cfg(test)]
#[path = "websocket_tests.rs"]
mod tests;
