//! Request interceptors, response and stream-chunk interceptors and the request
//! lifecycle around model execution (Go sdk/api/handlers/handlers_interceptors.go, the
//! interceptor calls in handlers_execution.go and handlers_stream.go, and
//! sdk/cliproxy/auth/conductor_execution.go `applyRequestAfterAuthInterceptor`).
//!
//! ponytail: Home mode runs no after-auth interceptor (Go calls it from the Home
//! conductor too); the Responses SSE validation of intercepted chunks is the route's.

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use cpa_core::exec::{Caller, ExecError, ExecRequest, ExecStream, FailureScope};
use cpa_core::format::Format;
use cpa_plugin::api::{
    COMPLETION_CANCELED, COMPLETION_FAILED, COMPLETION_REJECTED, COMPLETION_SUCCEEDED, RequestCompletion,
    RequestInterceptRequest, RequestInterceptResponse, ResponseInterceptRequest, STREAM_CHUNK_HEADER_INIT_INDEX,
    StreamChunkInterceptRequest, WebSocketResponseEvent,
};
use cpa_plugin::callbacks::RequestScope;
use cpa_plugin::gojson::{GoTime, Header, Metadata};
use futures_util::StreamExt;
use serde_json::Value;

use crate::Runtime;
use crate::dispatch::{Call, Done, Failure, Trace};

/// Go `maxStreamInterceptorHistoryChunks` / `maxStreamInterceptorHistoryBytes`.
const HISTORY_CHUNKS: usize = 32;
const HISTORY_BYTES: usize = 256 * 1024;

/// Go `requestLifecycleTracker`: one `request.complete` per request, whatever ends it.
/// Dropping an incomplete tracker reports the request canceled (the client left).
pub(crate) struct Lifecycle {
    host: cpa_plugin::Host,
    request_id: String,
    pending: Mutex<Option<RequestCompletion>>,
    scope: RequestScope,
    skip_plugin: String,
}

impl Lifecycle {
    pub(crate) fn start(
        rt: &Runtime,
        scope: RequestScope,
        source_format: &str,
        model: &str,
        requested_model: &str,
        stream: bool,
        metadata: Metadata,
    ) -> Arc<Self> {
        let request_id = uuid::Uuid::new_v4().to_string();
        Arc::new(Self {
            host: rt.plugins().clone(),
            pending: Mutex::new(Some(RequestCompletion {
                request_id: request_id.clone(),
                trace_id: scope.request_id.clone(),
                source_format: source_format.to_owned(),
                model: model.to_owned(),
                requested_model: requested_model.to_owned(),
                stream,
                started_at: now(),
                metadata,
                ..Default::default()
            })),
            request_id,
            scope,
            skip_plugin: super::model_callbacks::skip_plugin(),
        })
    }

    pub(crate) fn complete(&self, outcome: &str, status: u16, error: &str) {
        let Some(mut completion) = self.pending.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        completion.outcome = outcome.to_owned();
        completion.status_code = i64::from(status);
        completion.completed_at = now();
        completion.error = error.to_owned();
        self.host.complete_request(completion, &self.skip_plugin, &self.scope);
    }

    /// Go `completeError`: a direct (plugin or executor) response is `rejected`;
    /// `terminated` is the error text of a plugin termination.
    pub(crate) fn fail(&self, failure: &Failure, terminated: Option<&str>) {
        match failure.direct() {
            Some(e) => {
                let error = terminated.map_or_else(|| crate::classify::error_text(e), str::to_owned);
                self.complete(COMPLETION_REJECTED, e.status, &error);
            }
            None => self.complete(COMPLETION_FAILED, failure.status(), &failure.text()),
        }
    }
}

impl Drop for Lifecycle {
    fn drop(&mut self) {
        self.scope.cancel.cancel();
        self.complete(COMPLETION_CANCELED, 0, "context canceled");
    }
}

fn now() -> GoTime {
    GoTime(Some(chrono::Local::now().fixed_offset()))
}

fn scope(trace: &Trace, headers: Header, metadata: &Metadata, caller: Option<&Caller>) -> RequestScope {
    let inherited = super::model_callbacks::context().map(|c| c.scope).unwrap_or_default();
    let value = |key: &str| metadata.get(key).and_then(Value::as_str).map(str::to_owned);
    let (api_key, access_provider) = super::model_callbacks::usage_identity(caller, &inherited);
    RequestScope {
        request_id: trace.request_id(),
        capture: trace.capture(),
        headers,
        query: super::go_values(&super::execution::query()),
        cancel: inherited.cancel.child_token(),
        pinned_auth_id: value("pinned_auth_id"),
        execution_session: value("execution_session_id"),
        request_path: value("request_path").unwrap_or_default(),
        api_key,
        access_provider,
    }
}

/// Go `EmitWebSocketResponseEvent` and the handler's observer closure: what an event
/// carries besides the frame.
#[derive(Clone, Default)]
struct WebSocketEventBase {
    /// The handler's lifecycle request ID and trace ID; a non-empty `request_id` or
    /// `trace_id` string in the metadata takes their place.
    request_id: String,
    trace_id: String,
    source_format: String,
    metadata: Metadata,
}

impl WebSocketEventBase {
    fn event(
        &self,
        provider: &str,
        credential: &cpa_core::credential::Credential,
        model: &str,
        payload: &[u8],
    ) -> WebSocketResponseEvent {
        let text = |key: &str, fallback: &str| match self.metadata.get(key) {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => fallback.to_owned(),
        };
        // Go `PayloadRequestedModel`: the metadata's requested model, else the upstream one.
        let requested_model = match self.metadata.get("requested_model") {
            Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_owned(),
            _ => model.trim().to_owned(),
        };
        WebSocketResponseEvent {
            request_id: text("request_id", &self.request_id),
            trace_id: text("trace_id", &self.trace_id),
            source_format: self.source_format.clone(),
            model: model.to_owned(),
            requested_model,
            provider: provider.to_owned(),
            auth_id: credential.id.clone(),
            auth_label: credential.label.clone(),
            auth_type: cpa_core::registry::dynamic::account_info(credential).0.to_owned(),
            event_type: cpa_common::json::get(payload, "type").str().into_owned(),
            payload: Bytes::copy_from_slice(payload),
            metadata: self.metadata.clone(),
        }
    }
}

