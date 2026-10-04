//! Response steering: a full-duplex Codex WebSocket (codex_websockets_duplex.go).
//!
//! With `response-steering` on, a downstream Responses WebSocket whose turn runs on a
//! Codex credential in WebSocket mode keeps that upstream socket for the rest of the
//! connection. Once the first `response.created` reached the client, the client's later
//! frames go straight to the socket: `response.steer` raw, `response.create` and
//! `response.append` shaped like turns. A response terminal is not a stream terminal:
//! accepted steering may start an automatic successor or wait for tool results. The
//! stream ends with the socket, and the socket with the stream. There is no redial,
//! credential switch, local acknowledgement or replay here; only upstream owns a
//! steering submission.
//!
//! Go runs a writer and a reader goroutine around shared state. Here one task owns the
//! state and selects fairly between upstream events, client frames and delivery; a
//! separate writer task owns the socket's writes, so a write blocked in the network never
//! stops upstream events (Go's writer blocking in `writeCodexWebsocketMessage`). Writer
//! waits (`waitFor`, queued creates) become state the task re-checks after every step.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use bytes::Bytes;
use cpa_core::credential::Credential;
use cpa_core::exec::{ExecError, ExecRequest, ExecStream, FailureScope};
use futures_util::{FutureExt, StreamExt};
use tokio::sync::{OwnedMutexGuard, mpsc};

use super::{Read, Replay, Turn, Upstream, request_frame, transport, ws_error};
use crate::codex_json::{set_raw, set_str};
use crate::codex_request::{self as request, Call, Settings, View};
use crate::codex_response as response;

/// Bound on queued and outstanding `response.create` requests.
const MAX_PENDING: usize = 16;
/// Responses whose settings are retained for automatic successors and appends.
const RETAINED_RESPONSES: usize = 16;
/// Frames accepted for the writer and not yet written. Client frames are read only while
/// there is room, so a stalled upstream write backpressures the client as Go's does.
const WRITE_QUEUE: usize = 16;
/// Local items (events, rejections) waiting for the consumer. Upstream is read only when
/// none wait, as Go's reader blocks on its unbuffered `out`.
const OUTBOX: usize = 16;

type Enabled = dyn Fn(&str) -> bool + Send + Sync;

/// The queue of a downstream connection's client frames, in arrival order. A bounded
/// channel backpressures the client; the connection's own loop reads it between turns.
pub type ClientFrames = Arc<tokio::sync::Mutex<mpsc::Receiver<Bytes>>>;

/// A downstream connection's later client frames (Go `WebsocketInput`) and the live
/// account check for its bound credential (Go `WebsocketAuthCheck`).
pub struct SteeringInput {
    frames: ClientFrames,
    enabled: Box<Enabled>,
}

impl SteeringInput {
    /// `frames` is the connection's frame queue, shared with its loop as Go hands both
    /// the same channel; a duplex turn holds it for the rest of the connection.
    /// `enabled(credential_id)` says whether the bound credential may still send.
    pub fn new(frames: ClientFrames, enabled: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        Self {
            frames,
            enabled: Box::new(enabled),
        }
    }
}

/// What the duplex reads of one request's settings (Go `codexWebsocketPrepared`).
#[derive(Clone, Default)]
pub(super) struct Prepared {
    /// Raw `instructions` of the shaped body (Go `clientBody`).
    instructions: Option<String>,
    /// Raw `instructions` of the client payload (Go `originalPayload`); retained
    /// response settings drop it.
    original_instructions: Option<String>,
    /// The multi-agent v2 optimization renamed the `collaboration` namespace.
    optimized: bool,
    /// The request already used the upstream names (`multiAgentV2Conflict`).
    conflict: bool,
    /// Native Codex request: completion output stays as upstream sent it.
    native: bool,
    /// The request's reasoning replay scope (`replayScope`).
    replay: Replay,
}

