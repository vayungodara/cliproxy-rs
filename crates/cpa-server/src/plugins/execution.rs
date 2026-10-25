//! Plugin model routing and the plugin executors a router targets (Go
//! sdk/api/handlers/handlers_routing.go `applyModelRouter`, handlers_execution.go
//! `executeWithPluginExecutor` / `countWithPluginExecutor`, handlers_stream.go
//! `streamWithPluginExecutor`, and internal/pluginhost/adapters_executors.go's
//! `executorAdapter` called without an auth).
//!
//! ponytail: plugin response translators (`hasResponseTranslator`) are counted
//! in negotiation but not called, and the Responses SSE validation of plugin streams is
//! the route's own.

use std::collections::VecDeque;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use cpa_core::exec::{ExecError, ExecStream, FailureScope, Operation};
use cpa_core::format::Format;
use cpa_plugin::api::{ExecutorRequest, ModelRouteRequest};
use cpa_plugin::callbacks::RequestScope;
use cpa_plugin::executor::Negotiated;
use cpa_plugin::gojson::{Header, Metadata};
use cpa_plugin::rpc::CallError;
use cpa_plugin::streams::Chunk;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use serde_json::Value;

use crate::Runtime;
use crate::dispatch::{Call, Done, Failure, Trace};

/// Go `modelRouteDecision`.
pub(crate) enum Route {
    /// `self` / `executor`: the plugin whose executor serves the request.
    Executor(String),
    /// `provider`: a built-in provider with credentials, and the model to send (`None`
    /// keeps the client's model).
    Provider { provider: String, model: Option<String> },
}

tokio::task_local! {
    /// The client request's raw query (Go reads `URL.Query()` from the gin context).
    static QUERY: String;
}

/// Runs `f` with the client request's query visible to plugin requests.
pub(crate) async fn with_query<F: std::future::Future>(query: &str, f: F) -> F::Output {
    QUERY.scope(query.to_owned(), f).await
}

/// The client request's raw query; empty outside an HTTP request (a WebSocket turn).
pub(crate) fn query() -> String {
    QUERY.try_with(Clone::clone).unwrap_or_default()
}

/// Cancels the request scope's callbacks when the request ends (Go's request context).
struct Scope(RequestScope);

impl Scope {
    fn new(trace: &Trace, call: &Call) -> Self {
        let inherited = super::model_callbacks::context().map(|c| c.scope).unwrap_or_default();
        let (api_key, access_provider) = super::model_callbacks::usage_identity(Some(&call.caller), &inherited);
        Self(RequestScope {
            request_id: trace.request_id(),
            capture: trace.capture(),
            cancel: inherited.cancel.child_token(),
            headers: super::go_request_header(&call.headers),
            query: super::go_values(&query()),
            pinned_auth_id: call.pinned(),
            execution_session: call.execution_session.clone(),
            request_path: call.request_path.clone(),
            api_key,
            access_provider,
        })
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        self.0.cancel.cancel();
    }
}

/// Go's handler type (entry protocol) of a call: media routes have their own.
pub(crate) fn handler_type(call: &Call) -> &'static str {
    match call.media.as_ref().map(|m| m.kind) {
        Some(crate::dispatch::MediaKind::Images) => "openai-image",
        Some(crate::dispatch::MediaKind::Videos) => "openai-video",
        Some(crate::dispatch::MediaKind::Speech) => "openai-speech",
        None => call.entry.as_str(),
    }
}

/// Go's response protocol of a call: a media route answers in its handler type.
pub(crate) fn response_type(call: &Call) -> &'static str {
    if call.media.is_some() {
        handler_type(call)
    } else {
        call.response.as_str()
    }
}

/// Go `requestExecutionMetadata`: what a plugin may see of the request context.
/// Go's callback entries (selected-auth callbacks) never reach a plugin: they do not
/// encode.
fn request_metadata(call: &Call) -> Metadata {
    let mut meta = Metadata::new();
    let idempotency = call
        .headers
        .get("idempotency-key")
        .map(|v| String::from_utf8_lossy(v.as_bytes()).trim().to_owned())
        .unwrap_or_default();
    if !idempotency.is_empty() {
        meta.insert("idempotency_key".into(), Value::String(idempotency));
    }
    let path = call.request_path.trim();
    if !path.is_empty() {
        meta.insert("request_path".into(), Value::String(path.to_owned()));
    }
    if let Some(pinned) = call.pinned().filter(|p| !p.is_empty()) {
        meta.insert("pinned_auth_id".into(), Value::String(pinned.to_owned()));
    }
    if let Some(session) = call.execution_session.as_deref().filter(|s| !s.is_empty()) {
        meta.insert("execution_session_id".into(), Value::String(session.to_owned()));
    }
    let scope = cpa_common::session::caller_scope(&call.caller.principal);
    if !scope.is_empty() {
        meta.insert("caller_scope".into(), Value::String(scope));
    }
    meta
}