/// The attempt's `Options.WebSocketResponseObserver`.
struct WebSocketObserver {
    base: WebSocketEventBase,
    host: cpa_plugin::Host,
    skip: String,
    /// The request's scope with its own cancellation (Go `context.WithoutCancel`).
    scope: RequestScope,
}

impl cpa_core::exec::WebSocketObserver for WebSocketObserver {
    fn response<'a>(
        &'a self,
        provider: &'a str,
        credential: &'a cpa_core::credential::Credential,
        model: &'a str,
        payload: &'a [u8],
    ) -> futures_util::future::BoxFuture<'a, ()> {
        let event = self.base.event(provider, credential, model, payload);
        let (host, skip, scope) = (self.host.clone(), self.skip.clone(), self.scope.clone());
        // The executor awaits the observation before it handles the frame, as Go calls the
        // observers in its read loop. The call runs in its own task so that it, and the
        // callback context its plugin uses, finish even when the attempt is dropped
        // meanwhile; Go's executor waits for the observer the same way.
        let observed = tokio::spawn(async move { host.observe_websocket_response_event(event, &skip, &scope).await });
        Box::pin(async move {
            let _ = observed.await;
        })
    }
}

/// The session part of Go `applyRequestAfterAuthInterceptor`: a rewritten body or
/// cleared headers resolve the canonical and parent session again from the executed
/// request; rewritten headers alone can only set them.
fn resolve_session(metadata: &mut Metadata, req: &ExecRequest, resp: &RequestInterceptResponse) {
    let execution = match metadata.get("execution_session_id") {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    };
    let rewrote_body = !resp.clear_headers.is_empty() || !resp.body.is_empty();
    if !rewrote_body && resp.headers.is_empty() {
        return;
    }
    let payload: &[u8] = match (rewrote_body, req.original_body.is_empty()) {
        (false, _) => &[],
        (true, false) => &req.original_body,
        (true, true) => &req.body,
    };
    let info = cpa_common::session::extract_session_info(&req.headers, payload, execution.as_deref())
        .filter(|info| !info.session_id.is_empty());
    let bound = |s: &str| Value::String(cpa_common::session::bound_session_identity(s));
    match info {
        Some(info) => {
            metadata.insert("canonical_session_id".into(), bound(&info.session_id));
            if !info.parent_session_id.is_empty() && info.parent_session_id != info.session_id {
                metadata.insert("parent_session_id".into(), bound(&info.parent_session_id));
            } else if rewrote_body {
                metadata.remove("parent_session_id");
            }
        }
        None if rewrote_body => {
            for key in ["canonical_session_id", "parent_session_id", "lcp_affinity_session_id"] {
                metadata.remove(key);
            }
        }
        None => {}
    }
}

/// Go `normalizedTerminationStatus`.
fn termination_status(status: i64) -> u16 {
    if (200..=599).contains(&status) {
        status as u16
    } else {
        403
    }
}

/// Go `directTerminationError`: the plugin's status, headers and body, sent as is.
fn terminated(resp: &RequestInterceptResponse) -> Failure {
    Failure::Exec(ExecError {
        status: termination_status(resp.status_code),
        scope: FailureScope::Request,
        body: resp.response_body.clone(),
        headers: Box::new(header_map(&resp.response_headers)),
        retry_after: None,
        direct: true,
        addon: None,
    })
}

/// A plugin `http.Header` as a header map (invalid names or values dropped).
pub(crate) fn header_map(headers: &Header) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, values) in headers {
        let Ok(name) = HeaderName::try_from(name.as_str()) else {
            continue;
        };
        for value in values {
            if let Ok(value) = HeaderValue::from_str(value) {
                out.append(name.clone(), value);
            }
        }
    }
    out
}

/// Response headers as Go's `http.Header` (canonical names).
pub(crate) fn go_header(headers: &HeaderMap) -> Header {
    let mut out = Header::new();
    for (name, value) in headers {
        out.entry(cpa_exec::proxy::canonical_header(name.as_str()))
            .or_default()
            .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
    }
    out
}

/// Go `finalInterceptorHeaders`: the host answers with the chain's merged headers
/// (starting from the current ones), which replace the current ones.
fn final_headers(_current: &Header, intercepted: &Header) -> Header {
    intercepted.clone()
}

/// Go `diffHeaders`: the keys whose values changed.
fn diff_headers(base: &Header, next: &Header) -> Header {
    next.iter()
        .filter(|(k, v)| base.get(*k) != Some(*v))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// What a request becomes after the before-auth interceptors.
pub(crate) struct BeforeAuth {
    pub headers: Header,
    pub body: Option<Bytes>,
    pub path: Option<String>,
}

/// Go `applyRequestInterceptorsBeforeAuth`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn before_auth(
    host: &cpa_plugin::Host,
    lifecycle: &Lifecycle,
    source_format: &str,
    model: &str,
    requested_model: &str,
    stream: bool,
    headers: Header,
    body: &Bytes,
    metadata: &Metadata,
) -> Result<BeforeAuth, Failure> {
    if !host.has_request_interceptors() {
        return Ok(BeforeAuth {
            headers,
            body: None,
            path: None,
        });
    }
    let req = RequestInterceptRequest {
        request_id: lifecycle.request_id.clone(),
        trace_id: lifecycle.scope.request_id.clone(),
        source_format: source_format.to_owned(),
        model: model.to_owned(),
        requested_model: requested_model.to_owned(),
        stream,
        headers: headers.clone(),
        body: body.clone(),
        metadata: metadata.clone(),
        ..Default::default()
    };
    let resp = host
        .intercept_request_before_auth(req, &lifecycle.skip_plugin, &lifecycle.scope)
        .await;
    let out = BeforeAuth {
        headers: final_headers(&headers, &resp.headers),
        body: Some(resp.body.clone()).filter(|b| !b.is_empty()),
        path: Some(resp.path.trim().to_owned()).filter(|p| !p.is_empty()),
    };
    if resp.terminate {
        let failure = terminated(&resp);
        lifecycle.fail(&failure, Some(""));
        return Err(failure);
    }
    Ok(out)
}

