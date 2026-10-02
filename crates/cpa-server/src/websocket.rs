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

use std::collections::HashMap;
use std::sync::Arc;
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
use cpa_core::exec::{Caller, ExecError, ExecRequest, ExecSession, ExecStream, Operation, ResponseBody};
use cpa_core::format::Format;
use cpa_exec::codex::CodexExecutor;
use futures_util::StreamExt;

use crate::registry::provider_key;
use crate::runtime::{AcquireError, Completing, Lease, Outcome, Runtime, Selection};
use crate::scheduler::{Policy, canonical_model};
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

async fn upgrade(
    State(rt): State<Arc<Runtime>>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let Ok(ws) = ws else {
        // gorilla `returnError`: plain status text and the supported version.
        let mut response = (StatusCode::BAD_REQUEST, "Bad Request\n").into_response();
        let h = response.headers_mut();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
        h.insert(header::SEC_WEBSOCKET_VERSION, HeaderValue::from_static("13"));
        return response;
    };
    // `websocketUpgradeHeaders`: keep sticky turn state across reconnects.
    let turn_state = headers
        .get(TURN_STATE)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .and_then(|v| HeaderValue::from_str(v).ok());
    let connection = Connection::new(rt, caller, headers);
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
}

/// What one successful attempt selected.
struct Started {
    lease: Lease,
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
        }
    }

    async fn run(mut self, mut socket: WebSocket) {
        let _tools = Retained::new(self.tool_key.clone());
        let mut lost = Box::pin(self.rt.executors.session_closed(&self.session));
        loop {
            let message = tokio::select! {
                message = socket.recv() => message,
                error = &mut lost => {
                    close_for_upstream_loss(&mut socket, &error).await;
                    break;
                }
            };
            let payload = match message {
                Some(Ok(Message::Text(text))) => text.as_str().to_owned(),
                Some(Ok(Message::Binary(bytes))) => String::from_utf8_lossy(&bytes).into_owned(),
                Some(Ok(_)) => continue,
                Some(Err(_)) | None => break,
            };
            // The turn wins ties so frames already received reach the client before a
            // simultaneous upstream-loss signal closes the connection.
            let flow = tokio::select! {
                biased;
                flow = self.turn(&mut socket, payload) => Ok(flow),
                error = &mut lost => Err(error),
            };
            match flow {
                Ok(Flow::Next) => {}
                Ok(Flow::End) => break,
                Err(error) => {
                    close_for_upstream_loss(&mut socket, &error).await;
                    break;
                }
            }
        }
        self.rt.executors.close_session(&self.session);
    }

    async fn turn(&mut self, socket: &mut WebSocket, payload: String) -> Flow {
        let (cfg, policy) = self.rt.request_snapshot();
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
        // ponytail: client.codex.optimize-multi-agent-v2 and
        // codex.orphan-delegation-compatibility (both default off) are not applied here.

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
        let started = self.attempt(&request, &model, &cfg, &policy, &mut ctx).await;
        let forwarded = match started {
            Ok(started) => self.forward(socket, started, &cfg, &mut ctx).await,
            Err(error) => {
                let failure = Failure::from_dispatch(&error);
                if ctx.suppress(failure.status) {
                    Forwarded::Suppressed
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

    /// Selection, credential failover and retry rounds until a stream yields its first
    /// event (`ExecuteStreamWithAuthManager` with a pinned auth and the execution session).
    // ponytail: follows dispatch::run (retry rounds, request-scoped rules, 401
    // refresh-and-retry) but runs `execute_in_session`, honours the pin and reports the
    // serving credential, which dispatch cannot yet; alias pools use their first model.
    async fn attempt(
        &self,
        request: &str,
        model: &str,
        cfg: &Config,
        policy: &Arc<Policy>,
        ctx: &mut TurnCtx,
    ) -> Result<Started, dispatch::Failure> {
        let registry = self.rt.registry();
        let aliases = crate::registry::global_aliases(cfg);
        let (providers, _) = self.providers(model);
        if providers.is_empty() {
            return Err(dispatch::Failure::UnknownModel(model.to_owned()));
        }
        let resolved = resolve_auto(&self.rt, &registry, model);
        // A pinned credential is the only candidate (`WithPinnedAuthID`).
        let pinned_only = |c: &Arc<Credential>| !self.pinned.is_empty() && c.id != self.pinned;
        let exclude: Vec<String> = self
            .rt
            .store()
            .snapshot()
            .iter()
            .filter(|c| pinned_only(c))
            .map(|c| c.id.clone())
            .collect();
        let body = Bytes::from(request.to_owned());
        // Go `session.Enrich` with the connection as execution session (dispatch::run).
        let identity = crate::session::resolve(
            Format::OpenAIResponse,
            &self.headers,
            &body,
            Some(&self.session),
            &self.caller.principal,
        );
        let mut selection = Selection {
            providers: providers.clone(),
            model: resolved.clone(),
            session: identity.id.clone(),
            session_parent: identity.parent.clone(),
            session_fork: identity.fork,
            exclude: exclude.clone(),
            ..Selection::default()
        };
        let session = ExecSession {
            id: self.session.clone(),
            continuation: ctx.native && ctx.requires_current,
        };
        let req = ExecRequest {
            operation: Operation::Generate,
            source_format: Format::OpenAIResponse,
            response_format: Format::OpenAIResponse,
            requested_model: resolved.clone(),
            model: resolved.clone(),
            original_body: body.clone(),
            body,
            stream: true,
            alt: None,
            session: selection.session.clone(),
            execution_session: Some(self.session.clone()),
            derived_session: identity.derived.clone(),
            headers: self.headers.clone(),
            caller: self.caller.clone(),
        };
        let mut upstream: Option<ExecError> = None;
        loop {
            let mut last: Option<ExecError> = None;
            let mut attempted: Vec<String> = Vec::new();
            let pick_failure = loop {
                if policy.max_retry_credentials > 0 && attempted.len() >= policy.max_retry_credentials {
                    break None;
                }
                let mut lease = match self.rt.acquire(selection.clone(), cfg, policy.clone(), &registry).await {
                    Ok(lease) => lease,
                    Err(AcquireError::Prepare { id, error }) => {
                        attempted.push(id.clone());
                        selection.exclude.push(id);
                        last = Some(error);
                        continue;
                    }
                    Err(AcquireError::Cooldown { wait, cause }) => {
                        break Some(dispatch::Failure::Cooldown {
                            model: selection.model.clone(),
                            provider: if providers.len() == 1 {
                                providers[0].clone()
                            } else {
                                String::new()
                            },
                            wait,
                            cause,
                        });
                    }
                    Err(AcquireError::Unavailable { retry_after, cause }) => {
                        break Some(dispatch::Failure::Unavailable {
                            code: if retry_after.is_some() {
                                "auth_unavailable"
                            } else {
                                "auth_not_found"
                            },
                            providers: providers.clone(),
                            model: resolved.clone(),
                            cause,
                            retry_after,
                        });
                    }
                };
                selection.exclude.push(lease.credential.id.clone());
                attempted.push(lease.credential.id.clone());
                let (models, _) = crate::registry::execution_models(&aliases, &lease.credential, &selection.model);
                let upstream_model = models.into_iter().next().unwrap_or_else(|| resolved.clone());
                let selection_model = crate::registry::selection_model(&aliases, &lease.credential, &selection.model);
                lease.execution_model =
                    crate::registry::state_model(&selection_model, &selection.model, &upstream_model, false);
                // `WithSelectedAuthIDCallback`.
                let credential = lease.credential.clone();
                ctx.last_attempted.clone_from(&credential.id);
                ctx.pinned_attempted |= !self.pinned.is_empty() && credential.id == self.pinned;
                ctx.attempted_mode = if self.rt.executors.session_upstream(&credential) {
                    Mode::Websocket
                } else {
                    Mode::Http
                };
                ctx.preserve_output = ctx.native_request && credential.provider.eq_ignore_ascii_case("codex");
                let mut attempt_req = req.clone();
                attempt_req.model = upstream_model;
                let mut executed = self
                    .rt
                    .executors
                    .execute_in_session(&lease.credential, attempt_req.clone(), cfg, &session)
                    .await;
                // Go `tryRefreshAfterUnauthorized`: one refresh-and-retry per credential.
                if let Err(error) = &executed
                    && classify::is_unauthorized(error)
                    && let Some(current) = self.rt.refresh_after_unauthorized(&lease.credential, cfg).await
                {
                    lease.credential = current;
                    executed = self
                        .rt
                        .executors
                        .execute_in_session(&lease.credential, attempt_req, cfg, &session)
                        .await;
                }
                let error = match executed {
                    Ok(response) => match first_event(response.body).await {
                        Ok((first, stream)) => return Ok(Started { lease, first, stream }),
                        Err(error) => error,
                    },
                    Err(error) => error,
                };
                let action = policy.error_action(&lease.credential, &error);
                let stop = if action.matched {
                    action.stop
                } else {
                    classify::is_request_invalid(&error)
                };
                lease.complete(Outcome::Failure(error.clone()));
                if stop {
                    return Err(dispatch::Failure::Exec(error));
                }
                if classify::upstream_attempted(&error) {
                    upstream = Some(error.clone());
                }
                last = Some(error);
            };
            let round: dispatch::Failure = match (last, pick_failure) {
                (Some(error), _) => dispatch::Failure::Exec(error),
                (None, Some(failure)) => failure,
                (None, None) => dispatch::Failure::Unavailable {
                    code: "auth_not_found",
                    providers: providers.clone(),
                    model: resolved.clone(),
                    cause: None,
                    retry_after: None,
                },
            };
            let (status, retry_round) = match &round {
                dispatch::Failure::Exec(e) => (
                    classify::go_status(e),
                    classify::is_retry_round(e) && !classify::is_request_invalid(e),
                ),
                dispatch::Failure::Cooldown { .. } => (429, true),
                dispatch::Failure::Unavailable {
                    retry_after: Some(_), ..
                } => (503, true),
                _ => (0, false),
            };
            let terminal = |round| upstream.clone().map(dispatch::Failure::Exec).unwrap_or(round);
            if !retry_round {
                return Err(terminal(round));
            }
            let wait = {
                let admit = crate::runtime::admission(&registry, &aliases, &selection, &self.rt.executors);
                self.rt
                    .store()
                    .retry_wait(&selection, policy, status, &attempted, &admit)
            };
            let Some(wait) = wait else {
                return Err(terminal(round));
            };
            if !wait.is_zero() {
                tokio::time::sleep(dispatch::jitter(wait, policy.max_retry_interval)).await;
            }
            selection.retry_round += 1;
            selection.exclude.clone_from(&exclude);
        }
    }

    /// `forwardResponsesWebsocket`.
    async fn forward(&self, socket: &mut WebSocket, started: Started, cfg: &Config, ctx: &mut TurnCtx) -> Forwarded {
        let Started { lease, first, stream } = started;
        let mut stream = futures_util::stream::iter(first.map(Ok))
            .chain(Completing::new(stream, lease))
            .boxed();
        let keepalive = keepalive_interval(cfg);
        let mut deadline = keepalive.map(|k| tokio::time::Instant::now() + k);
        let mut turn = Turn::default();
        let (mut completed, mut completed_output, mut completed_id) = (false, String::from("[]"), String::new());
        loop {
            let item = tokio::select! {
                item = stream.next() => item,
                _ = sleep_until(deadline) => {
                    if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                        return Forwarded::End;
                    }
                    deadline = keepalive.map(|k| tokio::time::Instant::now() + k);
                    continue;
                }
            };
            let chunk = match item {
                None if completed => {
                    return Forwarded::Completed {
                        output: completed_output,
                        id: completed_id,
                        pending: turn.pending(),
                    };
                }
                // `stream closed before response.completed`: 408, closed silently.
                None => {
                    close_for_failure(socket, &Failure::local(408, "stream closed before response.completed")).await;
                    return Forwarded::End;
                }
                Some(Err(error)) => {
                    let failure = Failure::from_exec(&error);
                    if ctx.suppress(failure.status) {
                        return Forwarded::Suppressed;
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
                if kind == "error" {
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

/// Waits for the first non-empty event so a failed stream can still fail over (Go
/// conductor_stream.go: an empty stream is a failed attempt).
async fn first_event(body: ResponseBody) -> Result<(Option<Bytes>, ExecStream), ExecError> {
    let mut stream = match body {
        ResponseBody::Stream(stream) => stream,
        ResponseBody::Buffered(bytes) => futures_util::stream::once(async move { Ok(bytes) }).boxed(),
    };
    loop {
        match stream.next().await {
            Some(Ok(bytes)) if bytes.is_empty() => continue,
            Some(Ok(bytes)) => return Ok((Some(bytes), stream)),
            Some(Err(error)) => return Err(error),
            None => return Err(classify::empty_stream()),
        }
    }
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

async fn send_text(socket: &mut WebSocket, payload: String) -> bool {
    socket.send(Message::Text(payload.into())).await.is_ok()
}

async fn close_with_code(socket: &mut WebSocket, code: u16, reason: &str) {
    let reason = requests::truncate_reason(reason, CLOSE_REASON_MAX);
    let close = Message::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    }));
    let _ = tokio::time::timeout(TERMINAL_WRITE, socket.send(close)).await;
}

/// `websocketClosePayloadForUpstreamError`: replay and message-too-big failures map to
/// close codes the client acts on.
fn close_code(failure: &Failure) -> Option<(u16, String)> {
    if failure.replay {
        return Some((CLOSE_SERVICE_RESTART, "upstream requires HTTP replay".into()));
    }
    if failure.status == 413 && gjson::get(&failure.text, "error.code").str() == "message_too_big" {
        let reason = gjson::get(&failure.text, "error.message").str().trim().to_owned();
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
/// caller right after.
async fn close_for_failure(socket: &mut WebSocket, failure: &Failure) {
    if let Some((code, reason)) = close_code(failure) {
        close_with_code(socket, code, &reason).await;
        return;
    }
    if exposed(failure) {
        let payload = failure
            .payload
            .clone()
            .unwrap_or_else(|| error_payload(failure.status, &failure.text));
        let _ = tokio::time::timeout(TERMINAL_WRITE, socket.send(Message::Text(payload.into()))).await;
    }
}

/// `closeForUpstreamDisconnect`: the session's upstream socket was lost.
async fn close_for_upstream_loss(socket: &mut WebSocket, error: &ExecError) {
    close_for_failure(socket, &Failure::from_exec(error)).await;
}

#[cfg(test)]
#[path = "websocket_tests.rs"]
mod tests;