impl Prepared {
    pub(super) fn new(
        body: &str,
        original: &[u8],
        optimized: bool,
        conflict: bool,
        native: bool,
        replay: Replay,
    ) -> Self {
        let raw = |json: &str| {
            let value = gjson::get(json, "instructions");
            value.exists().then(|| value.json().to_owned())
        };
        Self {
            instructions: raw(body),
            original_instructions: raw(&String::from_utf8_lossy(original)),
            optimized,
            conflict,
            native,
            replay,
        }
    }

    /// The settings a response retains (Go keeps `reasoning` and `instructions`, never
    /// request history or headers; `reasoning` only feeds per-response usage here).
    fn retained(&self) -> Self {
        Self {
            original_instructions: None,
            ..self.clone()
        }
    }

    fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref().or(self.original_instructions.as_deref())
    }
}

/// The duplex stream's state: Go's writer and reader goroutines' shared metadata.
pub(super) struct Duplex {
    turn: Turn,
    frames: OwnedMutexGuard<mpsc::Receiver<Bytes>>,
    input: Arc<SteeringInput>,
    /// The bootstrap request: the template for later creates (Go `req`, `opts`).
    req: ExecRequest,
    credential: Credential,
    proxy: crate::proxy::Proxy,
    cpa_session: Option<String>,
    settings: Settings,
    exec_session: String,
    initial: Prepared,
    /// The bootstrap request's client model (Go `initial.originalPayload` `model`).
    initial_model: String,
    /// Creates sent upstream and not yet started or rejected, oldest first.
    pending: VecDeque<Prepared>,
    /// Parents of steers written and not yet acknowledged.
    unacknowledged: Vec<String>,
    /// Accepted steer id to parent response.
    accepted: HashMap<String, String>,
    current: Prepared,
    response_id: String,
    response_settings: HashMap<String, Prepared>,
    response_order: VecDeque<String>,
    /// Settings pinned by in-flight steering, independent of the retention window.
    steering_settings: HashMap<String, Prepared>,
    /// The response waiting for required input (`response.steer.pending`).
    waiting_parent: String,
    /// Upstream started a successor on its own.
    automatic_active: bool,
    /// Client creates waiting until steering settles.
    queued_creates: VecDeque<String>,
    /// A steer whose parent settings are unknown waits for pending creates to start.
    deferred_steer: Option<String>,
    /// Client frames are read only after the first `response.created` was delivered.
    input_ready: bool,
    first_response: bool,
    response_active: bool,
    /// Items for the consumer, in order; `out` takes one at a time.
    outbox: VecDeque<Result<Bytes, ExecError>>,
    out: mpsc::Sender<Result<Bytes, ExecError>>,
    /// The writer task's queue and handle; the task ends with the first write error.
    writes: mpsc::Sender<Outgoing>,
    writer: tokio::task::JoinHandle<Result<(), ExecError>>,
    done: bool,
}

impl Drop for Duplex {
    /// The socket ends with the stream; the handler already knows (no notify). Closing
    /// it releases a writer blocked in the network, which then stops (Go joins it).
    fn drop(&mut self) {
        self.turn
            .invalidate(&transport("codex websockets executor: duplex closed"), false);
        self.writer.abort();
    }
}

/// One frame for the writer task.
struct Outgoing {
    frame: String,
    /// A create's multi-agent v2 state `(optimized, conflict)`; steers carry none.
    namespace: Option<(bool, bool)>,
}

/// The network half of Go's writer goroutine: frames in order. A create updates the
/// socket's namespace state only when it reaches the writer (`setMultiAgentV2Optimized`),
/// so a create still queued behind a blocked write cannot change how the running
/// response's events are restored. Then the live account check (`WebsocketAuthEnabled`)
/// right before `writeCodexWebsocketMessage`.
async fn write_loop(
    conn: Arc<Upstream>,
    input: Arc<SteeringInput>,
    credential: String,
    wire: crate::codex_capture::Wire,
    mut queue: mpsc::Receiver<Outgoing>,
) -> Result<(), ExecError> {
    while let Some(Outgoing { frame, namespace }) = queue.recv().await {
        if let Some((optimized, conflict)) = namespace {
            conn.note_multi_agent(optimized, conflict);
        }
        if !(input.enabled)(&credential) {
            return Err(local("websocket credential is no longer enabled"));
        }
        wire.ws_frame(&conn.target.url, frame.as_bytes());
        conn.send(frame).await?;
    }
    Ok(())
}