/// What the after-auth interceptors changed (Go `requestAfterAuthCapture`).
#[derive(Clone, Default)]
pub(crate) struct AfterAuth {
    pub headers: Header,
    pub body: Option<Bytes>,
    pub path: Option<String>,
}

/// One request's interceptor and lifecycle state on the built-in providers (Go's
/// handler-level `opts` plus the tracker).
pub(crate) struct Hooks {
    host: cpa_plugin::Host,
    pub(crate) lifecycle: Arc<Lifecycle>,
    source_format: String,
    response_format: String,
    model: String,
    requested_model: String,
    stream: bool,
    /// Go's handler `opts.Headers` after the before-auth interceptors.
    headers: Header,
    /// Go's handler `opts.Metadata` after the before-auth interceptors.
    metadata: Metadata,
    /// The client's body after the before-auth interceptors (Go `opts.OriginalRequest`).
    body: Bytes,
    after: Mutex<Option<AfterAuth>>,
    /// The current attempt's executor metadata after its after-auth interceptors (Go's
    /// `opts.Metadata` once `applyRequestAfterAuthInterceptor` returns); `None` when none ran.
    executed_metadata: Mutex<Option<Metadata>>,
    /// The lifecycle error of a plugin termination: Go's after-auth termination is an
    /// error ("request terminated by plugin"); a plugin-executor-route one has none.
    terminated: Mutex<Option<&'static str>>,
}

/// The facts of one built-in request the hooks need.
pub(crate) struct Request<'a> {
    pub source_format: &'a str,
    pub response_format: &'a str,
    pub model: &'a str,
    pub requested_model: &'a str,
    pub stream: bool,
    pub headers: Header,
    pub body: &'a Bytes,
    pub metadata: Metadata,
    pub caller: Option<&'a Caller>,
}

impl Hooks {
    /// Starts the lifecycle and runs the before-auth interceptors. `Ok` carries the
    /// rewritten request parts.
    pub(crate) async fn start(
        rt: &Runtime,
        trace: &Trace,
        req: Request<'_>,
    ) -> Result<(Arc<Self>, BeforeAuth), Failure> {
        let host = rt.plugins().clone();
        let lifecycle = Lifecycle::start(
            rt,
            scope(trace, req.headers.clone(), &req.metadata, req.caller),
            req.source_format,
            req.model,
            req.requested_model,
            req.stream,
            req.metadata.clone(),
        );
        let before = before_auth(
            &host,
            &lifecycle,
            req.source_format,
            req.model,
            req.requested_model,
            req.stream,
            req.headers,
            req.body,
            &req.metadata,
        )
        .await?;
        let mut metadata = req.metadata;
        if let Some(path) = &before.path {
            metadata.insert("request_path".into(), Value::String(path.clone()));
        }
        let hooks = Arc::new(Self {
            host,
            lifecycle,
            source_format: req.source_format.to_owned(),
            response_format: req.response_format.to_owned(),
            model: req.model.to_owned(),
            requested_model: req.requested_model.to_owned(),
            stream: req.stream,
            headers: before.headers.clone(),
            metadata,
            body: before.body.clone().unwrap_or_else(|| req.body.clone()),
            after: Mutex::new(None),
            executed_metadata: Mutex::new(None),
            terminated: Mutex::new(None),
        });
        Ok((hooks, before))
    }

    /// Go `applyRequestAfterAuthInterceptor` for one attempt on `credential`: rewrites
    /// `req`, or ends the request with the plugin's response.
    pub(crate) async fn after_auth(
        &self,
        req: &mut ExecRequest,
        credential: &cpa_core::credential::Credential,
        route_model: &str,
        media: Option<crate::dispatch::MediaKind>,
    ) -> Result<(), ExecError> {
        *self.executed_metadata.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if !self.host.has_request_interceptors() {
            return Ok(());
        }
        let provider = crate::registry::provider_key(credential);
        let metadata = self.attempt_metadata(req, credential, route_model);
        let mut executed = metadata.clone();
        let requested = match metadata.get("requested_model") {
            Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_owned(),
            _ => route_model.trim().to_owned(),
        };
        let intercept = RequestInterceptRequest {
            request_id: self.lifecycle.request_id.clone(),
            trace_id: self.lifecycle.scope.request_id.clone(),
            source_format: self.source_format.clone(),
            to_format: request_to_format(&provider, &self.source_format, req, media),
            model: req.model.clone(),
            requested_model: requested,
            stream: req.stream,
            headers: self.headers.clone(),
            body: req.body.clone(),
            metadata,
        };
        let resp = self
            .host
            .intercept_request_after_auth(intercept, &self.lifecycle.skip_plugin, &self.lifecycle.scope)
            .await;
        // Go merges the chain's (already merged) headers onto the handler's again, without
        // the clears: a header an after-auth interceptor clears stays.
        let headers = cpa_plugin::interceptors::merge_headers(&self.headers, &resp.headers, &[]);
        req.headers = header_map(&headers);
        let body = Some(resp.body.clone()).filter(|b| !b.is_empty());
        if let Some(body) = &body {
            req.body = body.clone();
            req.original_body = body.clone();
        }
        let path = Some(resp.path.trim().to_owned()).filter(|p| !p.is_empty());
        if let Some(path) = &path {
            req.request_path = path.clone();
            executed.insert("request_path".into(), Value::String(path.clone()));
        }
        resolve_session(&mut executed, req, &resp);
        *self.executed_metadata.lock().unwrap_or_else(|e| e.into_inner()) = Some(executed);
        *self.after.lock().unwrap_or_else(|e| e.into_inner()) = Some(AfterAuth { headers, body, path });
        if resp.terminate {
            *self.terminated.lock().unwrap_or_else(|e| e.into_inner()) = Some("request terminated by plugin");
            let Failure::Exec(error) = terminated(&resp) else {
                unreachable!("terminations are executor errors")
            };
            return Err(error);
        }
        Ok(())
    }

