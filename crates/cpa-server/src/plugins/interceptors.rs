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
use cpa_core::exec::{ExecError, ExecRequest, ExecStream, FailureScope};
use cpa_core::format::Format;
use cpa_plugin::api::{
    COMPLETION_CANCELED, COMPLETION_FAILED, COMPLETION_REJECTED, COMPLETION_SUCCEEDED, RequestCompletion,
    RequestInterceptRequest, RequestInterceptResponse, ResponseInterceptRequest, STREAM_CHUNK_HEADER_INIT_INDEX,
    StreamChunkInterceptRequest,
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
}

impl Lifecycle {
    pub(crate) fn start(
        rt: &Runtime,
        trace: &Trace,
        source_format: &str,
        model: &str,
        requested_model: &str,
        stream: bool,
        metadata: Metadata,
    ) -> Arc<Self> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let scope = scope(trace);
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
        self.host.complete_request(completion, "", &self.scope);
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
        self.complete(COMPLETION_CANCELED, 0, "context canceled");
    }
}

fn now() -> GoTime {
    GoTime(Some(chrono::Local::now().fixed_offset()))
}

fn scope(trace: &Trace) -> RequestScope {
    RequestScope {
        request_id: trace.request_id(),
        capture: trace.capture(),
        ..Default::default()
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
fn go_header(headers: &HeaderMap) -> Header {
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
    let resp = host.intercept_request_before_auth(req, "", &lifecycle.scope).await;
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
    /// The lifecycle error of a plugin termination: Go's after-auth termination is an
    /// error ("request terminated by plugin"); a plugin-executor-route one has none.
    terminated: Mutex<Option<&'static str>>,
    /// Built-in executors frame OpenAI-style events; Go's chunks are their payloads.
    builtin: bool,
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
    pub builtin: bool,
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
            trace,
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
            terminated: Mutex::new(None),
            builtin: req.builtin,
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
        if !self.host.has_request_interceptors() {
            return Ok(());
        }
        let provider = crate::registry::provider_key(credential);
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
            .intercept_request_after_auth(intercept, "", &self.lifecycle.scope)
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
        }
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
                        "",
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
                "",
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
        // Go's executors hand OpenAI and Gemini handlers bare payloads, which the handler
        // frames; built-in executors here send them framed.
        let payload = match self.hooks.response_format.as_str() {
            "openai" | "gemini" if self.hooks.builtin => crate::respond::data_payload(&payload)
                .map(Bytes::from)
                .unwrap_or(payload),
            _ => payload,
        };
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
        let resp = host.intercept_stream_chunk(req, "", &self.hooks.lifecycle.scope).await;
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
/// (and a lifecycle of its own) before it is written. `response` is the built 200 list.
pub(crate) async fn model_list(
    rt: &Runtime,
    source_format: &str,
    request_headers: &HeaderMap,
    response: axum::response::Response,
) -> axum::response::Response {
    let host = rt.plugins();
    if response.status() != axum::http::StatusCode::OK
        || !(host.has_response_interceptors() || host.has_request_lifecycle_plugins())
    {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(body) = axum::body::to_bytes(body, usize::MAX).await else {
        return axum::response::Response::from_parts(parts, axum::body::Body::empty());
    };
    let trace = Trace::with_request_id(crate::observability::current_request_id());
    let lifecycle = Lifecycle::start(rt, &trace, source_format, "", "", false, Metadata::new());
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
    lifecycle.complete(COMPLETION_SUCCEEDED, 200, "");
    axum::response::Response::from_parts(parts, axum::body::Body::from(body))
}

/// Starts the request's lifecycle and runs the before-auth interceptors, rewriting
/// `call` (Go's handlers before `AuthManager.Execute*` or a plugin executor). `None`
/// when no plugin intercepts requests or observes their lifecycle: the request runs
/// untouched, as in Go where every hook is then a no-op.
pub(crate) async fn start(
    rt: &Runtime,
    trace: &Trace,
    call: &mut Call,
    model: &str,
    builtin: bool,
) -> Result<Option<Arc<Hooks>>, Failure> {
    let host = rt.plugins();
    let needed = host.has_request_interceptors()
        || host.has_response_interceptors()
        || host.has_stream_interceptors()
        || host.has_request_lifecycle_plugins();
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
            builtin,
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
            .intercept_request_after_auth(intercept, "", &self.lifecycle.scope)
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