/// A failure of the bound socket: request-scoped, so the credential is not cooled
/// (Go `codexDuplexConnectionError`). Status and body are kept for the handler.
fn connection(mut error: ExecError) -> ExecError {
    error.scope = FailureScope::Request;
    error
}

fn local(message: &str) -> ExecError {
    connection(transport(message))
}

enum Step {
    Upstream(Option<Read>),
    Client(Option<Bytes>),
    Writer(Result<Result<(), ExecError>, tokio::task::JoinError>),
}

impl Duplex {
    /// `streamCodexDuplex`, after the bootstrap `response.create` was written.
    pub(super) async fn start(
        turn: Turn,
        input: Arc<SteeringInput>,
        initial: Prepared,
        req: ExecRequest,
        view: &View<'_>,
        settings: &Settings,
        exec_session: &str,
    ) -> ExecStream {
        // Turns on a session are serialised, so no other stream holds the frames.
        let frames = input.frames.clone().lock_owned().await;
        let initial_model = gjson::get(&String::from_utf8_lossy(&req.original_body), "model")
            .str()
            .to_owned();
        let (writes, queue) = mpsc::channel(WRITE_QUEUE);
        let writer = tokio::spawn(write_loop(
            turn.conn.clone(),
            input.clone(),
            view.credential.id.clone(),
            turn.wire.clone(),
            queue,
        ));
        // Go's `out` is unbuffered; one slot lets the task hand over and keep reading.
        let (out, chunks) = mpsc::channel(1);
        let duplex = Self {
            turn,
            frames,
            input,
            req,
            credential: view.credential.clone(),
            proxy: view.proxy.clone(),
            cpa_session: view.session.clone(),
            settings: settings.clone(),
            exec_session: exec_session.to_owned(),
            current: initial.clone(),
            pending: VecDeque::from([initial.clone()]),
            initial,
            initial_model,
            unacknowledged: Vec::new(),
            accepted: HashMap::new(),
            response_id: String::new(),
            response_settings: HashMap::new(),
            response_order: VecDeque::new(),
            steering_settings: HashMap::new(),
            waiting_parent: String::new(),
            automatic_active: false,
            queued_creates: VecDeque::new(),
            deferred_steer: None,
            input_ready: false,
            first_response: true,
            response_active: false,
            outbox: VecDeque::new(),
            out,
            writes,
            writer,
            done: false,
        };
        // The task runs whether or not the consumer polls, as Go's goroutines do, and
        // ends when the consumer drops the stream.
        tokio::spawn(duplex.run());
        futures_util::stream::unfold(chunks, |mut chunks| async move {
            chunks.recv().await.map(|item| (item, chunks))
        })
        .boxed()
    }