    /// Go's executor `opts.Metadata` for one attempt on `credential`: the handler's,
    /// plus what the conductor adds before the after-auth interceptors run.
    fn attempt_metadata(
        &self,
        req: &ExecRequest,
        credential: &cpa_core::credential::Credential,
        route_model: &str,
    ) -> Metadata {
        let mut metadata = self.metadata.clone();
        if let Some(session) = req.session.as_deref().filter(|s| !s.is_empty()) {
            metadata.insert("canonical_session_id".into(), Value::String(session.to_owned()));
        }
        if let Some(derived) = req.derived_session.as_deref().filter(|s| !s.is_empty()) {
            metadata.insert("derived_session_id".into(), Value::String(derived.to_owned()));
        }
        if !credential.id.trim().is_empty() {
            metadata.insert(
                "selected_auth_id".into(),
                Value::String(credential.id.trim().to_owned()),
            );
        }
        let index = cpa_core::config::credentials::auth_index(credential);
        if !index.trim().is_empty() {
            metadata.insert("selected_auth_index".into(), Value::String(index.trim().to_owned()));
        }
        // Go's mixed-provider pick writes its scope into the shared metadata.
        metadata.insert("session_affinity_provider".into(), Value::String("mixed".into()));
        metadata.insert("session_affinity_model".into(), Value::String(route_model.to_owned()));
        metadata
    }

    /// Go `webSocketResponseObserver` for one attempt: `None` without observer plugins.
    /// The event metadata is the attempt's executor metadata after its after-auth
    /// interceptors.
    pub(crate) fn websocket_observer(
        &self,
        req: &ExecRequest,
        credential: &cpa_core::credential::Credential,
        route_model: &str,
    ) -> Option<Arc<dyn cpa_core::exec::WebSocketObserver>> {
        if !self.host.has_websocket_response_observers() {
            return None;
        }
        let metadata = self
            .executed_metadata
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(|| self.attempt_metadata(req, credential, route_model));
        Some(Arc::new(WebSocketObserver {
            base: WebSocketEventBase {
                request_id: self.lifecycle.request_id.clone(),
                trace_id: self.lifecycle.scope.request_id.clone(),
                source_format: self.source_format.clone(),
                metadata,
            },
            host: self.host.clone(),
            skip: self.lifecycle.skip_plugin.clone(),
            scope: RequestScope {
                cancel: Default::default(),
                ..self.lifecycle.scope.clone()
            },
        }))
    }

    /// Go `afterAuthCapture.apply`: the executed request's headers, bodies and metadata.
    fn executed(&self) -> (Header, Bytes, Metadata) {
        let mut metadata = self.metadata.clone();
        let Some(after) = self.after.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            return (self.headers.clone(), self.body.clone(), metadata);
        };
        if let Some(path) = after.path {
            metadata.insert("request_path".into(), Value::String(path));
        }
        (after.headers, after.body.unwrap_or_else(|| self.body.clone()), metadata)
    }

    /// Ends the request: response or stream-chunk interceptors on success, then the
    /// lifecycle. Headers the interceptors changed go to `trace` for the response.
    pub(crate) async fn finish(self: Arc<Self>, result: Result<Done, Failure>, trace: &Trace) -> Result<Done, Failure> {
        match result {
            Err(failure) => {
                let terminated = *self.terminated.lock().unwrap_or_else(|e| e.into_inner());
                self.lifecycle.fail(&failure, terminated);
                Err(failure)
            }
            Ok(Done::Buffered { headers, body }) => {
                let raw = go_header(&headers);
                let (request_headers, request_body, metadata) = self.executed();
                let resp = self
                    .host
                    .intercept_response(
                        ResponseInterceptRequest {
                            request_id: self.lifecycle.request_id.clone(),
                            source_format: self.response_format.clone(),
                            model: self.model.clone(),
                            requested_model: self.requested_model.clone(),
                            stream: false,
                            request_headers,
                            response_headers: raw.clone(),
                            original_request: request_body.clone(),
                            request_body,
                            body: body.clone(),
                            status_code: 200,
                            metadata,
                        },
                        &self.lifecycle.skip_plugin,
                        &self.lifecycle.scope,
                    )
                    .await;
                let body = if resp.body.is_empty() { body } else { resp.body.clone() };
                // Go `downstreamHeadersAfterInterceptors`: the final headers under
                // passthrough, else what the interceptors changed.
                let final_headers = final_headers(&raw, &resp.headers);
                trace.set_intercepted_headers(header_map(&diff_headers(&raw, &final_headers)));
                self.lifecycle.complete(COMPLETION_SUCCEEDED, 200, "");
                let headers = if final_headers == raw {
                    headers
                } else {
                    header_map(&final_headers)
                };
                Ok(Done::Buffered { headers, body })
            }
            // Primed by [`Hooks::prime`]: the stream completes the lifecycle itself.
            Ok(done) => Ok(done),
        }
    }

    /// Runs a stream's chunk interceptors up to its first delivered chunk (Go reads
    /// through dropped chunks before committing, inside the bootstrap retries) and wraps
    /// the rest, which completes the lifecycle. An error before any delivered chunk is
    /// returned for the caller's retry decision; the lifecycle stays open.
    pub(crate) async fn prime(self: &Arc<Self>, result: Result<Done, Failure>, trace: &Trace) -> Result<Done, Failure> {
        match result {
            Ok(Done::Stream { headers, first, rest }) => {
                let raw = go_header(&headers);
                ChunkInterceptor::new(self.clone(), raw)
                    .run(headers, first, rest, trace)
                    .await
                    .map_err(Failure::Exec)
            }
            other => other,
        }
    }
}

