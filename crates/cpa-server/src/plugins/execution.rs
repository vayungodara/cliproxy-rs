//! Plugin model routing and the plugin executors a router targets (Go
//! sdk/api/handlers/handlers_routing.go `applyModelRouter`, handlers_execution.go
//! `executeWithPluginExecutor` / `countWithPluginExecutor`, handlers_stream.go
//! `streamWithPluginExecutor`, and internal/pluginhost/adapters_executors.go's
//! `executorAdapter` called without an auth).
//!
//! ponytail: usage records for plugin executors are not published; plugin response translators (`hasResponseTranslator`) are counted
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
    fn new(trace: &Trace) -> Self {
        Self(RequestScope {
            request_id: trace.request_id(),
            capture: trace.capture(),
            ..Default::default()
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
    if !host.has_model_routers("") {
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
    let scope = Scope::new(trace);
    let resp = host
        .route_model(request, "", &available_providers(rt), &scope.0)
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
    let hooks = super::interceptors::start(rt, trace, &mut call, &model, false).await?;
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
        Ok(Done::Stream { headers, first, rest }) if call.response == Format::OpenAI && call.media.is_none() => {
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
        )
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
    let scope = Scope::new(trace);
    if !stream {
        let resp = if count {
            host.executor_count_tokens(&adapter, &request, &scope.0).await
        } else {
            host.executor_execute(&adapter, &request, &scope.0).await
        }
        .map_err(call_failure)?;
        return Ok(Done::Buffered {
            headers: header_map(&resp.headers),
            body: translate_response(&negotiated, &request, resp.payload),
        });
    }
    let opened = host
        .executor_execute_stream(&adapter, &request, &scope.0)
        .await
        .map_err(call_failure)?;
    let mut rest = translate_stream(negotiated, request, opened.chunks, scope);
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

/// Go's execution metadata (`pluginExecutorRequest`, `executeWithAuthManager`): the
/// request context plus the request's model facts; `model` is the resolved model.
pub(crate) fn execution_metadata(call: &Call, model: &str) -> Metadata {
    let mut meta = request_metadata(call);
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

/// Go `sdktranslator.TranslateRequest` between two executor format names.
/// ponytail: a format outside the built-in set passes the body through; Go would still
/// rewrite a differing top-level `model`.
fn translate_request(from: &str, to: &str, model: &str, body: &Bytes, stream: bool) -> Bytes {
    let (Some(from), Some(to)) = (Format::parse(from), Format::parse(to)) else {
        return body.clone();
    };
    let ctx = cpa_translate::RequestCtx { model, stream };
    cpa_translate::translate_request(from, to, &ctx, body)
        .map(Bytes::from)
        .unwrap_or_else(|_| body.clone())
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
fn translate_response(n: &Negotiated, req: &ExecutorRequest, payload: Bytes) -> Bytes {
    let responses = n.requested == Format::OpenAIResponse.as_str();
    if n.requested.is_empty() || n.output == n.requested {
        return ensure_usage(responses, payload);
    }
    let pair = Format::parse(&n.requested)
        .zip(Format::parse(&n.output))
        .and_then(|(to, from)| cpa_translate::pair(to, from));
    let Some(pair) = pair else {
        // Go's registry returns an unregistered pair's body unchanged.
        return ensure_usage(responses, payload);
    };
    let original = if req.original_request.is_empty() {
        &req.payload
    } else {
        &req.original_request
    };
    let ctx = cpa_translate::ResponseCtx {
        model: &req.model,
        original_request: original,
        translated_request: &req.payload,
    };
    let out = (pair.non_stream)(&ctx, &payload).map(Bytes::from).unwrap_or(payload);
    ensure_usage(responses, out)
}

/// Go `translateExecutorStreamChunks` and `mapExecutorStreamChunks`, plus the OpenAI
/// handler's `data: %s` framing of each chunk.
fn translate_stream(
    n: Negotiated,
    req: ExecutorRequest,
    chunks: BoxStream<'static, Chunk>,
    scope: Scope,
) -> ExecStream {
    let translating = !n.requested.is_empty() && n.output != n.requested;
    let formats = Format::parse(&n.requested).zip(Format::parse(&n.output));
    let translator = formats.filter(|_| translating).and_then(|(to, from)| {
        let original = if req.original_request.is_empty() {
            &req.payload
        } else {
            &req.original_request
        };
        let ctx = cpa_translate::ResponseCtx {
            model: &req.model,
            original_request: original,
            translated_request: &req.payload,
        };
        cpa_translate::go_stream(to, from).map(|open| open(&ctx))
    });
    let state = StreamState {
        native: translator.is_some(),
        translator,
        passthrough: n.requested.is_empty()
            || (n.output == n.requested && n.requested != Format::OpenAIResponse.as_str()),
        responses: n.requested == Format::OpenAIResponse.as_str(),
        openai_tail: translating && n.output == Format::OpenAI.as_str(),
        chunks,
        ready: VecDeque::new(),
        done: false,
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
                    if !chunk.payload.is_empty() {
                        for frame in s.frames(&chunk.payload) {
                            s.push(frame);
                        }
                    }
                    if let Some(error) = chunk.error {
                        s.ready
                            .push_back(Err(ExecError::local(0, FailureScope::Request, error)));
                        s.done = true;
                    }
                }
                None => {
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

struct StreamState {
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

#[cfg(test)]
mod tests {
    use cpa_core::credential::Credential;

    use super::*;

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