    async fn run(mut self) {
        loop {
            if self.input_ready && !self.done {
                self.progress().await;
            }
            let deliver = !self.outbox.is_empty();
            if self.done && !deliver {
                return;
            }
            let live = !self.done;
            let writable = self.writes.capacity() > 0;
            let read_client =
                live && self.input_ready && self.deferred_steer.is_none() && writable && self.outbox.len() < OUTBOX;
            let await_room = live && self.input_ready && !writable && !self.writes.is_closed();
            let step = tokio::select! {
                permit = self.out.reserve(), if deliver => match permit {
                    Ok(permit) => {
                        permit.send(self.outbox.pop_front().expect("an item to deliver"));
                        continue;
                    }
                    Err(_) => return,
                },
                () = self.out.closed(), if !deliver => return,
                read = self.turn.rx.recv(), if live && !deliver => Step::Upstream(read),
                frame = self.frames.recv(), if read_client => Step::Client(frame),
                ended = &mut self.writer, if live => Step::Writer(ended),
                // The writer freed room: re-check progress and client reads.
                _ = self.writes.reserve(), if await_room => continue,
            };
            match step {
                Step::Upstream(read) => self.upstream(read),
                Step::Client(frame) => self.client(frame).await,
                Step::Writer(ended) => self.fail(match ended {
                    Ok(Err(error)) => error,
                    Ok(Ok(())) | Err(_) => local("websocket writer stopped"),
                }),
            }
        }
    }

    /// Go's reader reports the writer's failure in place of its own read error (the
    /// writer's cancel is what ended the read).
    fn writer_error(&mut self) -> Option<ExecError> {
        if self.done || !self.writer.is_finished() {
            return None;
        }
        match (&mut self.writer).now_or_never()? {
            Ok(Err(error)) => Some(error),
            Ok(Ok(())) | Err(_) => None,
        }
    }

    fn fail(&mut self, error: ExecError) {
        self.outbox.push_back(Err(connection(error)));
        self.done = true;
    }

    fn enabled(&self) -> bool {
        (self.input.enabled)(&self.credential.id)
    }

    /// Go's local `reject`: an `error` event; the socket stays usable.
    fn reject(&mut self, message: &str) {
        let message = cpa_common::json::quote(message);
        let mut payload = br#"{"error":{"message":"#.to_vec();
        payload.extend_from_slice(&message);
        payload.extend_from_slice(br#","type":"invalid_request_error"},"status":400,"type":"error"}"#);
        self.outbox.push_back(Ok(Bytes::from(payload)));
    }

    /// `readyForCreate`: no unacknowledged steer, no automatic successor running, and
    /// every accepted steer waits on the response the create continues.
    fn ready_for_create(&self) -> bool {
        self.unacknowledged.is_empty()
            && !self.automatic_active
            && self.accepted.values().all(|parent| *parent == self.waiting_parent)
    }

    /// Writer progress after a state change: a deferred steer once pending creates have
    /// started, then queued creates while steering allows them (`flushPendingCreates`).
    /// Each step needs room in the write queue (Go's writer is blocked meanwhile).
    async fn progress(&mut self) {
        if let Some(steer) = self.deferred_steer.take() {
            if !self.pending.is_empty() || self.writes.capacity() == 0 {
                self.deferred_steer = Some(steer);
                return;
            }
            self.steer(steer);
        }
        while !self.done
            && !self.queued_creates.is_empty()
            && self.ready_for_create()
            && self.writes.capacity() > 0
            && self.outbox.len() < OUTBOX
        {
            let payload = self.queued_creates.pop_front().expect("non-empty");
            self.create(payload).await;
        }
    }

    /// Hands a frame to the writer task; callers checked for room.
    fn write(&mut self, frame: String, namespace: Option<(bool, bool)>) {
        match self.writes.try_send(Outgoing { frame, namespace }) {
            Ok(()) => {}
            // The writer stopped on an error it reports through its handle.
            Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(_)) => self.fail(local("websocket write queue is full")),
        }
    }