/// Go's stream forwarding with chunk interceptors and the lifecycle's end.
struct ChunkInterceptor {
    hooks: Arc<Hooks>,
    raw: Header,
    base: Header,
    request_headers: Header,
    original_request: Bytes,
    request_body: Bytes,
    header_init: bool,
    index: i64,
    history: Vec<Bytes>,
}

impl ChunkInterceptor {
    fn new(hooks: Arc<Hooks>, raw: Header) -> Self {
        Self {
            hooks,
            base: raw.clone(),
            raw,
            request_headers: Header::new(),
            original_request: Bytes::new(),
            request_body: Bytes::new(),
            header_init: false,
            index: 0,
            history: Vec::new(),
        }
    }

    fn active(&self) -> bool {
        self.hooks.host.has_stream_interceptors()
    }

    /// Go `applyStreamHeaderInit`: once, before the first chunk is delivered.
    async fn header_init(&mut self) {
        if self.header_init || !self.active() {
            return;
        }
        let (headers, body, metadata) = self.hooks.executed();
        self.request_headers = headers;
        self.original_request = body.clone();
        self.request_body = body;
        let resp = self
            .hooks
            .host
            .intercept_stream_chunk(
                StreamChunkInterceptRequest {
                    request_id: self.hooks.lifecycle.request_id.clone(),
                    source_format: self.hooks.response_format.clone(),
                    model: self.hooks.model.clone(),
                    requested_model: self.hooks.requested_model.clone(),
                    request_headers: self.request_headers.clone(),
                    response_headers: self.raw.clone(),
                    original_request: self.original_request.clone(),
                    request_body: self.request_body.clone(),
                    chunk_index: STREAM_CHUNK_HEADER_INIT_INDEX,
                    metadata,
                    ..Default::default()
                },
                &self.hooks.lifecycle.skip_plugin,
                &self.hooks.lifecycle.scope,
            )
            .await;
        self.raw = final_headers(&self.raw, &resp.headers);
        self.header_init = true;
    }

    /// Go `transformStreamPayload`: `None` drops the chunk.
    async fn chunk(&mut self, payload: Bytes) -> Option<Bytes> {
        self.header_init().await;
        if !self.active() {
            self.index += 1;
            return Some(payload);
        }
        let host = &self.hooks.host;
        let mut req = StreamChunkInterceptRequest {
            request_id: self.hooks.lifecycle.request_id.clone(),
            source_format: self.hooks.response_format.clone(),
            model: self.hooks.model.clone(),
            requested_model: self.hooks.requested_model.clone(),
            request_headers: self.request_headers.clone(),
            response_headers: self.raw.clone(),
            body: payload.clone(),
            chunk_index: self.index,
            metadata: self.hooks.metadata.clone(),
            ..Default::default()
        };
        if host.stream_chunk_payload_includes_history() {
            req.history_chunks = self.history.clone();
        }
        if host.stream_chunk_payload_includes_request_body() {
            req.original_request = self.original_request.clone();
            req.request_body = self.request_body.clone();
        }
        let resp = host
            .intercept_stream_chunk(req, &self.hooks.lifecycle.skip_plugin, &self.hooks.lifecycle.scope)
            .await;
        self.raw = final_headers(&self.raw, &resp.headers);
        let payload = if resp.body.is_empty() { payload } else { resp.body };
        self.index += 1;
        if resp.drop_chunk {
            return None;
        }
        // Go `appendStreamInterceptorHistory`: the delivered chunks, bounded.
        self.history.push(payload.clone());
        while self.history.len() > HISTORY_CHUNKS || self.history.iter().map(Bytes::len).sum::<usize>() > HISTORY_BYTES
        {
            self.history.remove(0);
        }
        Some(payload)
    }

    /// Reads up to the first deliverable chunk (Go resolves header initialization
    /// before the response headers go out), then wraps the rest. An error before any
    /// delivered chunk fails the request.
    async fn run(
        mut self,
        headers: HeaderMap,
        first: Option<Bytes>,
        mut rest: ExecStream,
        trace: &Trace,
    ) -> Result<Done, ExecError> {
        let mut next = first.map(Ok);
        let first = loop {
            let item = match next.take() {
                Some(item) => Some(item),
                None => rest.next().await,
            };
            match item {
                Some(Ok(payload)) if payload.is_empty() => {}
                Some(Ok(payload)) => {
                    if let Some(payload) = self.chunk(payload).await {
                        break Some(payload);
                    }
                }
                Some(Err(error)) => return Err(error),
                None => {
                    self.header_init().await;
                    break None;
                }
            }
        };
        trace.set_intercepted_headers(header_map(&diff_headers(&self.base, &self.raw)));
        let headers = if self.raw == self.base {
            headers
        } else {
            header_map(&self.raw)
        };
        if first.is_none() {
            self.hooks.lifecycle.complete(COMPLETION_SUCCEEDED, 200, "");
            let rest = futures_util::stream::empty().boxed();
            return Ok(Done::Stream { headers, first, rest });
        }
        let rest = futures_util::stream::unfold(Some((self, rest)), |state| async move {
            let (mut this, mut rest) = state?;
            loop {
                match rest.next().await {
                    Some(Ok(payload)) if payload.is_empty() => {}
                    Some(Ok(payload)) => {
                        if let Some(payload) = this.chunk(payload).await {
                            return Some((Ok(payload), Some((this, rest))));
                        }
                    }
                    Some(Err(error)) => {
                        this.hooks.lifecycle.fail(&Failure::Exec(error.clone()), None);
                        return Some((Err(error), None));
                    }
                    None => {
                        this.hooks.lifecycle.complete(COMPLETION_SUCCEEDED, 200, "");
                        return None;
                    }
                }
            }
        })
        .boxed();
        Ok(Done::Stream { headers, first, rest })
    }
}