/// Go `AuthManager.AvailableProviders`: providers with an enabled credential, by their
/// scheduling key, sorted. Go folds `kimi.com` and `kimi.ai` into `kimi` and `kimi-ai`
/// here and in selection; selection here matches the key as is, so routers are offered
/// the keys a routed provider can actually select.
fn available_providers(rt: &Runtime) -> Vec<String> {
    let mut out: Vec<String> = rt
        .store()
        .snapshot()
        .iter()
        .filter(|c| !c.disabled)
        .map(|c| crate::registry::provider_key(c))
        .filter(|p| !p.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Go `applyModelRouter`: the first router that handles the request with a usable
/// target decides where it goes.
pub(crate) async fn route(rt: &Runtime, call: &Call, query: &str, trace: &Trace) -> Option<Route> {
    let host = rt.plugins();
    let skip = super::model_callbacks::skip_plugin();
    if !host.has_model_routers(&skip) {
        return None;
    }
    let mut metadata = request_metadata(call);
    metadata.insert("requested_model".into(), Value::String(call.model.clone()));
    let request = ModelRouteRequest {
        source_format: handler_type(call).to_owned(),
        requested_model: call.model.clone(),
        stream: call.stream,
        headers: super::go_request_header(&call.headers),
        query: super::go_values(query),
        body: call.body.clone(),
        metadata,
        ..Default::default()
    };
    let scope = Scope::new(trace, call);
    let resp = host
        .route_model(request, &skip, &available_providers(rt), &scope.0)
        .await?;
    Some(match resp.target_kind.as_str() {
        "provider" => Route::Provider {
            provider: resp.target,
            model: Some(resp.target_model.trim().to_owned()).filter(|m| !m.is_empty()),
        },
        _ => Route::Executor(resp.target),
    })
}

fn local(status: u16, message: impl Into<String>) -> Failure {
    Failure::Exec(ExecError::local(status, FailureScope::Request, message))
}

/// Go `executionErrorMessage` of an executor error: the plugin's HTTP status, else 500.
fn call_failure(e: CallError) -> Failure {
    local(e.status(), e.to_string())
}

/// Go `executeWithPluginExecutor`, `countWithPluginExecutor` and
/// `streamWithPluginExecutor`: the plugin's executor serves the request directly, with
/// Go's format negotiation and translation around it.
pub(crate) async fn execute(
    rt: &Runtime,
    call: &Call,
    plugin_id: &str,
    query: &str,
    trace: &Trace,
) -> Result<Done, Failure> {
    if rt.remote_dispatch().is_some() {
        return Err(local(
            503,
            "plugin executor routing is unavailable while Home is enabled",
        ));
    }
    let mut call = call.clone();
    let model = call.model.clone();
    let hooks = super::interceptors::start(rt, trace, &mut call, &model).await?;
    let result = run_executor(rt, &mut call, plugin_id, query, trace, hooks.as_deref()).await;
    let result = match hooks {
        Some(hooks) => {
            let result = hooks.prime(result, trace).await;
            hooks.finish(result, trace).await
        }
        None => result,
    };
    // The OpenAI handler frames each chunk after the stream interceptors saw it.
    match result {
        Ok(Done::Stream { headers, first, rest })
            if call.response == Format::OpenAI
                && call.media.is_none()
                && super::model_callbacks::context().is_none() =>
        {
            Ok(Done::Stream {
                headers,
                first: first.and_then(|f| openai_frame(&f)),
                rest: rest
                    .filter_map(|item| async move {
                        match item {
                            Ok(chunk) => openai_frame(&chunk).map(Ok),
                            Err(e) => Some(Err(e)),
                        }
                    })
                    .boxed(),
            })
        }
        other => other,
    }
}

/// Go's OpenAI handler writes every chunk as `data: %s\n\n`, even one that is already a
/// frame; empty chunks are dropped.
fn openai_frame(chunk: &Bytes) -> Option<Bytes> {
    cpa_translate::stream::frame(Format::OpenAI, chunk).map(Bytes::from)
}

async fn run_executor(
    rt: &Runtime,
    call: &mut Call,
    plugin_id: &str,
    query: &str,
    trace: &Trace,
    hooks: Option<&super::interceptors::Hooks>,
) -> Result<Done, Failure> {
    let host = rt.plugins();
    let count = call.operation == Operation::CountTokens;
    let stream = call.stream && !count;
    // upstream.claude.tool-search, pinned for this call's request and response.
    let tool_search = cpa_exec::claude::tool_search_enabled(&rt.config());
    // Counting answers in the entry format (Go passes the handler type twice).
    let response = if count { handler_type(call) } else { response_type(call) };
    let adapter = host.executor_adapter(plugin_id).await.map_err(|e| local(0, e))?;
    let negotiated = adapter
        .negotiate(handler_type(call), response, host.has_response_translator())
        .map_err(|e| local(0, e))?;
    if let Some(hooks) = hooks {
        hooks.after_plugin_route(&negotiated.input, call).await?;
    }
    let payload = if negotiated.input_requested == negotiated.input {
        call.body.clone()
    } else {
        translate_request(
            &negotiated.input_requested,
            &negotiated.input,
            &call.model,
            &call.body,
            stream,
            tool_search,
        )
        .map_err(Failure::Exec)?
    };
    let request = ExecutorRequest {
        model: call.model.clone(),
        format: negotiated.output.clone(),
        stream,
        alt: call.alt.clone().unwrap_or_default(),
        headers: super::go_request_header(&call.headers),
        query: super::go_values(query),
        original_request: call.body.clone(),
        source_format: negotiated.input.clone(),
        payload,
        metadata: execution_metadata(call, &call.model),
        ..Default::default()
    };
    let scope = Scope::new(trace, call);
    if !stream {
        let resp = if count {
            host.executor_count_tokens(&adapter, &request, &scope.0).await
        } else {
            host.executor_execute(&adapter, &request, &scope.0).await
        }
        .map_err(call_failure)?;
        return Ok(Done::Buffered {
            headers: header_map(&resp.headers),
            body: translate_response(&negotiated, &request, resp.payload, tool_search),
        });
    }
    let opened = host
        .executor_execute_stream(&adapter, &request, &scope.0)
        .await
        .map_err(call_failure)?;
    // Go creates no usage reporter without an auth.
    let mut rest = translate_stream(
        negotiated,
        request,
        opened.chunks,
        scope,
        |error| ExecError::local(0, FailureScope::Request, error),
        None,
        tool_search,
    );
    let first = match rest.next().await {
        Some(Ok(event)) => Some(event),
        Some(Err(error)) => return Err(Failure::Exec(error)),
        None => None,
    };
    Ok(Done::Stream {
        headers: header_map(&opened.headers),
        first,
        rest,
    })
}

/// Go `executorAdapter.Execute`, `ExecuteStream` and `CountTokens` with an auth: a
/// credential whose provider a plugin executor registered, chosen by the ordinary
/// selection loop. `adapter` is the registered executor; a plugin replaced since then
/// does not serve it (Go `pluginIdentityCurrent`). The answer is in the client's
/// format, as built-in executors give it.
/// Usage goes through the attempt's reporter as Go's `NewExecutorUsageReporter` takes
/// it: parsed from the plugin's own output format, not the client's.
pub(crate) async fn execute_credential(
    rt: &Runtime,
    call: &Call,
    adapter: &cpa_plugin::executor::Adapter,
    credential: &cpa_core::credential::Credential,
    private: &[String],
    req: cpa_core::exec::ExecRequest,
) -> Result<cpa_core::exec::ExecResponse, ExecError> {
    let host = rt.plugins();
    let count = req.operation == Operation::CountTokens;
    let stream = req.stream && !count;
    // upstream.claude.tool-search, pinned for this call's request and response.
    let tool_search = cpa_exec::claude::tool_search_enabled(&rt.config());
    let source = handler_type(call);
    let response = if count { source } else { response_type(call) };
    // Go creates the attempt's reporter before `prepareExecutorCall`, so a failure to
    // negotiate or translate is recorded as the plugin executor's; counting has none.
    if !count {
        req.usage.plugin_executor(&adapter.provider);
    }
    let negotiated = adapter
        .negotiate(source, response, host.has_response_translator())
        .map_err(|e| attempt_error(0, e))?;
    let payload = if negotiated.input_requested == negotiated.input {
        req.body.clone()
    } else {
        translate_request(
            &negotiated.input_requested,
            &negotiated.input,
            &req.model,
            &req.body,
            stream,
            tool_search,
        )?
    };
    // Go `storageJSONFromAuth` without a plugin token storage: the metadata as JSON.
    let auth_metadata: Metadata = credential.metadata.clone().into_iter().collect();
    // Go `applyRequestAfterAuthInterceptor` rewrites only the path in the metadata; the
    // handler's reasoning effort, service tier and generate facts stay.
    let mut metadata = execution_metadata(call, &req.model);
    if !req.request_path.trim().is_empty() {
        metadata.insert("request_path".into(), Value::String(req.request_path.trim().to_owned()));
    }
    let request = ExecutorRequest {
        auth_id: credential.id.clone(),
        auth_provider: credential.provider.clone(),
        model: req.model.clone(),
        format: negotiated.output.clone(),
        stream,
        alt: req.alt.clone().unwrap_or_default(),
        headers: super::go_request_header(&req.headers),
        query: super::go_values(&query()),
        original_request: req.original_body.clone(),
        source_format: negotiated.input.clone(),
        payload,
        metadata,
        storage_json: Bytes::from(cpa_plugin::gojson::to_vec(&auth_metadata)),
        auth_metadata,
        auth_attributes: plugin_attributes(credential, private),
    };
    // The router path's scope (`Scope::new`) with the post-auth headers and path, so
    // nested `host.model.*` calls keep the pin, session and request context.
    let inherited = super::model_callbacks::context().map(|c| c.scope).unwrap_or_default();
    let (api_key, access_provider) = super::model_callbacks::usage_identity(Some(&call.caller), &inherited);
    let scope = Scope(RequestScope {
        request_id: crate::observability::current_request_id().unwrap_or_default(),
        capture: req.capture().clone(),
        cancel: inherited.cancel.child_token(),
        headers: super::go_request_header(&req.headers),
        query: super::go_values(&query()),
        pinned_auth_id: call.pinned(),
        execution_session: call.execution_session.clone(),
        request_path: req.request_path.clone(),
        api_key,
        access_provider,
    });
    // Go `SetTranslatedReasoningEffort` and `StartResponseTTFT`; counting has no
    // reporter.
    if !count {
        req.usage.request_for(&negotiated.input, &request.payload);
        req.usage.round_trip_started();
    }
    if !stream {
        let resp = if count {
            host.executor_count_tokens(adapter, &request, &scope.0).await
        } else {
            host.executor_execute(adapter, &request, &scope.0).await
        }
        .map_err(|e| attempt_error(e.status(), e.to_string()))?;
        if !count {
            // Go `RecordFirstPacket`, `Publish` and `EnsurePublished`.
            req.usage.token_event(false);
            req.usage.plugin_response_body(&negotiated.output, &resp.payload);
            req.usage.publish();
        }
        return Ok(cpa_core::exec::ExecResponse {
            status: 200,
            headers: header_map(&resp.headers),
            body: cpa_core::exec::ResponseBody::Buffered(translate_response(
                &negotiated,
                &request,
                resp.payload,
                tool_search,
            )),
        });
    }
    let opened = host
        .executor_execute_stream(adapter, &request, &scope.0)
        .await
        .map_err(|e| attempt_error(e.status(), e.to_string()))?;
    let headers = header_map(&opened.headers);
    Ok(cpa_core::exec::ExecResponse {
        status: 200,
        headers,
        body: cpa_core::exec::ResponseBody::Stream(translate_stream(
            negotiated,
            request,
            opened.chunks,
            scope,
            |error| attempt_error(0, error),
            Some(req.usage.clone()),
            tool_search,
        )),
    })
}

/// A credential-bound plugin executor's failure as an attempt on that credential. Go's
/// `rpcError` carries a status but neither `IsRequestScoped` nor `IsCredentialScoped`,
/// and a plain error (an unavailable executor, a failed negotiation, an undecodable
/// answer) carries neither; the auth manager classifies both by status and body. So
/// the error is model-scoped with the plugin's status (0 when it gave none) and the
/// shared classifier (`classify.rs`) decides failover, cooldown and retry rounds,
/// including Go's message fallbacks for statusless errors.
fn attempt_error(status: u16, message: impl Into<String>) -> ExecError {
    ExecError::local(status, FailureScope::Model, message)
}

/// What a plugin scheduler learns about a handler request (Go `schedulerOptions` at
/// `pickNextMixedLegacy`): the client headers, and the execution metadata with the
/// session-affinity namespace `mixed` and, under session affinity, the route model.
pub(crate) fn scheduler_context(
    call: &Call,
    model: &str,
    session_affinity: bool,
    derived: Option<&str>,
) -> super::SchedulerContext {
    let mut metadata = execution_metadata(call, model);
    // Go `session.Enrich` (its branches, not the affinity resolver's): a client-named
    // session (`hasExplicitSession`) is canonical with its parent; otherwise the
    // execution session is; otherwise only the derived identity is named.
    let bound = |s: &str| Value::String(cpa_common::session::bound_session_identity(s));
    let execution = call
        .execution_session
        .as_deref()
        .map(cpa_common::session::normalize_explicit_id)
        .unwrap_or_default();
    if cpa_common::session::has_explicit_session(&call.headers, &call.body) {
        let execution = (!execution.is_empty()).then_some(execution.as_str());
        if let Some(info) = cpa_common::session::extract_session_info(&call.headers, &call.body, execution)
            .filter(|info| !info.session_id.is_empty())
        {
            metadata.insert("canonical_session_id".into(), bound(&info.session_id));
            if !info.parent_session_id.is_empty() && info.parent_session_id != info.session_id {
                metadata.insert("parent_session_id".into(), bound(&info.parent_session_id));
            }
        }
    } else if !execution.is_empty() {
        metadata.insert("canonical_session_id".into(), bound(&format!("execution:{execution}")));
    } else if let Some(derived) = derived.filter(|d| !d.is_empty()) {
        metadata.insert("derived_session_id".into(), Value::String(derived.to_owned()));
    }
    metadata.insert("session_affinity_provider".into(), Value::String("mixed".into()));
    let affinity_model = if session_affinity {
        model.to_owned()
    } else {
        String::new()
    };
    metadata.insert("session_affinity_model".into(), Value::String(affinity_model));
    super::SchedulerContext {
        stream: call.stream,
        options: cpa_plugin::api::SchedulerOptions {
            headers: super::go_request_header(&call.headers),
            metadata,
        },
    }
}

/// request context plus the request's model facts; `model` is the resolved model.
pub(crate) fn execution_metadata(call: &Call, model: &str) -> Metadata {
    let mut meta = request_metadata(call);
    if super::model_callbacks::context().is_some() {
        meta.insert("source".into(), Value::String("plugin_host_model_callback".into()));
    }
    meta.insert("requested_model".into(), Value::String(call.model.clone()));
    if let Some(model) = call.selection_model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        meta.insert("auth_selection_model".into(), Value::String(model.to_owned()));
    }
    let effort = cpa_common::thinking::extract_reasoning_effort(&call.body, handler_type(call), model);
    if !effort.is_empty() {
        meta.insert("reasoning_effort".into(), Value::String(effort));
    }
    // Go `setServiceTierMetadata` and `setGenerateMetadata`.
    let tier = cpa_common::json::get(&call.body, "service_tier");
    let tier = if tier.exists() {
        tier.str().trim().to_owned()
    } else {
        String::new()
    };
    let tier = if tier.is_empty() { "auto".to_owned() } else { tier };
    meta.insert("service_tier".into(), Value::String(tier));
    let generate = cpa_common::json::get(&call.body, "generate");
    let generate = !(generate.exists() && generate.is_bool() && generate.bytes().as_ref() == b"false");
    meta.insert("generate".into(), Value::Bool(generate));
    meta
}

/// Go `sdktranslator.TranslateRequestEnvelope` between two executor format names
/// (`prepareExecutorCall`). A refusal, such as an unsupported content part, is a
/// request-scoped 400 and nothing is sent, as on the built-in path.
fn translate_request(
    from: &str,
    to: &str,
    model: &str,
    body: &Bytes,
    stream: bool,
    tool_search: bool,
) -> Result<Bytes, ExecError> {
    let ctx = cpa_translate::RequestCtx {
        model,
        stream,
        tool_search,
    };
    cpa_translate::translate_request_named(from, to, &ctx, body)
        .map(Bytes::from)
        .map_err(|e| ExecError::local(400, FailureScope::Request, e.to_string()))
}

/// The plugin's `http.Header` as response headers (invalid names or values dropped).
fn header_map(headers: &Header) -> HeaderMap {
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

fn ensure_usage(responses: bool, payload: Bytes) -> Bytes {
    if responses {
        Bytes::from(cpa_exec::ensure_responses_usage_details(&payload))
    } else {
        payload
    }
}

/// Go `translateExecutorResponse` (non-stream).
fn translate_response(n: &Negotiated, req: &ExecutorRequest, payload: Bytes, tool_search: bool) -> Bytes {
    let responses = n.requested == Format::OpenAIResponse.as_str();
    if n.requested.is_empty() || n.output == n.requested {
        return ensure_usage(responses, payload);
    }
    let pair = Format::parse(&n.requested)
        .zip(Format::parse(&n.output))
        .and_then(|(to, from)| cpa_translate::pair(to, from));
    let original = if req.original_request.is_empty() {
        &req.payload
    } else {
        &req.original_request
    };
    let ctx = cpa_translate::ResponseCtx {
        model: &req.model,
        original_request: original,
        translated_request: &req.payload,
        tool_search,
    };
    let out = match pair {
        Some(pair) => (pair.non_stream)(&ctx, &payload).map(Bytes::from).unwrap_or(payload),
        // Go's registry returns an unregistered pair's body unchanged, after the plugin
        // response hooks.
        None => cpa_translate::hooks::unregistered_non_stream(&n.output, &n.requested, &ctx, &payload)
            .map_or(payload, Bytes::from),
    };
    ensure_usage(responses, out)
}

/// Go `translateExecutorStreamChunks` and `mapExecutorStreamChunks`, plus the OpenAI
/// handler's `data: %s` framing of each chunk.
fn translate_stream(
    n: Negotiated,
    req: ExecutorRequest,
    chunks: BoxStream<'static, Chunk>,
    scope: Scope,
    chunk_error: fn(String) -> ExecError,
    usage: Option<cpa_core::exec::UsageSink>,
    tool_search: bool,
) -> ExecStream {
    let translating = !n.requested.is_empty() && n.output != n.requested;
    let native = translating
        && Format::parse(&n.requested)
            .zip(Format::parse(&n.output))
            .is_some_and(|(to, from)| cpa_translate::go_stream(to, from).is_some());
    let translator = translating
        .then(|| {
            let original = if req.original_request.is_empty() {
                &req.payload
            } else {
                &req.original_request
            };
            let ctx = cpa_translate::ResponseCtx {
                model: &req.model,
                original_request: original,
                translated_request: &req.payload,
                tool_search,
            };
            cpa_translate::hooked_go_stream(&n.requested, &n.output, &ctx)
        })
        .flatten();
    let usage = usage.map(|sink| StreamUsage {
        sink,
        protocol: n.output.clone(),
        buffer: Vec::new(),
        scan: JsonScan::default(),
        ended: false,
    });
    let state = StreamState {
        usage,
        native,
        translator,
        passthrough: n.requested.is_empty()
            || (n.output == n.requested && n.requested != Format::OpenAIResponse.as_str()),
        responses: n.requested == Format::OpenAIResponse.as_str(),
        openai_tail: translating && n.output == Format::OpenAI.as_str(),
        chunks,
        ready: VecDeque::new(),
        done: false,
        chunk_error,
        _scope: scope,
    };
    Box::pin(futures_util::stream::unfold(state, |mut s| async move {
        loop {
            if let Some(item) = s.ready.pop_front() {
                return Some((item, s));
            }
            if s.done {
                return None;
            }
            match s.chunks.next().await {
                Some(chunk) => {
                    if let Some(usage) = &mut s.usage {
                        match &chunk.error {
                            // Published before the error reaches the reader.
                            Some(error) => usage.end(Some(error)),
                            None => usage.chunk(&chunk.payload),
                        }
                    }
                    if !chunk.payload.is_empty() {
                        for frame in s.frames(&chunk.payload) {
                            s.push(frame);
                        }
                    }
                    if let Some(error) = chunk.error {
                        s.ready.push_back(Err((s.chunk_error)(error)));
                        s.done = true;
                    }
                }
                None => {
                    if let Some(usage) = &mut s.usage {
                        usage.end(None);
                    }
                    // Go `emitTranslatedExecutorStreamTail`.
                    if s.openai_tail {
                        for frame in s.frames(b"data: [DONE]") {
                            s.push(frame);
                        }
                    }
                    s.done = true;
                }
            }
        }
    }))
}

/// Go `observeAndTranslateExecutorStream`'s usage side for a credential-bound stream:
/// usage from the plugin's output lines, published once when the stream ends, fails
/// or is dropped (Go's canceled context).
/// ponytail: TTFT is the first chunk (Go `RecordFirstPacket`); Go's per-protocol token
/// detection (`Observe*TokenEvent`) is not ported, as for the built-in executors.
struct StreamUsage {
    sink: cpa_core::exec::UsageSink,
    protocol: String,
    /// The part of the last line not yet ended by a newline.
    buffer: Vec<u8>,
    /// Bracket and string state of `buffer`, advanced over each new byte only.
    scan: JsonScan,
    ended: bool,
}

/// Incremental bracket and string tracking over a partial line, so a line arriving in
/// many pieces is scanned once rather than revalidated as each piece lands.
#[derive(Default)]
struct JsonScan {
    /// Bytes of the buffer already scanned.
    offset: usize,
    depth: usize,
    in_string: bool,
    escaped: bool,
    opened: bool,
    /// The last non-whitespace byte scanned.
    last: u8,
    /// A close without an open, or a balanced value that failed validation: no
    /// extension of the line can become one whole JSON value.
    broken: bool,
}

impl JsonScan {
    /// Scans the bytes added since the last call; true when the line may now hold one
    /// whole object or array.
    fn advance(&mut self, buffer: &[u8]) -> bool {
        for &b in &buffer[self.offset..] {
            if !b.is_ascii_whitespace() {
                self.last = b;
            }
            if self.in_string {
                match b {
                    _ if self.escaped => self.escaped = false,
                    b'\\' => self.escaped = true,
                    b'"' => self.in_string = false,
                    _ => {}
                }
                continue;
            }
            match b {
                b'"' => self.in_string = true,
                b'{' | b'[' => {
                    self.depth += 1;
                    self.opened = true;
                }
                b'}' | b']' => match self.depth.checked_sub(1) {
                    Some(depth) => self.depth = depth,
                    None => self.broken = true,
                },
                _ => {}
            }
        }
        self.offset = buffer.len();
        !self.broken && self.opened && self.depth == 0 && !self.in_string && matches!(self.last, b'}' | b']')
    }
}

impl StreamUsage {
    fn chunk(&mut self, payload: &[u8]) {
        if payload.is_empty() || self.ended {
            return;
        }
        self.sink.token_event(false);
        let start = self.buffer.len();
        self.buffer.extend_from_slice(payload);
        if let Some(end) = payload.iter().rposition(|b| *b == b'\n') {
            let rest = self.buffer.split_off(start + end + 1);
            let lines = std::mem::replace(&mut self.buffer, rest);
            self.lines(&lines);
            // The remainder is part of this payload, so rescanning it stays linear.
            self.scan = JsonScan::default();
        }
        // A complete JSON payload without its newline counts now, as Go's does. Go
        // runs `json.Valid` on the whole remainder after every chunk (quadratic in a
        // line split into many chunks); here each byte is scanned once, and the
        // remainder is validated only when its outer brackets close: a valid one is
        // consumed, an invalid one is never validated again.
        // ponytail: a newline-free scalar remainder such as `data: null` is not flushed
        // (Go flushes it, since `json.Valid("null")` holds), so the next chunk joins it
        // into one invalid line and a usage line arriving that way is lost. Flushing
        // would mean validating scalar prefixes; add it if a plugin emits scalars.
        if self.scan.advance(&self.buffer) {
            if extract_stream_json(&self.buffer).is_some_and(cpa_common::json::std_valid) {
                let rest = std::mem::take(&mut self.buffer);
                self.scan = JsonScan::default();
                self.lines(&rest);
            } else {
                self.scan.broken = true;
            }
        }
    }

    /// Go `IterateStreamLines`.
    fn lines(&self, payload: &[u8]) {
        for line in payload.split(|b| *b == b'\n') {
            let line = line.trim_ascii();
            if !line.is_empty() {
                self.sink.plugin_response_line(&self.protocol, line);
            }
        }
    }

    /// Go `publishResult`: the stream ended, cleanly or with `error`.
    fn end(&mut self, error: Option<&str>) {
        if std::mem::replace(&mut self.ended, true) {
            return;
        }
        let rest = std::mem::take(&mut self.buffer);
        self.lines(&rest);
        match error {
            // Go `HTTPStatusFromError` of a plugin chunk error carries no status.
            Some(error) => self.sink.publish_failure_with_usage(0, error),
            None => self.sink.publish(),
        }
    }
}

impl Drop for StreamUsage {
    fn drop(&mut self) {
        if !self.ended {
            self.ended = true;
            let rest = std::mem::take(&mut self.buffer);
            self.lines(&rest);
            // Go: the request context was canceled (`StatusClientClosedRequest`).
            self.sink.publish_failure_with_usage(499, "context canceled");
        }
    }
}

/// Go `ExtractStreamJSONPayload`.
fn extract_stream_json(line: &[u8]) -> Option<&[u8]> {
    let mut trimmed = line.trim_ascii();
    if trimmed.is_empty() || trimmed == b"[DONE]" || trimmed.starts_with(b"event:") {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix(b"data:") {
        trimmed = rest.trim_ascii();
    }
    (!trimmed.is_empty() && trimmed != b"[DONE]").then_some(trimmed)
}

struct StreamState {
    usage: Option<StreamUsage>,
    translator: Option<Box<dyn cpa_translate::stream::GoStream>>,
    /// A built-in stream translator exists for the pair (Go
    /// `executorNativeStreamResponseTranslatorExists`).
    native: bool,
    passthrough: bool,
    responses: bool,
    openai_tail: bool,
    chunks: BoxStream<'static, Chunk>,
    ready: VecDeque<Result<Bytes, ExecError>>,
    done: bool,
    /// A chunk error: request-scoped on the model router path; on the credential-bound
    /// path an attempt (Go hands the plugin's statusless chunk error to the auth
    /// manager's stream bootstrap, which classifies it like an execution error).
    chunk_error: fn(String) -> ExecError,
    _scope: Scope,
}

impl StreamState {
    /// Go `translateExecutorStreamPayload` for one plugin chunk.
    fn frames(&mut self, payload: &[u8]) -> Vec<Vec<u8>> {
        if self.passthrough {
            return vec![payload.to_vec()];
        }
        let frames = match &mut self.translator {
            Some(t) => t.line(payload).unwrap_or_default(),
            // Go's registry passes an unregistered pair's chunk through.
            None => vec![payload.to_vec()],
        };
        // Go `executorStreamTranslationFellBack`: an unchanged single frame from a
        // registered translator is the registry fallback, not a translated frame.
        if self.native && frames.len() == 1 && frames[0] == payload {
            return Vec::new();
        }
        if !self.responses {
            return frames;
        }
        frames
            .into_iter()
            .map(|f| cpa_exec::ensure_responses_usage_details(&f))
            .collect()
    }

    /// One chunk for the route's writer; Go's handlers drop empty chunks.
    fn push(&mut self, frame: Vec<u8>) {
        if !frame.is_empty() {
            self.ready.push_back(Ok(Bytes::from(frame)));
        }
    }
}

/// Go `Auth.Attributes` as a plugin executor gets it. `private` names what the Home
/// dispatcher set itself on a dispatched credential (Go keeps those outside
/// `Auth.Attributes`); attributes Home sent stay, whatever their names.
fn plugin_attributes(
    credential: &cpa_core::credential::Credential,
    private: &[String],
) -> std::collections::BTreeMap<String, String> {
    let mut attributes = crate::management::quota::go_attributes(credential);
    for key in private {
        attributes.remove(key);
    }
    attributes
}

#[cfg(test)]
mod tests {
    use cpa_core::credential::Credential;

    use super::*;

    /// A line arriving in pieces is reported whole only once its outer brackets close;
    /// brackets and escaped quotes inside strings do not count.
    #[test]
    fn a_partial_stream_line_is_whole_only_when_its_outer_value_closes() {
        let mut scan = JsonScan::default();
        let mut line = Vec::new();
        let pieces: [&[u8]; 4] = [br#"data: {"usage":[{"t":"}\"]"}"#, br#",{"n":1}"#, b"]", b"}  "];
        let whole: Vec<bool> = pieces
            .iter()
            .map(|piece| {
                line.extend_from_slice(piece);
                scan.advance(&line)
            })
            .collect();
        assert_eq!(whole, [false, false, false, true]);

        let mut stray = JsonScan::default();
        assert!(!stray.advance(b"data: }{}"), "a close before any open never balances");
    }

    /// A Home credential's dispatcher-set attributes stay out of the plugin request;
    /// one Home sent itself stays in, even under a name the dispatcher also uses.
    #[test]
    fn plugin_attributes_keep_home_sent_ones_and_drop_dispatcher_ones() {
        let mut c = Credential::relay_session("home-a");
        for (key, value) in [
            ("base_url", "http://127.0.0.1:9"),
            (crate::remote::SESSION, "plugin-owned"),
            (crate::remote::MODEL_INFO, r#"{"id":"gpt-up"}"#),
            (crate::remote::PROVIDER, "openai-compatible-probe"),
        ] {
            c.attributes.insert(key.into(), value.into());
        }
        let private = [crate::remote::MODEL_INFO.to_owned(), crate::remote::PROVIDER.to_owned()];
        let attributes = plugin_attributes(&c, &private);
        assert_eq!(
            attributes.keys().map(String::as_str).collect::<Vec<_>>(),
            ["base_url", crate::remote::SESSION, "runtime_only"]
        );
        assert_eq!(attributes[crate::remote::SESSION], "plugin-owned");
    }

    /// Every provider offered to a model router selects its credential when routed to,
    /// including Kimi files typed with their domain.
    #[test]
    fn routers_are_offered_selectable_providers() {
        let dir = cpa_plugin::testing::scratch(&std::env::temp_dir(), "plugin-available-providers");
        let cred = |name: &str, provider: &str| {
            let mut meta = serde_json::Map::new();
            meta.insert("type".into(), provider.into());
            Credential::from_file(&dir, &dir.join(name), meta).unwrap()
        };
        let creds = vec![
            cred("a.json", "kimi.com"),
            cred("b.json", "kimi.ai"),
            cred("c.json", "claude"),
        ];
        let cfg = cpa_core::config::Config::parse(&format!("auth-dir: {}\n", dir.display())).unwrap();
        let rt = std::sync::Arc::new(crate::testing::runtime(
            cfg,
            creds,
            cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        let providers = available_providers(&rt);
        assert_eq!(providers, ["claude", "kimi.ai", "kimi.com"]);
        for (provider, id) in providers.iter().zip(["c.json", "b.json", "a.json"]) {
            let lease = rt
                .store()
                .select(crate::runtime::Selection::new(provider, "some-model"))
                .unwrap_or_else(|| panic!("{provider} selects a credential"));
            assert_eq!(lease.credential.id, id);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