    /// One client frame (the writer's input loop).
    async fn client(&mut self, frame: Option<Bytes>) {
        let Some(frame) = frame else {
            // The downstream reader stopped: Go's writer fails with `context.Canceled`,
            // a connection error that does not cool the credential.
            self.fail(local("context canceled"));
            return;
        };
        if !self.enabled() {
            // A fresh connection can select an enabled credential; this frame is neither
            // sent on a disabled account nor replayed elsewhere.
            self.fail(local("websocket credential is no longer enabled"));
            return;
        }
        if !cpa_common::json::std_valid(&frame) {
            self.reject("invalid websocket request JSON");
            return;
        }
        // ponytail: invalid UTF-8 inside JSON strings is replaced; Go forwards the bytes.
        let payload = String::from_utf8_lossy(&frame).into_owned();
        let kind = gjson::get(&payload, "type").str().to_owned();
        match kind.as_str() {
            "response.steer" => {
                let parent = gjson::get(&payload, "previous_response_id").str().to_owned();
                if !self.response_settings.contains_key(&parent) && !self.pending.is_empty() {
                    // `waitFor(len(pending) == 0)`: its settings may belong to a create
                    // upstream has not started yet.
                    self.deferred_steer = Some(payload);
                    return;
                }
                self.steer(payload);
            }
            "response.create" | "response.append" => {
                if self.queued_creates.is_empty() && self.ready_for_create() {
                    self.create(payload).await;
                } else if self.queued_creates.len() >= MAX_PENDING {
                    self.fail(local("too many outstanding response.create requests"));
                } else {
                    self.queued_creates.push_back(payload);
                }
            }
            other => self.reject(&format!("unsupported websocket request type: {other}")),
        }
    }

    /// A steer goes upstream exactly as the client sent it: no create translation or
    /// defaults; upstream validates unknown fields and input.
    fn steer(&mut self, payload: String) {
        let parent = gjson::get(&payload, "previous_response_id").str().to_owned();
        if let Some(settings) = self.response_settings.get(&parent).cloned() {
            self.steering_settings.insert(parent.clone(), settings);
        }
        self.unacknowledged.push(parent);
        self.write(payload, None);
    }

    /// `processCreatePayload`.
    async fn create(&mut self, mut payload: String) {
        let append = gjson::get(&payload, "type").str() == "response.append";
        let mut parent = gjson::get(&payload, "previous_response_id").str().trim().to_owned();
        if append && parent.is_empty() && !self.response_id.is_empty() {
            parent.clone_from(&self.response_id);
            payload = set_str(&payload, "previous_response_id", &parent);
        }
        if !self.accepted.is_empty() && parent != self.waiting_parent {
            self.reject("response.create must continue the response waiting for required input");
            return;
        }
        let model = gjson::get(&payload, "model").str().trim().to_owned();
        if !model.is_empty() && model != self.req.model && model != self.initial_model {
            self.fail(ExecError::replay_required());
            return;
        }
        if model.is_empty() {
            let model = if self.req.model.is_empty() {
                self.initial_model.trim()
            } else {
                self.req.model.as_str()
            };
            payload = set_str(&payload, "model", model);
        }
        if append && !gjson::get(&payload, "instructions").exists() {
            let target = self.response_settings.get(&parent).unwrap_or(&self.initial);
            if let Some(instructions) = target.instructions().or(self.initial.instructions()) {
                payload = set_raw(&payload, "instructions", instructions);
            }
        }
        let mut next = self.req.clone();
        next.body = Bytes::from(payload);
        next.original_body = next.body.clone();
        let view = View {
            proxy: self.proxy.clone(),
            session: self.cpa_session.clone(),
            ..View::new(&self.credential)
        };
        let request::Shaped {
            body,
            optimized,
            conflict,
        } = match request::shape(&next, &view, &self.settings, Call::Websocket) {
            Ok(shaped) => shaped,
            Err(error) => {
                self.fail(error);
                return;
            }
        };
        let (body, replay) = Replay::apply(&self.initial.replay.cache, &next, body).await;
        // ponytail: Go starts a usage record per response; this stream reports every
        // response's events to the bootstrap attempt's record and keeps its request.
        let (body, _) = request::prompt_cache(&next, body, Some(&self.exec_session), true);
        // Go also fails when the URL differs from the bootstrap's; it cannot here, since
        // both come from the same credential.
        let prepared = Prepared::new(
            &body,
            &next.original_body,
            optimized,
            conflict,
            request::is_native(&next),
            replay,
        );
        if self.pending.len() >= MAX_PENDING {
            self.fail(local("too many outstanding response.create requests"));
            return;
        }
        self.pending.push_back(prepared);
        self.write(request_frame(body), Some((optimized, conflict)));
    }