/// Go `WriteModelListResponse`: a model list goes through the response interceptors
/// (and a lifecycle of its own) before it is written. `response` is the built 200 list;
/// with `detail`, the answer is that model's entry from the final list. `caller` is the
/// authenticated client: Go keeps the request context, so nested `host.model.*` calls
/// from these interceptors record its identity.
pub(crate) async fn model_list(
    rt: &Runtime,
    caller: &Caller,
    source_format: &str,
    request_headers: &HeaderMap,
    response: axum::response::Response,
    detail: Option<&str>,
) -> axum::response::Response {
    let host = rt.plugins();
    if response.status() != axum::http::StatusCode::OK {
        return response;
    }
    let hooked = host.has_response_interceptors() || host.has_request_lifecycle_plugins();
    if !hooked && detail.is_none() {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(body) = axum::body::to_bytes(body, usize::MAX).await else {
        return axum::response::Response::from_parts(parts, axum::body::Body::empty());
    };
    if !hooked {
        let (status, body) = model_detail(body, detail.unwrap_or_default());
        parts.headers.remove(axum::http::header::CONTENT_LENGTH);
        parts.status = axum::http::StatusCode::from_u16(status).unwrap_or_default();
        return axum::response::Response::from_parts(parts, axum::body::Body::from(body));
    }
    let trace = Trace::with_request_id(crate::observability::current_request_id());
    let lifecycle = Lifecycle::start(
        rt,
        scope(
            &trace,
            super::go_request_header(request_headers),
            &Metadata::new(),
            Some(caller),
        ),
        source_format,
        "",
        "",
        false,
        Metadata::new(),
    );
    let raw: Header = [(
        "Content-Type".to_owned(),
        vec!["application/json; charset=utf-8".to_owned()],
    )]
    .into();
    let resp = host
        .intercept_response(
            ResponseInterceptRequest {
                request_id: lifecycle.request_id.clone(),
                source_format: source_format.to_owned(),
                request_headers: super::go_request_header(request_headers),
                response_headers: raw,
                body: body.clone(),
                status_code: 200,
                ..Default::default()
            },
            "",
            &lifecycle.scope,
        )
        .await;
    let body = if resp.body.is_empty() { body } else { resp.body };
    for (name, values) in &resp.headers {
        let Ok(name) = HeaderName::try_from(name.as_str()) else {
            continue;
        };
        parts.headers.remove(&name);
        for value in values {
            if let Ok(value) = HeaderValue::from_str(value) {
                parts.headers.append(name.clone(), value);
            }
        }
    }
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    if !parts.headers.contains_key(axum::http::header::CONTENT_TYPE) {
        parts.headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
    }
    let (status, body) = match detail {
        Some(id) => model_detail(body, id),
        None => (200, body),
    };
    // ponytail: Go reports encoding/json's error text for an undecodable catalog; this
    // reports a fixed one. Only a lifecycle plugin sees it.
    match status {
        200 => lifecycle.complete(COMPLETION_SUCCEEDED, 200, ""),
        404 => lifecycle.complete(COMPLETION_FAILED, 404, ""),
        _ => lifecycle.complete(COMPLETION_FAILED, status, "invalid model catalog"),
    }
    parts.status = axum::http::StatusCode::from_u16(status).unwrap_or_default();
    axum::response::Response::from_parts(parts, axum::body::Body::from(body))
}

/// The status and body Go writes for a model detail lookup in a final model list.
fn model_detail(body: bytes::Bytes, id: &str) -> (u16, bytes::Bytes) {
    match crate::models::select_detail(&body, id) {
        Ok(Some(entry)) => (200, body.slice_ref(entry)),
        Ok(None) => (
            404,
            bytes::Bytes::from_static(
                br#"{"error":{"code":"model_not_found","message":"Model not found","type":"invalid_request_error"}}"#,
            ),
        ),
        Err(()) => (
            502,
            bytes::Bytes::from_static(br#"{"error":{"message":"Invalid model catalog","type":"api_error"}}"#),
        ),
    }
}

/// Starts the request's lifecycle and runs the before-auth interceptors, rewriting
/// `call` (Go's handlers before `AuthManager.Execute*` or a plugin executor). `None`
/// when no plugin intercepts requests, observes their lifecycle or watches upstream
/// WebSocket frames: the request runs
/// untouched, as in Go where every hook is then a no-op.
pub(crate) async fn start(
    rt: &Runtime,
    trace: &Trace,
    call: &mut Call,
    model: &str,
) -> Result<Option<Arc<Hooks>>, Failure> {
    let host = rt.plugins();
    let needed = host.has_request_interceptors()
        || host.has_response_interceptors()
        || host.has_stream_interceptors()
        || host.has_request_lifecycle_plugins()
        || host.has_websocket_response_observers();
    if !needed {
        return Ok(None);
    }
    let source = super::execution::handler_type(call).to_owned();
    // Counting answers in the entry format (Go passes the handler type).
    let response = if call.operation == cpa_core::exec::Operation::CountTokens {
        source.clone()
    } else {
        super::execution::response_type(call).to_owned()
    };
    let headers = super::go_request_header(&call.headers);
    let (hooks, before) = Hooks::start(
        rt,
        trace,
        Request {
            source_format: &source,
            response_format: &response,
            model,
            requested_model: &call.model,
            stream: call.stream,
            headers: headers.clone(),
            body: &call.body,
            metadata: super::execution::execution_metadata(call, model),
            caller: Some(&call.caller),
        },
    )
    .await?;
    hooks.apply(call, &headers, before.headers, before.body, before.path);
    Ok(Some(hooks))
}

impl Hooks {
    /// Writes interceptor changes into the call the executors read.
    fn apply(&self, call: &mut Call, original: &Header, headers: Header, body: Option<Bytes>, path: Option<String>) {
        if &headers != original {
            call.headers = header_map(&headers);
        }
        if let Some(body) = body {
            call.body = body;
        }
        if let Some(path) = path {
            call.request_path = path;
        }
    }

    /// Go `applyRequestInterceptorsAfterPluginExecutorRoute`: the after-auth chain for a
    /// plugin executor, whose request format is `to_format`.
    pub(crate) async fn after_plugin_route(&self, to_format: &str, call: &mut Call) -> Result<(), Failure> {
        if !self.host.has_request_interceptors() {
            return Ok(());
        }
        let intercept = RequestInterceptRequest {
            request_id: self.lifecycle.request_id.clone(),
            trace_id: self.lifecycle.scope.request_id.clone(),
            source_format: self.source_format.clone(),
            to_format: to_format.to_owned(),
            model: self.model.clone(),
            requested_model: self.requested_model.clone(),
            stream: self.stream,
            headers: self.headers.clone(),
            body: call.body.clone(),
            metadata: self.metadata.clone(),
        };
        let resp = self
            .host
            .intercept_request_after_auth(intercept, &self.lifecycle.skip_plugin, &self.lifecycle.scope)
            .await;
        // As on the built-in path, the chain's clears do not survive the merge.
        let headers = cpa_plugin::interceptors::merge_headers(&self.headers, &resp.headers, &[]);
        let body = Some(resp.body.clone()).filter(|b| !b.is_empty());
        let path = Some(resp.path.trim().to_owned()).filter(|p| !p.is_empty());
        *self.after.lock().unwrap_or_else(|e| e.into_inner()) = Some(AfterAuth {
            headers: headers.clone(),
            body: body.clone(),
            path: path.clone(),
        });
        self.apply(call, &self.headers, headers, body, path);
        if resp.terminate {
            *self.terminated.lock().unwrap_or_else(|e| e.into_inner()) = Some("");
            return Err(terminated(&resp));
        }
        Ok(())
    }
}

/// Go `requestToFormat`: the request format an executor sends upstream.
fn request_to_format(
    provider: &str,
    source: &str,
    req: &ExecRequest,
    media: Option<crate::dispatch::MediaKind>,
) -> String {
    let format = |f: Format| f.as_str().to_owned();
    match provider {
        "gemini-interactions"
            if matches!(
                req.source_format,
                Format::Interactions | Format::OpenAI | Format::OpenAIResponse | Format::Claude | Format::Gemini
            ) =>
        {
            return format(Format::Interactions);
        }
        "gemini" | "gemini-interactions" => return format(Format::Gemini),
        "kimi" | "kimi-ai" | "kimi.ai" | "kimi.com" => {
            return match req.source_format {
                Format::Claude => format(Format::Claude),
                Format::OpenAIResponse => format(Format::OpenAIResponse),
                _ => format(Format::OpenAI),
            };
        }
        "devin" => return format(Format::Interactions),
        _ => {}
    }
    if media.is_some() {
        return source.to_owned();
    }
    if req.alt.as_deref() == Some("responses/compact") && !req.stream {
        return format(Format::OpenAIResponse);
    }
    match provider {
        "codex" | "xai" | "meta" => format(Format::Codex),
        "claude" => format(Format::Claude),
        "vertex" | "aistudio" => format(Format::Gemini),
        "antigravity" => format(Format::Antigravity),
        _ => format(Format::OpenAI),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpa_core::credential::{Credential, Source};
    use cpa_core::exec::{Caller, Operation, UsageSink};
    use serde_json::json;

    fn credential(metadata: Value, attributes: &[(&str, &str)]) -> Credential {
        Credential {
            id: "codex-user.json".into(),
            provider: "codex".into(),
            source: Source::Runtime,
            disabled: false,
            label: "user@example.com".into(),
            attributes: attributes.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect(),
            metadata: metadata.as_object().unwrap().clone(),
            revision: 0,
            next_refresh_after: None,
        }
    }

    fn request(usage: UsageSink) -> ExecRequest {
        ExecRequest {
            operation: Operation::Generate,
            source_format: Format::OpenAIResponse,
            response_format: Format::OpenAIResponse,
            stream_payloads: false,
            requested_model: "alias".into(),
            model: "gpt-upstream".into(),
            original_body: Bytes::new(),
            body: Bytes::new(),
            stream: true,
            alt: None,
            session: None,
            execution_session: None,
            derived_session: None,
            resolved_model: None,
            usage,
            request_path: "/v1/responses".into(),
            headers: HeaderMap::new(),
            caller: Caller {
                principal: String::new(),
                source: "authorization",
                provider: String::new(),
            },
        }
    }

    /// Go `EmitWebSocketResponseEvent` and the handler's observer closure: the request and
    /// trace IDs (a metadata string overrides the handler's), the metadata's requested
    /// model, the credential's ID, label and account type, and the frame's `type`.
    #[test]
    fn websocket_events_carry_the_attempt_and_its_credential() {
        let oauth = credential(
            json!({"type": "codex", "email": "user@example.com", "access_token": "t"}),
            &[],
        );
        let mut base = WebSocketEventBase {
            request_id: "lifecycle-req".into(),
            trace_id: "trace-1".into(),
            source_format: "openai-response".into(),
            metadata: Metadata::new(),
        };
        let frame = br#"{"type":"response.created","response":{}}"#;
        let event = base.event("codex", &oauth, "gpt-upstream", frame);
        let ids = |e: &WebSocketResponseEvent| (e.request_id.clone(), e.trace_id.clone(), e.requested_model.clone());
        assert_eq!(
            ids(&event),
            ("lifecycle-req".into(), "trace-1".into(), "gpt-upstream".into())
        );
        assert_eq!(
            (
                event.auth_id.as_str(),
                event.auth_label.as_str(),
                event.auth_type.as_str()
            ),
            ("codex-user.json", "user@example.com", "oauth")
        );
        assert_eq!(
            (event.provider.as_str(), event.event_type.as_str()),
            ("codex", "response.created")
        );
        assert_eq!(event.payload.as_ref(), frame);

        for (key, value) in [
            ("request_id", "meta-req"),
            ("trace_id", "meta-trace"),
            ("requested_model", " requested "),
        ] {
            base.metadata.insert(key.into(), Value::String(value.into()));
        }
        let api_key = credential(json!({"type": "codex"}), &[("api_key", "sk-fake")]);
        let event = base.event("codex", &api_key, "gpt-upstream", b"not json");
        assert_eq!(
            ids(&event),
            ("meta-req".into(), "meta-trace".into(), "requested".into())
        );
        assert_eq!((event.auth_type.as_str(), event.event_type.as_str()), ("api_key", ""));
        assert_eq!(event.metadata, base.metadata);
    }

    /// Without an observer an attempt has no emitter; an empty frame is not reported.
    #[tokio::test]
    async fn websocket_emitters_skip_empty_frames_and_absent_observers() {
        #[derive(Default)]
        struct Count(std::sync::atomic::AtomicUsize);
        impl cpa_core::exec::WebSocketObserver for Count {
            fn response<'a>(
                &'a self,
                _: &'a str,
                _: &'a Credential,
                _: &'a str,
                _: &'a [u8],
            ) -> futures_util::future::BoxFuture<'a, ()> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async {})
            }
        }
        let cred = credential(json!({"type": "codex"}), &[("api_key", "sk-fake")]);
        assert!(
            request(UsageSink::default())
                .websocket_emitter("codex", &cred)
                .is_none()
        );
        let count = Arc::new(Count::default());
        let req = request(UsageSink::default().with_websocket(Some(count.clone())));
        let emitter = req.websocket_emitter("codex", &cred).unwrap();
        emitter.emit(b"").await;
        assert_eq!(count.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        emitter.emit(b"{}").await;
        assert_eq!(count.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Go detaches the observer from the request (`context.WithoutCancel`): an observer
    /// that calls `host.http.do` after its request was canceled still gets through.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    // Only CI's media relay job enables the feature, and it has no Go toolchain; the
    // workspace test job runs this test, where CPA_TEST_NO_SKIP fails a missing one.
    #[cfg_attr(feature = "media-relay", ignore = "needs the Go plugin toolchain")]
    async fn websocket_observers_outlive_the_request_cancellation() {
        let tmp = std::env::temp_dir();
        let Some(built) = cpa_plugin::testing::built_plugins(&tmp, "websocket_observers_outlive") else {
            return;
        };
        let dir = cpa_plugin::testing::scratch(&tmp, "ws-observer-cancel");
        let (auths, plugins, records) = (dir.join("auths"), dir.join("plugins"), dir.join("records"));
        for d in [&auths, &plugins, &records.join("respond/watcher")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::copy(built.join("recorder.so"), plugins.join("watcher.so")).unwrap();
        let ping = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/ping", listener.local_addr().unwrap());
            let app = axum::Router::new().route("/ping", axum::routing::get(|| async { "pong" }));
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            url
        };
        let respond = json!({"ok": true, "result": {}, "host_calls": [
            {"method": "host.http.do", "request": {"method": "GET", "url": ping}}
        ]});
        std::fs::write(
            records.join("respond/watcher/websocket.response_event.json"),
            respond.to_string(),
        )
        .unwrap();
        let yaml = format!(
            "auth-dir: {}\nplugins:\n  enabled: true\n  dir: {}\n  configs:\n    watcher:\n      enabled: true\n      label: watcher\n      record: {}\n      caps: websocket_response_observer\n",
            auths.display(),
            plugins.display(),
            records.display()
        );
        let rt = Arc::new(crate::testing::runtime(
            cpa_core::config::Config::parse(&yaml).unwrap(),
            Vec::new(),
            cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        crate::plugins::start(&rt).await;
        let body = Bytes::from_static(b"{}");
        let Ok((hooks, _)) = Hooks::start(
            &rt,
            &Trace::default(),
            Request {
                source_format: "openai-response",
                response_format: "openai-response",
                model: "gpt-upstream",
                requested_model: "gpt-upstream",
                stream: true,
                headers: Header::new(),
                body: &body,
                metadata: Metadata::new(),
                caller: None,
            },
        )
        .await
        else {
            panic!("hooks did not start");
        };
        let cred = credential(json!({"type": "codex"}), &[("api_key", "sk-fake")]);
        let observer = hooks
            .websocket_observer(&request(UsageSink::default()), &cred, "gpt-upstream")
            .expect("an observer plugin is loaded");
        // The request ends (its lifecycle cancels its scope) before the frame arrives.
        hooks.lifecycle.scope.cancel.cancel();
        observer
            .response("codex", &cred, "gpt-upstream", br#"{"type":"response.created"}"#)
            .await;
        let log = std::fs::read_to_string(records.join("watcher.jsonl")).unwrap();
        let callback: Value = log
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|r| r["method"] == "host_callback")
            .map(|r| serde_json::from_str(r["request"].as_str().unwrap()).unwrap())
            .expect("the observer made its host.http.do call");
        assert_eq!(callback["ok"], true, "{callback}");
        assert_eq!(callback["result"]["StatusCode"], 200, "{callback}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
