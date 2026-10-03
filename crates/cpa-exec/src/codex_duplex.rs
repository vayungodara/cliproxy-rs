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
//! Go runs a writer and a reader goroutine around shared state. Here one task does both:
//! each step first forwards what upstream sent, then makes writer progress, then reads the
//! next client frame. Writer waits (`waitFor`, queued creates) become state the next step
//! re-checks after every upstream event.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use bytes::Bytes;
use cpa_core::credential::Credential;
use cpa_core::exec::{ExecError, ExecRequest, ExecStream, FailureScope};
use futures_util::StreamExt;
use tokio::sync::{OwnedMutexGuard, mpsc};

use super::{Read, Turn, request_frame, transport, ws_error};
use crate::codex_json::{set_raw, set_str};
use crate::codex_request::{self as request, Call, Settings, View};
use crate::codex_response as response;

/// Bound on queued and outstanding `response.create` requests.
const MAX_PENDING: usize = 16;
/// Responses whose settings are retained for automatic successors and appends.
const RETAINED_RESPONSES: usize = 16;

type Enabled = dyn Fn(&str) -> bool + Send + Sync;

/// A downstream connection's later client frames (Go `WebsocketInput`) and the live
/// account check for its bound credential (Go `WebsocketAuthCheck`).
pub struct SteeringInput {
    frames: Arc<tokio::sync::Mutex<mpsc::Receiver<Bytes>>>,
    enabled: Box<Enabled>,
}

impl SteeringInput {
    /// `frames` carries the client's frames in arrival order; a bounded channel
    /// backpressures the client. `enabled(credential_id)` says whether the bound
    /// credential may still send.
    pub fn new(frames: mpsc::Receiver<Bytes>, enabled: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        Self {
            frames: Arc::new(tokio::sync::Mutex::new(frames)),
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
    /// Upstream payloads need collaboration names restored (multi-agent v2).
    restore: bool,
    /// Native Codex request: completion output stays as upstream sent it.
    native: bool,
}

impl Prepared {
    pub(super) fn new(body: &str, original: &[u8], restore: bool, native: bool) -> Self {
        let raw = |json: &str| {
            let value = gjson::get(json, "instructions");
            value.exists().then(|| value.json().to_owned())
        };
        Self {
            instructions: raw(body),
            original_instructions: raw(&String::from_utf8_lossy(original)),
            restore,
            native,
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
    outbox: VecDeque<Result<Bytes, ExecError>>,
    done: bool,
}

impl Drop for Duplex {
    /// The socket ends with the stream; the handler already knows (no notify).
    fn drop(&mut self) {
        self.turn
            .invalidate(&transport("codex websockets executor: duplex closed"), false);
    }
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
            done: false,
        };
        futures_util::stream::unfold(duplex, |mut d| async move { d.next().await.map(|item| (item, d)) }).boxed()
    }

    async fn next(&mut self) -> Option<Result<Bytes, ExecError>> {
        loop {
            if let Some(item) = self.outbox.pop_front() {
                return Some(item);
            }
            if self.done {
                return None;
            }
            if self.input_ready {
                self.progress().await;
                if self.done || !self.outbox.is_empty() {
                    continue;
                }
            }
            let read_client = self.input_ready && self.deferred_steer.is_none();
            let step = tokio::select! {
                biased;
                read = self.turn.rx.recv() => Step::Upstream(read),
                frame = self.frames.recv(), if read_client => Step::Client(frame),
            };
            match step {
                Step::Upstream(read) => self.upstream(read),
                Step::Client(frame) => self.client(frame).await,
            }
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
    async fn progress(&mut self) {
        if let Some(steer) = self.deferred_steer.take() {
            if !self.pending.is_empty() {
                self.deferred_steer = Some(steer);
                return;
            }
            self.steer(steer).await;
        }
        while !self.done && !self.queued_creates.is_empty() && self.ready_for_create() {
            let payload = self.queued_creates.pop_front().expect("non-empty");
            self.create(payload).await;
        }
    }

    async fn write(&mut self, frame: String) {
        if !self.enabled() {
            self.fail(local("websocket credential is no longer enabled"));
            return;
        }
        if let Err(error) = self.turn.conn.send(frame).await {
            self.fail(error);
        }
    }

    /// One client frame (the writer's input loop).
    async fn client(&mut self, frame: Option<Bytes>) {
        let Some(frame) = frame else {
            // The downstream reader stopped: the connection is gone.
            self.done = true;
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
                self.steer(payload).await;
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
    async fn steer(&mut self, payload: String) {
        let parent = gjson::get(&payload, "previous_response_id").str().to_owned();
        if let Some(settings) = self.response_settings.get(&parent).cloned() {
            self.steering_settings.insert(parent.clone(), settings);
        }
        self.unacknowledged.push(parent);
        self.write(payload).await;
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
        let shaped = request::shape(&next, &view, &self.settings, Call::Websocket);
        let (body, restore) = match shaped {
            Ok(shaped) => shaped,
            Err(error) => {
                self.fail(error);
                return;
            }
        };
        // ponytail: Go starts a usage record per response; this stream reports every
        // response's events to the bootstrap attempt's record and keeps its request.
        let (body, _) = request::prompt_cache(&next, body, Some(&self.exec_session), true);
        // Go also fails when the URL differs from the bootstrap's; it cannot here, since
        // both come from the same credential.
        let prepared = Prepared::new(&body, &next.original_body, restore, request::is_native(&next));
        if self.pending.len() >= MAX_PENDING {
            self.fail(local("too many outstanding response.create requests"));
            return;
        }
        self.pending.push_back(prepared);
        self.write(request_frame(body)).await;
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
            Some(Read::Failed(error)) => return self.fail(error),
            None => return self.fail(transport("codex websockets executor: session read channel closed")),
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
        let mut restore = self.current.restore;
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
                Some(rejected) if !current_failure => restore = rejected.restore,
                popped => {
                    if let Some(popped) = popped {
                        self.pending.push_front(popped);
                    }
                    self.response_active = false;
                    self.automatic_active = false;
                }
            }
        }
        let payload = response::restore(payload, restore);
        // Only the first rejection fails the stream (and can enter bootstrap retry);
        // later ones are events the client may correct on this socket.
        if self.first_response
            && let Some(error) = classify(&payload)
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