    /// `restoreMultiAgent` for an event of `prepared`'s request on this socket.
    fn restores(&self, prepared: &Prepared) -> bool {
        self.turn.conn.restores(prepared.optimized, prepared.conflict)
    }

    /// `releaseSteeringSettings`.
    fn release_steering_settings(&mut self, parent: &str) {
        let in_flight = self.unacknowledged.iter().any(|p| p == parent) || self.accepted.values().any(|p| p == parent);
        if !in_flight {
            self.steering_settings.remove(parent);
        }
    }

    /// One upstream frame (the reader goroutine).
    fn upstream(&mut self, read: Option<Read>) {
        let text = match read {
            Some(Read::Text(text)) => text,
            // Go's duplex wraps the raw read error, not `mapCodexWebsocketReadError`'s 413.
            Some(Read::Failed(error)) => {
                let error = self.writer_error().or_else(|| self.turn.loss()).unwrap_or(error);
                return self.fail(error);
            }
            None => {
                let error = self
                    .writer_error()
                    .or_else(|| self.turn.loss())
                    .unwrap_or_else(|| transport("codex websockets executor: session read channel closed"));
                return self.fail(error);
            }
        };
        let payload = text.trim();
        if payload.is_empty() {
            return;
        }
        let kind = gjson::get(payload, "type").str().to_owned();
        let establishing = self.first_response && kind == "response.created";
        if kind == "response.created" && !self.response_created(payload) {
            return;
        }
        self.turn.observe(payload);
        self.turn.wire.ws_response(payload.as_bytes());
        // Steering acknowledgements, pending notices and failures are opaque: IDs, input,
        // sequence numbers and event types reach the client byte for byte.
        if kind.starts_with("response.steer.") {
            self.steer_event(&kind, payload);
            self.outbox.push_back(Ok(Bytes::from(payload.to_owned())));
            return;
        }
        let failure = kind == "error" || kind == "response.failed";
        let cooling = self.turn.cooling;
        let classify = |payload: &str| {
            ws_error(payload, cooling).or_else(|| response::terminal_failure(payload, cooling).map(|(e, _)| e))
        };
        if !self.first_response && failure {
            // Account health does not depend on which queued request failed: the stream
            // ends with the original classification and is not replayed elsewhere.
            if let Some(error) = classify(payload).filter(|e| matches!(e.status, 401 | 403 | 429)) {
                self.outbox.push_back(Ok(Bytes::from(payload.to_owned())));
                self.outbox.push_back(Err(error));
                self.done = true;
                return;
            }
        }
        // The request this event belongs to: a rejected pending create, else the running one.
        let mut rejected = None;
        if !self.first_response && failure {
            let mut failed = gjson::get(payload, "response.id").str().to_owned();
            if failed.is_empty() {
                failed = gjson::get(payload, "response_id").str().to_owned();
            }
            // A failure of the running response must not consume a queued create; a
            // rejection before `response.created` belongs to the oldest pending create.
            let current_failure = !failed.is_empty() && failed == self.response_id;
            let ambiguous = failed.is_empty()
                && ((!self.pending.is_empty() && self.response_active) || !self.unacknowledged.is_empty());
            if ambiguous {
                // Guessing could corrupt either request: keep the event, end the socket.
                self.outbox.push_back(Ok(Bytes::from(payload.to_owned())));
                self.fail(local(
                    "cannot associate websocket failure with a response or pending create",
                ));
                return;
            }
            match self.pending.pop_front() {
                Some(create) if !current_failure => rejected = Some(create),
                popped => {
                    if let Some(popped) = popped {
                        self.pending.push_front(popped);
                    }
                    self.response_active = false;
                    self.automatic_active = false;
                }
            }
        }
        let event = rejected.as_ref().unwrap_or(&self.current);
        let payload = response::restore(payload, self.restores(event));
        // Every rejection clears its own request's replay; only the first fails the
        // stream (and can enter bootstrap retry), later ones are events the client may
        // correct on this socket.
        let terminal = classify(&payload);
        if let Some(error) = &terminal {
            event.replay.clear(&payload, cooling);
            self.turn.wire.ws_exec_error("upstream_error", error);
        }
        if self.first_response
            && let Some(error) = terminal
        {
            self.outbox.push_back(Err(error));
            self.done = true;
            return;
        }
        let mut out = payload.into_owned();
        if kind == "response.output_item.done" {
            self.turn.items.collect(&out);
        }
        if matches!(
            kind.as_str(),
            "response.completed" | "response.done" | "response.incomplete"
        ) {
            self.response_active = false;
            self.automatic_active = false;
            out = response::normalize_completion(out);
            if !self.current.native {
                out = self.turn.items.patch(out);
            }
            if kind != "response.incomplete" {
                self.current.replay.completed(&out);
            }
        }
        self.outbox
            .push_back(Ok(Bytes::from(response::ensure_usage_details(out))));
        if establishing {
            // Delivered before any locally generated error, so the handler also sees the
            // bootstrap succeed first.
            self.input_ready = true;
        }
    }

    /// `response.created`: which request it serves and what it retains. False ends the
    /// stream (an automatic successor without retained parent settings).
    fn response_created(&mut self, payload: &str) -> bool {
        let mut parent = gjson::get(payload, "response.previous_response_id").str().to_owned();
        if parent.is_empty() {
            parent.clone_from(&self.response_id);
        }
        let automatic = !self.first_response && self.pending.is_empty();
        if automatic {
            let settings = self
                .steering_settings
                .get(&parent)
                .or_else(|| self.response_settings.get(&parent))
                .cloned();
            let Some(settings) = settings else {
                self.fail(local("automatic successor has no retained parent settings"));
                return false;
            };
            self.current = settings;
        }
        self.accepted.retain(|_, target| *target != parent);
        self.waiting_parent.clear();
        self.automatic_active = automatic;
        if let Some(next) = self.pending.pop_front() {
            self.current = next;
        }
        self.response_id = gjson::get(payload, "response.id").str().to_owned();
        self.response_settings
            .insert(self.response_id.clone(), self.current.retained());
        self.response_order.push_back(self.response_id.clone());
        if self.response_order.len() > RETAINED_RESPONSES
            && let Some(oldest) = self.response_order.pop_front()
        {
            self.response_settings.remove(&oldest);
        }
        self.release_steering_settings(&parent);
        self.first_response = false;
        self.response_active = true;
        self.turn.items = Default::default();
        true
    }

    /// `response.steer.*`: acknowledgement bookkeeping.
    fn steer_event(&mut self, kind: &str, payload: &str) {
        let id = gjson::get(payload, "steer.id").str().to_owned();
        let mut parent = gjson::get(payload, "steer.previous_response_id").str().to_owned();
        if parent.is_empty() {
            parent.clone_from(&self.response_id);
        }
        let consume = |unacknowledged: &mut Vec<String>| {
            if let Some(i) = unacknowledged.iter().position(|t| *t == parent || t.is_empty()) {
                unacknowledged.remove(i);
            }
        };
        match kind {
            "response.steer.accepted" => {
                consume(&mut self.unacknowledged);
                self.accepted.insert(id, parent);
            }
            "response.steer.failed" => {
                if self.accepted.remove(&id).is_none() {
                    consume(&mut self.unacknowledged);
                }
                self.release_steering_settings(&parent);
            }
            // Tool results may already wait in the queue; no automatic successor can start
            // until an explicit continuation supplies them.
            "response.steer.pending" => self.waiting_parent = parent,
            _ => {}
        }
    }
}
