//! The provider-neutral attempt loop every inference route runs (Go
//! `BaseAPIHandler.Execute*WithAuthManager` plus `Manager.Execute*`).
//!
//! Model to providers through the registry, credential selection across all of them,
//! per-credential alias pools, request-scoped rules, retry rounds and stream bootstrap.
//! Routes only parse their request and render the [`Done`] or [`Failure`].

use std::sync::Arc;
use std::time::Duration;

use axum::http::HeaderMap;
use bytes::{Bytes, BytesMut};
use cpa_core::config::Config;
use cpa_core::exec::{Caller, ExecError, ExecRequest, ExecStream, Operation, ResponseBody};
use cpa_core::format::Format;
use futures_util::StreamExt;

use crate::classify;
use crate::gojson;
use crate::registry::{self, AliasResult, Registry};
use crate::respond;
use crate::runtime::{AcquireError, Completing, Lease, Outcome, Runtime, Selection};
use crate::scheduler::{Policy, canonical_model};

/// One client request, already parsed by its route.
#[derive(Clone)]
pub struct Call {
    /// Format of the client request (Go entry protocol).
    pub entry: Format,
    /// Format the client expects back.
    pub response: Format,
    pub operation: Operation,
    /// The model exactly as the client named it (gjson `String()`, untrimmed).
    pub model: String,
    pub body: Bytes,
    pub stream: bool,
    pub alt: Option<String>,
    pub headers: HeaderMap,
    pub caller: Caller,
    /// Skip registry routing and use this provider (Interactions agents).
    pub forced_provider: Option<String>,
    /// Model used for credential selection when it differs from `model`.
    pub selection_model: Option<String>,
    /// Go `execution_session_id`: set by transports with long-lived sessions.
    pub execution_session: Option<String>,
    /// The matched route ([`route_path`]).
    pub request_path: String,
    /// The downstream peer (Go `Request.RemoteAddr`), when the listener provides it.
    pub peer: Option<std::net::SocketAddr>,
    /// A turn of a long-lived downstream session (the Responses WebSocket), `None` for
    /// ordinary HTTP requests.
    pub turn: Option<Arc<SessionTurn>>,
    /// A media route (Go's `openai-image` / `openai-video` handler types), `None` for
    /// inference routes.
    pub media: Option<Arc<Media>>,
}

/// How a media route runs through [`run`]: which executor operation serves it, and the
/// same credential binding a [`SessionTurn`] has (`WithPinnedAuthID`,
/// `WithSelectedAuthIDCallback`), which the video routes use to keep a job on the
/// credential that created it.
pub struct Media {
    pub kind: MediaKind,
    /// Only this credential may serve the call.
    pub pinned: Option<String>,
    /// Called with each credential right before it is attempted.
    pub on_selected: Option<OnSelected>,
    /// The client gets an event stream (Go's image streams, also when emulated from a
    /// buffered call): until the result renders, [`serve`] holds the connection with
    /// `: keep-alive` frames every `requests.streaming.keepalive-seconds`
    /// (`waitImagesStreamExecution`) instead of the non-stream keep-alive. A failure that
    /// renders after that commit is replaced by the response's [`SseError`] event.
    pub sse: bool,
    /// `WithDisallowFreeAuth` (routed images): Codex credentials on the free plan are
    /// never selected (Go `isFreeCodexAuth`), in any round.
    pub disallow_free: bool,
}

/// The `event: error` frame a route attaches to an error response (as a response
/// extension), for [`serve`] to write instead when the event stream already committed
/// (`writeImagesStreamErrorEvent`).
#[derive(Clone)]
pub struct SseError(pub Bytes);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    /// [`cpa_exec::Executors::images`]; image-only models are allowed
    /// (`ExecuteImageWithAuthManager`).
    Images,
    /// [`cpa_exec::Executors::videos`].
    Videos,
}

impl Call {
    /// `WithPinnedAuthID` of a session turn or media call.
    pub(crate) fn pinned(&self) -> Option<&str> {
        self.turn
            .as_ref()
            .and_then(|t| t.pinned.as_deref())
            .or_else(|| self.media.as_ref().and_then(|m| m.pinned.as_deref()))
    }

    /// `WithSelectedAuthIDCallback` of a session turn or media call.
    fn on_selected(&self) -> Option<&OnSelected> {
        self.turn
            .as_ref()
            .and_then(|t| t.on_selected.as_ref())
            .or_else(|| self.media.as_ref().and_then(|m| m.on_selected.as_ref()))
    }
}

/// How a session transport runs one turn through [`run`] (Go
/// `ExecuteStreamWithAuthManager` with `WithPinnedAuthID`,
/// `WithSelectedAuthIDCallback` and the execution session).
pub struct SessionTurn {
    /// Passed to [`cpa_exec::Executors::execute_in_session`] for every attempt.
    pub session: cpa_core::exec::ExecSession,
    /// Only this credential may serve the turn (`WithPinnedAuthID`); every retry round
    /// excludes all others.
    pub pinned: Option<String>,
    /// Called with each credential right before it is attempted
    /// (`WithSelectedAuthIDCallback`); the last call before success is the serving one.
    pub on_selected: Option<OnSelected>,
    /// The Home pick the connection's pooled upstream socket keeps between turns; none
    /// keeps every Home pick to its turn.
    pub home: Option<Arc<crate::home_session::SessionHome>>,
}

/// The [`SessionTurn::on_selected`] callback.
pub type OnSelected = Box<dyn Fn(&cpa_core::credential::Credential) + Send + Sync>;

/// The downstream peer address, as the listener records it (`ConnectInfo`).
pub type Peer = Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>;

pub fn peer(peer: Peer) -> Option<std::net::SocketAddr> {
    peer.map(|axum::Extension(axum::extract::ConnectInfo(addr))| addr)
}

/// Go `RequestPathMetadataKey` (gin `FullPath()`): the matched route template in gin's
/// spelling (`/v1beta/models/*action`), or the URI path when no route matched.
pub fn route_path(matched: Option<&axum::extract::MatchedPath>, uri: &axum::http::Uri) -> String {
    let Some(matched) = matched else {
        return uri.path().to_owned();
    };
    matched
        .as_str()
        .split('/')
        .map(
            |segment| match segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                Some(name) if name.starts_with('*') => name.to_owned(),
                Some(name) => format!(":{name}"),
                None => segment.to_owned(),
            },
        )
        .collect::<Vec<_>>()
        .join("/")
}

pub enum Done {
    Buffered {
        headers: HeaderMap,
        body: Bytes,
    },
    /// A stream whose first event already arrived. `rest` reports its lease outcome.
    Stream {
        headers: HeaderMap,
        first: Option<Bytes>,
        rest: ExecStream,
    },
}

#[derive(Debug, Clone)]
pub enum Failure {
    /// An executor or upstream error, rendered by the route's error shape.
    Exec(ExecError),
    /// A trusted control-plane rejection (Go `HomeConcurrencyBusyError`): rendered like
    /// `Exec`, with the `Retry-After` seconds it may expose.
    Remote { error: ExecError, retry_after: Option<u64> },
    /// No provider registered the model (Go `getRequestDetails`).
    UnknownModel(String),
    /// An image-only model on a non-image route.
    ImageOnly(String),
    /// `auth_not_found` / `auth_unavailable`, enriched like Go.
    Unavailable {
        code: &'static str,
        providers: Vec<String>,
        model: String,
        cause: Option<String>,
        retry_after: Option<Duration>,
    },
    /// Every candidate is cooling down for this model.
    Cooldown {
        model: String,
        provider: String,
        wait: Duration,
        cause: Option<String>,
    },
}

impl Failure {
    pub fn status(&self) -> u16 {
        match self {
            Failure::Exec(e) | Failure::Remote { error: e, .. } => classify::response_status(e),
            Failure::UnknownModel(_) => 400,
            Failure::ImageOnly(_) | Failure::Unavailable { .. } => 503,
            Failure::Cooldown { .. } => 429,
        }
    }

    /// Go `err.Error()`: what route error writers render.
    pub fn text(&self) -> String {
        match self {
            Failure::Exec(e) | Failure::Remote { error: e, .. } => classify::error_text(e),
            Failure::UnknownModel(model) => {
                let message = gojson::sjson_string(&format!("unknown provider for model {model}"));
                format!(
                    r#"{{"error":{{"message":{message},"type":"invalid_request_error","code":"model_not_found","param":"model"}}}}"#
                )
            }
            Failure::ImageOnly(model) => {
                let base = model.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(model);
                format!(
                    "model {} is only supported on /v1/images/generations and /v1/images/edits",
                    base.trim()
                )
            }
            Failure::Unavailable {
                code,
                providers,
                model,
                cause,
                ..
            } => {
                let providers = if providers.is_empty() {
                    "unknown".into()
                } else {
                    providers.join(",")
                };
                let model = if gojson::trim(model).is_empty() {
                    "unknown"
                } else {
                    gojson::trim(model)
                };
                let mut detail = match cause.as_deref().map(upstream_summary).filter(|s| !s.is_empty()) {
                    Some(summary) => format!(
                        "no auth available (providers={providers}, model={model}; last upstream error: {summary})"
                    ),
                    None => format!("no auth available (providers={providers}, model={model})"),
                };
                if format!(",{providers},").contains(",claude,") {
                    detail.push_str("; check Claude auth/key session and cooldown state via /v0/management/auth-files");
                }
                format!("{code}: {detail}")
            }
            Failure::Cooldown {
                model,
                provider,
                wait,
                cause,
            } => {
                let shown = if model.is_empty() { "requested model" } else { model };
                let mut message = format!("All credentials for model {shown} are cooling down");
                if !provider.is_empty() {
                    message.push_str(&format!(" via provider {provider}"));
                }
                let display = if !wait.is_zero() && *wait < Duration::from_secs(1) {
                    Duration::from_secs(1)
                } else {
                    Duration::from_secs((wait.as_secs_f64()).round() as u64)
                };
                let mut body = serde_json::Map::new();
                body.insert("code".into(), "model_cooldown".into());
                body.insert("model".into(), model.clone().into());
                body.insert("reset_time".into(), gojson::duration(display).into());
                body.insert("reset_seconds".into(), ceil_seconds(*wait).into());
                if !provider.is_empty() {
                    body.insert("provider".into(), provider.clone().into());
                }
                if let Some(summary) = cause.as_deref().map(upstream_summary).filter(|s| !s.is_empty()) {
                    message.push_str(&format!(" (last error: {summary})"));
                    body.insert("last_upstream_error".into(), summary.into());
                }
                body.insert("message".into(), message.into());
                gojson::sorted(&serde_json::json!({ "error": body }))
            }
        }
    }

    /// `Retry-After` from Go's `SafeResponseHeaders` (scheduler errors only).
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            Failure::Cooldown { wait, .. } => Some(ceil_seconds(*wait)),
            Failure::Remote { retry_after, .. } => *retry_after,
            Failure::Unavailable {
                retry_after: Some(wait),
                ..
            } if !wait.is_zero() => Some(ceil_seconds(*wait).max(1)),
            _ => None,
        }
    }

    /// An error the route returns untouched (claude_executor_fast_error.go).
    pub fn direct(&self) -> Option<&ExecError> {
        // Go finds the direct response through any wrapper, a Home round marker included.
        match self {
            Failure::Exec(e) | Failure::Remote { error: e, .. } if e.direct => Some(e),
            _ => None,
        }
    }
}

fn ceil_seconds(d: Duration) -> u64 {
    d.as_secs() + u64::from(d.subsec_nanos() > 0)
}

/// Go `ExtractUpstreamErrorSummary`: the upstream error's code and message, sanitized.
pub fn upstream_summary(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    let json_part = match raw.find(": {") {
        Some(i) if i < 50 => raw[i + 2..].trim(),
        _ => raw,
    };
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_part) {
        let s = |v: Option<&serde_json::Value>| gojson::trim(&gojson::gjson_string(v)).to_owned();
        let (mut code, mut message) = (String::new(), String::new());
        match v.get("error") {
            Some(e @ serde_json::Value::Object(_)) => {
                code = s(e.get("code"));
                if code.is_empty() {
                    code = s(e.get("type"));
                }
                message = s(e.get("message"));
            }
            Some(serde_json::Value::String(m)) => message = m.trim().to_owned(),
            _ => {}
        }
        if code.is_empty() && message.is_empty() {
            code = s(v.get("code"));
            if code.is_empty() {
                code = s(v.get("type"));
            }
            message = s(v.get("message"));
        }
        let summary = match (code.is_empty(), message.is_empty()) {
            (false, false)
                if code.eq_ignore_ascii_case(&message) || message.to_lowercase().contains(&code.to_lowercase()) =>
            {
                message
            }
            (false, false) => format!("{code}: {message}"),
            (true, false) => message,
            (false, true) => code,
            (true, true) => String::new(),
        };
        if !summary.is_empty() {
            return crate::sanitize::summary(&summary);
        }
    }
    crate::sanitize::summary(raw)
}

const IMAGE_ONLY: [&str; 8] = [
    "gpt-image-1.5",
    "gpt-image-2",
    "gpt-image-2.5-flare",
    "gpt-image-2.5-sunburst",
    "gpt-image-2.5",
    "grok-imagine-image",
    "grok-imagine-image-quality",
    "grok-imagine-image-2.0",
];

/// Go `getRequestDetails`: resolve `auto`, find the providers, keep the suffix.
fn route(
    rt: &Runtime,
    registry: &Registry,
    call: &Call,
    routed: Option<&(String, Option<String>)>,
) -> Result<(Vec<String>, String), RunError> {
    if let Some(provider) = &call.forced_provider {
        return Ok((vec![provider.clone()], gojson::trim(&call.model).to_owned()));
    }
    // Go `providersForExecution` with a router's provider: that provider, the router's
    // model or else the client's, and only the image-only check.
    if let Some((provider, model)) = routed {
        let model = model.clone().unwrap_or_else(|| call.model.clone());
        let base = gojson::trim(canonical_model_raw(&model));
        let image = base
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(base)
            .trim()
            .to_lowercase();
        let images_route = call.media.as_ref().is_some_and(|m| m.kind == MediaKind::Images);
        if IMAGE_ONLY.contains(&image.as_str()) && !images_route {
            return Err(Failure::ImageOnly(base.to_owned()).into());
        }
        let mut providers = vec![provider.clone()];
        adjust_for_entry(call.entry, &mut providers);
        return Ok((providers, model));
    }
    let model = call.model.as_str();
    let base = match model.rfind('(') {
        Some(i) if model.ends_with(')') => &model[..i],
        _ => model,
    };
    // Go Home mode: no `auto` resolution; Home routes the model as sent.
    let remote = rt.remote_dispatch().is_some();
    let resolved = if base == "auto" && !remote {
        let first = registry
            .resolve_auto(|client, model| rt.suspension(client, model))
            .unwrap_or_else(|| "auto".into());
        format!("{first}{}", &model[base.len()..])
    } else {
        model.to_owned()
    };
    let base = gojson::trim(canonical_model_raw(&resolved)).to_owned();
    let image = base
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(&base)
        .trim()
        .to_lowercase();
    let images_route = call.media.as_ref().is_some_and(|m| m.kind == MediaKind::Images);
    if IMAGE_ONLY.contains(&image.as_str()) && !images_route {
        return Err(Failure::ImageOnly(base).into());
    }
    if remote {
        return Ok((vec!["home".to_owned()], resolved));
    }
    let mut providers = registry.providers(&base);
    if providers.is_empty() && base != resolved {
        providers = registry.providers(&resolved);
    }
    if providers.is_empty() {
        return Err(Failure::UnknownModel(call.model.clone()).into());
    }
    adjust_for_entry(call.entry, &mut providers);
    Ok((providers, resolved))
}

/// Go `adjustExecutionProvidersForEntryProtocol`.
fn adjust_for_entry(entry: Format, providers: &mut Vec<String>) {
    match entry {
        Format::Interactions => {
            if let Some(i) = providers.iter().position(|p| p == "gemini-interactions") {
                let p = providers.remove(i);
                providers.insert(0, p);
            }
        }
        Format::OpenAI | Format::OpenAIResponse | Format::Claude | Format::Gemini => {}
        _ => providers.retain(|p| p != "gemini-interactions"),
    }
}

/// `thinking.ParseSuffix(model).ModelName` without trimming.
fn canonical_model_raw(model: &str) -> &str {
    match model.rfind('(') {
        Some(i) if model.ends_with(')') => &model[..i],
        _ => model,
    }
}

/// Runs `call` and renders the result, adding Go's `X-CPA-TRACE-ID` (selection time,
/// the selected credential's auth index and a request ID) once a credential was picked.
///
/// Non-stream generate calls run under Go's `StartNonStreamingKeepAlive`: when
/// `requests.nonstream-keepalive-interval` is set and no result arrived within one
/// interval, the response commits as 200 `application/json` and a `\n` goes out every
/// interval until the call finishes; the rendered body follows. The rendered status and
/// headers are lost then, as in Go, where they are written after the first keep-alive
/// flush. Media calls marked [`Media::sse`] hold with event-stream keep-alives instead.
pub async fn serve<F, Fut>(rt: &Arc<Runtime>, call: Call, render: F) -> axum::response::Response
where
    F: FnOnce(Result<Done, Failure>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = axum::response::Response> + Send + 'static,
{
    let sse = call.media.as_ref().is_some_and(|m| m.sse);
    let interval = if sse {
        crate::respond::keepalive(&rt.config())
    } else {
        nonstream_keepalive(&call, &rt.config())
    };
    let trace = Arc::new(Trace::with_request_id(crate::observability::current_request_id()));
    // A keep-alive moves `run` into the response body, past the request's gate.
    let mut run = Box::pin(crate::remote::keep_query_credential({
        let (rt, trace) = (rt.clone(), trace.clone());
        async move {
            let result = run_with_bootstrap_retries(&rt, call, &trace).await;
            let upstream = upstream_headers(&rt.config(), &result);
            (result, upstream)
        }
    }));
    // Upstream headers and the trace ID go on a rendered response; once a keep-alive
    // committed the headers they are lost, as in Go.
    let traced = {
        let trace = trace.clone();
        move |mut response: axum::response::Response, upstream: Option<Upstream>| {
            // The same config snapshot decides passthrough for both header sources.
            let passthrough = matches!(upstream, Some(Upstream::Success(_)));
            match upstream {
                Some(Upstream::Success(headers)) => respond::write_upstream_headers(response.headers_mut(), &headers),
                Some(Upstream::Error(headers)) => respond::write_error_headers(response.headers_mut(), &headers),
                None => {}
            }
            // Go `downstreamHeadersAfterInterceptors`: without passthrough, the headers
            // plugin interceptors changed; under passthrough the final ones went above.
            let intercepted = trace.intercepted_headers();
            if !passthrough && !intercepted.is_empty() {
                respond::write_upstream_headers(
                    response.headers_mut(),
                    &respond::filter_upstream_headers(&intercepted),
                );
            }
            if let Some(value) = trace.header() {
                response.headers_mut().insert("x-cpa-trace-id", value);
            }
            response
        }
    };
    let Some(interval) = interval else {
        let (result, upstream) = run.await;
        return traced(render(result).await, upstream);
    };
    tokio::select! {
        biased;
        (result, upstream) = &mut run => return traced(render(result).await, upstream),
        () = tokio::time::sleep(interval) => {}
    }
    // Go's handlers stop the keep-alive as soon as Execute returns: no beats while the
    // result renders (the video download, for one).
    let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    enum Event {
        Tick,
        Done(Result<Done, Failure>),
    }
    let events = futures_util::stream::unfold(Some((run, ticks)), |state| async move {
        let (mut run, mut ticks) = state?;
        tokio::select! {
            biased;
            (result, _) = &mut run => Some((Event::Done(result), None)),
            _ = ticks.tick() => Some((Event::Tick, Some((run, ticks)))),
        }
    });
    let beat = Bytes::from_static(if sse { b": keep-alive\n\n" } else { b"\n" });
    let once = |bytes: Bytes| futures_util::stream::iter([Ok::<_, axum::Error>(bytes)]);
    let first = once(beat.clone());
    let mut render = Some(render);
    let body = first.chain(events.flat_map(move |event| match event {
        Event::Tick => once(beat.clone()).left_stream(),
        Event::Done(result) => {
            let render = render.take().expect("the run finishes once");
            let rendered = futures_util::stream::once(render(result));
            let body = rendered.flat_map(move |response| match response.extensions().get::<SseError>() {
                Some(SseError(event)) if sse => once(event.clone()).left_stream(),
                _ => response.into_body().into_data_stream().right_stream(),
            });
            body.right_stream()
        }
    }));
    let body = axum::body::Body::from_stream(body);
    let mut response = if sse {
        crate::respond::sse(body)
    } else {
        let mut response = axum::response::Response::new(body);
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        );
        response
    };
    let headers = response.headers_mut();
    // Go cpa_trace.go applies the trace ID when the first keep-alive commits headers.
    if let Some(value) = trace.header() {
        headers.insert("x-cpa-trace-id", value);
    }
    response
}

/// Upstream response headers bound for the client under `requests.passthrough-headers`.
enum Upstream {
    /// Go `downstreamHeadersFromExecutor`, written by `WriteUpstreamHeaders`.
    Success(HeaderMap),
    /// Go `ErrorMessage.Addon`, written by `WriteErrorResponse`.
    Error(HeaderMap),
}

fn upstream_headers(cfg: &Config, result: &Result<Done, Failure>) -> Option<Upstream> {
    if !respond::passthrough_headers(cfg) {
        return None;
    }
    match result {
        Ok(Done::Buffered { headers, .. } | Done::Stream { headers, .. }) => {
            Some(Upstream::Success(respond::filter_upstream_headers(headers)))
        }
        Err(Failure::Exec(error)) if !error.direct => Some(Upstream::Error((*error.headers).clone())),
        Err(_) => None,
    }
}

/// `requests.nonstream-keepalive-interval` seconds (Go `NonStreamingKeepAliveInterval`;
/// 0 or below disables it), for the calls whose Go handler starts the keep-alive: only
/// non-stream generate handlers do; the count-tokens handlers (code_handlers.go,
/// gemini_handlers.go) never call `StartNonStreamingKeepAlive`.
fn nonstream_keepalive(call: &Call, cfg: &Config) -> Option<Duration> {
    if call.stream || call.operation != Operation::Generate {
        return None;
    }
    let seconds = cfg
        .document
        .get("requests")
        .and_then(|r| r.get("nonstream-keepalive-interval"))
        .and_then(serde_yaml_ng::Value::as_i64)
        .unwrap_or(0);
    (seconds > 0).then(|| Duration::from_secs(seconds as u64))
}

/// `requests.streaming.bootstrap-retries` (Go `StreamingBootstrapRetries`).
fn bootstrap_retries(cfg: &Config) -> usize {
    cfg.document
        .get("requests")
        .and_then(|r| r.get("streaming"))
        .and_then(|s| s.get("bootstrap-retries"))
        .and_then(serde_yaml_ng::Value::as_i64)
        .unwrap_or(0)
        .max(0) as usize
}

/// Go handlers_stream.go: a stream that failed before its first payload is retried as a
/// whole request when the status is statusless, auth, quota, timeout or 5xx.
pub async fn run_with_bootstrap_retries(rt: &Arc<Runtime>, call: Call, trace: &Trace) -> Result<Done, Failure> {
    // Go `applyModelRouter` runs once per request, before provider resolution: a plugin
    // executor takes the request whole; a provider decision replaces the registry route.
    use crate::plugins::execution::{self as plugin_execution, Route};
    let query = plugin_execution::query();
    let forced = call.forced_provider.as_deref().map(|p| p.trim().to_lowercase());
    let routed = match (plugin_execution::route(rt, &call, &query, trace).await, forced) {
        (None, _) => None,
        // Go `validateNativeInteractionsExecution` / `providersForExecution`: a forced
        // provider admits only a router decision for that same provider, and keeps its
        // own route.
        (Some(Route::Provider { provider, .. }), Some(forced)) if provider == forced => None,
        (Some(_), Some(_)) => {
            return Err(Failure::Exec(ExecError::local(
                400,
                cpa_core::exec::FailureScope::Request,
                "agent is only supported for native interactions execution",
            )));
        }
        (Some(Route::Executor(plugin)), None) => {
            return plugin_execution::execute(rt, &call, &plugin, &query, trace).await;
        }
        (Some(Route::Provider { provider, model }), None) => Some((provider, model)),
    };
    // Go resolves providers before the lifecycle starts: an unknown model never starts one.
    let resolved = route(rt, &rt.registry(), &call, routed.as_ref()).map_err(|e| e.failure)?;
    let mut call = call;
    let hooks = crate::plugins::interceptors::start(rt, trace, &mut call, &resolved.1, true).await?;
    let result = retry_bootstrap(rt, call, trace, &resolved, hooks.as_ref()).await;
    match hooks {
        Some(hooks) => hooks.finish(result, trace).await,
        None => result,
    }
}

/// The bootstrap retry rounds of one resolved request.
async fn retry_bootstrap(
    rt: &Arc<Runtime>,
    call: Call,
    trace: &Trace,
    resolved: &(Vec<String>, String),
    hooks: Option<&Arc<crate::plugins::interceptors::Hooks>>,
) -> Result<Done, Failure> {
    // Go `maxBootstrapRetries = 0` while Home is enabled: Home's own rounds decide.
    let max = if call.stream && rt.remote_dispatch().is_none() {
        bootstrap_retries(&rt.config())
    } else {
        0
    };
    let mut result = run_primed(rt, call.clone(), trace, resolved.clone(), hooks).await;
    for _ in 0..max {
        let original = match &result {
            Err(RunError {
                failure: Failure::Exec(e),
                bootstrap: true,
            }) if bootstrap_eligible(classify::go_status(e)) => e.clone(),
            _ => break,
        };
        result = match run_primed(rt, call.clone(), trace, resolved.clone(), hooks).await {
            Err(RunError {
                failure,
                bootstrap: false,
            }) if matches!(failure, Failure::Unavailable { .. }) && classify::response_status(&original) >= 500 => {
                Err(RunError {
                    failure: Failure::Exec(original),
                    bootstrap: false,
                })
            }
            Err(RunError {
                failure,
                bootstrap: false,
            }) => {
                return Err(failure);
            }
            other => other,
        };
    }
    result.map_err(|e| e.failure)
}

/// [`run_routed`], with a stream read through the chunk interceptors up to its first
/// delivered chunk: an error before it is a bootstrap failure (Go
/// `readInitialStreamChunks` inside the bootstrap loop).
async fn run_primed(
    rt: &Arc<Runtime>,
    call: Call,
    trace: &Trace,
    resolved: (Vec<String>, String),
    hooks: Option<&Arc<crate::plugins::interceptors::Hooks>>,
) -> Result<Done, RunError> {
    let result = run_routed(rt, call, trace, resolved, hooks.map(|h| &**h)).await;
    match (hooks, result) {
        (Some(hooks), Ok(done @ Done::Stream { .. })) => {
            hooks.prime(Ok(done), trace).await.map_err(|failure| RunError {
                failure,
                bootstrap: true,
            })
        }
        (_, result) => result,
    }
}

fn bootstrap_eligible(status: u16) -> bool {
    matches!(status, 0 | 401 | 402 | 403 | 408 | 429) || status >= 500
}

/// The trace ID of the last credential selected for a request.
#[derive(Default)]
pub struct Trace(
    std::sync::Mutex<Option<String>>,
    std::sync::OnceLock<String>,
    cpa_core::exec::CaptureSink,
    /// Response headers plugin interceptors changed (Go `downstreamHeadersAfterInterceptors`).
    std::sync::Mutex<HeaderMap>,
);

impl Trace {
    /// Uses the HTTP middleware identity; absent identity keeps internal execution's
    /// lazy UUID generation. Capture this before spawning background work.
    pub fn with_request_id(id: Option<String>) -> Self {
        let trace = Self(
            Default::default(),
            Default::default(),
            crate::request_logging::current()
                .map(|log| log.capture_sink())
                .unwrap_or_default(),
            Default::default(),
        );
        if let Some(id) = id.filter(|id| !id.is_empty()) {
            let _ = trace.1.set(id);
        }
        trace
    }

    /// The request ID (Go `logging.GetRequestID`), created on first use.
    pub fn request_id(&self) -> String {
        self.1.get_or_init(request_id).clone()
    }

    /// Connection-owned transports pass their capture explicitly after leaving
    /// middleware task scope, along with the same middleware request ID.
    pub fn with_capture(mut self, capture: cpa_core::exec::CaptureSink) -> Self {
        self.2 = capture;
        self
    }

    /// The request's upstream capture.
    pub(crate) fn capture(&self) -> cpa_core::exec::CaptureSink {
        self.2.clone()
    }

    /// Records the response headers plugin interceptors changed; [`serve`] writes them
    /// (filtered like upstream headers) over the rendered response.
    pub(crate) fn set_intercepted_headers(&self, headers: HeaderMap) {
        *self.3.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = headers;
    }

    fn intercepted_headers(&self) -> HeaderMap {
        std::mem::take(&mut *self.3.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// The `X-CPA-TRACE-ID` value of the credential selected so far.
    fn header(&self) -> Option<axum::http::HeaderValue> {
        let id = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()?;
        axum::http::HeaderValue::from_str(&id).ok()
    }

    pub(crate) fn selected(&self, credential: &cpa_core::credential::Credential) {
        let index = cpa_core::config::credentials::auth_index(credential);
        if index.is_empty() {
            return;
        }
        let request = self.1.get_or_init(request_id);
        let stamp = chrono::Local::now().format("%Y%m%d%H%M%S");
        *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(format!("{stamp}-{index}-{request}"));
    }

    /// The trace ID once a credential was selected.
    pub(crate) fn id(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
}

/// A UUIDv7 request ID (Go `logging.GenerateRequestID`).
pub fn request_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let random = || {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        );
        h.finish()
    };
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let (a, b) = (random(), random());
    let hi = (millis << 16) | 0x7000 | (a & 0x0fff);
    let lo = (b & 0x3fff_ffff_ffff_ffff) | 0x8000_0000_0000_0000;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        hi >> 32,
        (hi >> 16) & 0xffff,
        hi & 0xffff,
        lo >> 48,
        lo & 0xffff_ffff_ffff
    )
}

/// A terminal failure of [`run`]. `bootstrap` marks a stream that failed before its
/// first payload (Go `streamBootstrapError`).
pub struct RunError {
    pub failure: Failure,
    pub bootstrap: bool,
}

impl From<Failure> for RunError {
    fn from(failure: Failure) -> Self {
        Self {
            failure,
            bootstrap: false,
        }
    }
}

/// One attempt's failure.
#[derive(Clone)]
struct Fault {
    error: ExecError,
    bootstrap: bool,
}

impl Fault {
    fn into_run_error(self) -> RunError {
        RunError {
            failure: Failure::Exec(self.error),
            bootstrap: self.bootstrap,
        }
    }
}

/// Runs one request through selection, execution and retry rounds (Go
/// `Manager.Execute*`). Selection failures take part in retry rounds too.
pub async fn run(rt: &Arc<Runtime>, call: Call, trace: &Trace) -> Result<Done, RunError> {
    let resolved = route(rt, &rt.registry(), &call, None)?;
    run_routed(rt, call, trace, resolved, None).await
}

/// [`run`] for resolved providers and model, with the request's plugin hooks.
async fn run_routed(
    rt: &Arc<Runtime>,
    call: Call,
    trace: &Trace,
    (providers, model): (Vec<String>, String),
    hooks: Option<&crate::plugins::interceptors::Hooks>,
) -> Result<Done, RunError> {
    let (cfg, policy) = rt.request_snapshot();
    let registry = rt.registry();
    let aliases = registry::global_aliases(&cfg);
    let session = crate::session::resolve(
        call.entry,
        &call.headers,
        &call.body,
        call.execution_session.as_deref(),
        &call.caller.principal,
    );
    // Go's usage reporter: one record per Generate attempt while the queue accepts.
    // Home count-tokens attempts record only their upstream 401s (`reportHomeUnauthorized`).
    let home_count = call.operation == Operation::CountTokens && rt.remote_dispatch().is_some();
    let facts = ((call.operation == Operation::Generate
        && (rt.usage_queue().accepts() || rt.plugins().has_usage_plugins()))
        || (home_count && rt.usage_queue().accepts()))
    .then(|| {
        Arc::new(crate::usage_record::Facts::new(
            usage_client(&cfg, &call, trace, &session),
            call.entry,
            call.response,
            &call.model,
            &call.body,
            call.stream,
        ))
    });
    let (usage, unauthorized) = if home_count { (None, facts) } else { (facts, None) };
    // `WithPinnedAuthID`: every other credential is excluded in every round.
    // A remote dispatcher receives the pinned ID itself.
    let mut pinned_exclusion: Vec<String> = match call.pinned() {
        Some(pinned) if !pinned.is_empty() && rt.remote_dispatch().is_none() => rt
            .store()
            .snapshot()
            .iter()
            .filter(|c| c.id != pinned)
            .map(|c| c.id.clone())
            .collect(),
        _ => Vec::new(),
    };
    // `WithDisallowFreeAuth`: free-plan Codex credentials are ineligible in every round.
    // ponytail: Home mode selects remotely and does not see this flag.
    if call.media.as_ref().is_some_and(|m| m.disallow_free) {
        for credential in rt.store().snapshot() {
            let free = credential.provider.trim().eq_ignore_ascii_case("codex")
                && credential
                    .attributes
                    .get("plan_type")
                    .is_some_and(|plan| plan.trim().eq_ignore_ascii_case("free"));
            if free && !pinned_exclusion.contains(&credential.id) {
                pinned_exclusion.push(credential.id.clone());
            }
        }
    }
    let mut selection = Selection {
        providers: providers.clone(),
        model: call.selection_model.clone().unwrap_or_else(|| model.clone()),
        session: session.id.clone(),
        session_parent: session.parent,
        session_fork: session.fork,
        exclude: pinned_exclusion.clone(),
        ..Selection::default()
    };
    let request = ExecRequest {
        operation: call.operation,
        source_format: call.entry,
        response_format: call.response,
        requested_model: model.clone(),
        model: model.clone(),
        original_body: call.body.clone(),
        body: call.body.clone(),
        stream: call.stream,
        alt: call.alt.clone(),
        session: session.id,
        execution_session: call.execution_session.clone(),
        derived_session: session.derived,
        resolved_model: None,
        usage: cpa_core::exec::UsageSink::default().with_capture(trace.2.clone()),
        request_path: call.request_path.clone(),
        headers: call.headers.clone(),
        caller: call.caller.clone(),
    };
    let compact = call.alt.as_deref() == Some("responses/compact");
    if let Some(remote) = rt.remote_dispatch() {
        let context = RemoteContext {
            rt,
            cfg: &cfg,
            policy: &policy,
            call: &call,
            request: &request,
            usage: usage.as_ref(),
            unauthorized: unauthorized.as_ref(),
            aliases: &aliases,
            compact,
            trace,
            user_key: Default::default(),
        };
        return run_remote(context, remote, selection).await;
    }
    // Go `pickLCP`: with session affinity, an authenticated request without an explicit
    // session binds by its conversation prefix.
    if policy.session_affinity && !session.explicit {
        selection.lcp = crate::lcp::Request::new(call.entry.as_str(), &call.body, &call.caller.principal).map(Arc::new);
    }
    // Go `preferredExecutionAttemptError`: the latest failure that reached upstream wins
    // over later selection failures.
    let mut upstream: Option<Fault> = None;
    loop {
        let mut last: Option<Fault> = None;
        let mut attempted: Vec<String> = Vec::new();
        let pick_failure = loop {
            if policy.max_retry_credentials > 0 && attempted.len() >= policy.max_retry_credentials {
                break None;
            }
            let lease = match rt.acquire(selection.clone(), &cfg, policy.clone(), &registry).await {
                Ok(lease) => lease,
                Err(AcquireError::Prepare { id, error }) => {
                    attempted.push(id.clone());
                    selection.exclude.push(id);
                    last = Some(Fault {
                        error,
                        bootstrap: false,
                    });
                    continue;
                }
                Err(AcquireError::Cooldown { wait, cause }) => {
                    let provider = if providers.len() == 1 {
                        providers[0].clone()
                    } else {
                        String::new()
                    };
                    break Some(Failure::Cooldown {
                        model: selection.model.clone(),
                        provider,
                        wait,
                        cause,
                    });
                }
                Err(AcquireError::Unavailable { retry_after, cause }) => {
                    break Some(Failure::Unavailable {
                        code: if retry_after.is_some() {
                            "auth_unavailable"
                        } else {
                            "auth_not_found"
                        },
                        providers: providers.clone(),
                        model: model.clone(),
                        cause,
                        retry_after,
                    });
                }
            };
            selection.exclude.push(lease.credential.id.clone());
            trace.selected(&lease.credential);
            let (mut models, alias) = registry::execution_models(&aliases, &lease.credential, &selection.model);
            let pooled = models.len() > 1;
            if pooled {
                // Go `nextModelPoolOffset`: rotate the alias pool once per selection.
                let key = format!(
                    "{}|{}|{}",
                    lease.credential.id.trim().to_lowercase(),
                    registry::provider_key(&lease.credential),
                    canonical_model(registry::strip_prefix(&selection.model, &lease.credential)).to_lowercase()
                );
                let offset = rt.next_pool_offset(&key, models.len());
                models.rotate_left(offset);
            }
            let selection_model = registry::selection_model(&aliases, &lease.credential, &selection.model);
            let models: Vec<String> = models
                .into_iter()
                .filter(|m| {
                    let state = registry::state_model(&selection_model, &selection.model, m, pooled);
                    !rt.store().blocked(&lease.credential, &state)
                })
                .collect();
            if models.is_empty() {
                continue;
            }
            attempted.push(lease.credential.id.clone());
            let target = Target {
                models: &models,
                selection_model: &selection_model,
                pooled,
                alias: &alias,
                compact,
                keep_model: call.selection_model.is_some(),
                unauthorized: None,
            };
            match attempt(rt, &cfg, &policy, &call, &request, usage.as_ref(), lease, target, hooks).await {
                Attempt::Done(done) => return Ok(done),
                Attempt::Stop(fault) => return Err(fault.into_run_error()),
                Attempt::Next(fault) => {
                    if classify::upstream_attempted(&fault.error) {
                        upstream = Some(fault.clone());
                    }
                    last = Some(fault);
                }
            }
        };
        // The round's error decides retries: an attempt error when there was one, else
        // the selection failure. The response reports the latest upstream attempt when
        // there was one (Go executeMixedOnce, preferredExecutionAttemptError).
        let round_error: RunError = match (last, pick_failure) {
            (Some(fault), _) => fault.into_run_error(),
            (None, Some(failure)) => failure.into(),
            (None, None) => Failure::Unavailable {
                code: "auth_not_found",
                providers: providers.clone(),
                model: model.clone(),
                cause: None,
                retry_after: None,
            }
            .into(),
        };
        // Go `shouldRetryAfterErrorWithAttempted` / `isRequestRetryRoundError`.
        let (status, retry_round) = match &round_error.failure {
            Failure::Exec(e) => (
                classify::go_status(e),
                classify::is_retry_round(e) && !classify::is_request_invalid(e),
            ),
            Failure::Cooldown { .. } => (429, true),
            Failure::Unavailable {
                retry_after: Some(_), ..
            } => (503, true),
            _ => (0, false),
        };
        let terminal = |round_error: RunError| upstream.clone().map(Fault::into_run_error).unwrap_or(round_error);
        if !retry_round {
            return Err(terminal(round_error));
        }
        let wait = {
            let admit = crate::runtime::admission(&registry, &aliases, &selection, &rt.executors);
            rt.store().retry_wait(&selection, &policy, status, &attempted, &admit)
        };
        let Some(wait) = wait else {
            return Err(terminal(round_error));
        };
        if !wait.is_zero() {
            tokio::time::sleep(jitter(wait, policy.max_retry_interval)).await;
        }
        selection.retry_round += 1;
        selection.exclude.clone_from(&pinned_exclusion);
    }
}

/// What a remote round needs from [`run`].
struct RemoteContext<'a> {
    rt: &'a Arc<Runtime>,
    cfg: &'a Config,
    policy: &'a Arc<crate::scheduler::Policy>,
    call: &'a Call,
    request: &'a ExecRequest,
    usage: Option<&'a Arc<crate::usage_record::Facts>>,
    /// Home count-tokens requests: the facts their upstream 401s are recorded with.
    unauthorized: Option<&'a Arc<crate::usage_record::Facts>>,
    aliases: &'a std::collections::HashMap<String, Vec<cpa_core::registry::dynamic::OAuthAlias>>,
    compact: bool,
    trace: &'a Trace,
    /// The client key Home authenticated on the latest pick that sent one (Go
    /// `setHomeUserAPIKeyOnGinContext`, which keeps it for the rest of the request).
    user_key: std::sync::Mutex<String>,
}

/// Go `homeRetryRoundExhaustedError`: the round ended; its timing decides the next.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Exhausted {
    retry_after: Option<Duration>,
    retry_now: bool,
}

/// One failure in Home mode with what Go's Home retry decisions read from it.
#[derive(Debug, Clone)]
struct RemoteFault {
    failure: Failure,
    bootstrap: bool,
    /// The failure reached upstream (Go `hasUpstreamExecutionAttempt`).
    upstream: bool,
    /// A matched request-scoped stop: ends the request at once.
    stop: bool,
    /// Go `isRequestInvalidError`: never starts another round.
    invalid: bool,
    kind: crate::remote::RemoteErrorKind,
    exhausted: Option<Exhausted>,
}

impl RemoteFault {
    fn from_pick(error: crate::remote::RemoteError) -> Self {
        Self {
            failure: Failure::Exec(error.error),
            bootstrap: false,
            upstream: false,
            stop: false,
            invalid: false,
            kind: error.kind,
            exhausted: None,
        }
    }

    /// `stop` is [`attempt`]'s verdict (a matched stop rule, or an unmatched
    /// request-invalid error); a matched `continue` comes back as `Next`.
    fn from_attempt(fault: Fault, stop: bool) -> Self {
        Self {
            upstream: classify::upstream_attempted(&fault.error),
            invalid: classify::is_request_invalid(&fault.error),
            failure: Failure::Exec(fault.error),
            bootstrap: fault.bootstrap,
            stop,
            kind: crate::remote::RemoteErrorKind::Plain,
            exhausted: None,
        }
    }

    fn local(failure: Failure) -> Self {
        Self {
            failure,
            bootstrap: false,
            upstream: false,
            stop: false,
            invalid: false,
            kind: crate::remote::RemoteErrorKind::Plain,
            exhausted: None,
        }
    }

    /// Go `SafeResponseHeaders`: busy, then the round marker, then a Home cooldown.
    fn safe_retry_after(&self) -> Option<u64> {
        let seconds = |d: Duration| (!d.is_zero()).then(|| ceil_seconds(d).max(1));
        match (&self.kind, self.exhausted) {
            (crate::remote::RemoteErrorKind::Busy { header }, _) => *header,
            (_, Some(exhausted)) => exhausted.retry_after.and_then(seconds),
            (crate::remote::RemoteErrorKind::Cooldown { retry_after, .. }, None) => retry_after.and_then(seconds),
            _ => None,
        }
    }

    /// Go `markHomeRetryRoundExhausted`.
    fn exhausted(mut self, retry_after: Option<Duration>, retry_now: bool) -> Self {
        self.exhausted = Some(Exhausted { retry_after, retry_now });
        self
    }

    /// Go `retryAfterFromError`: the round marker's timing, else the error's own.
    fn retry_after(&self) -> Option<Duration> {
        if let Some(exhausted) = self.exhausted {
            return exhausted.retry_after;
        }
        match (&self.kind, &self.failure) {
            (crate::remote::RemoteErrorKind::Cooldown { retry_after, .. }, _) => *retry_after,
            (_, Failure::Exec(e)) => e.retry_after,
            _ => None,
        }
    }

    /// Go's status for retry decisions: transport faults have none.
    fn status(&self) -> u16 {
        match &self.failure {
            Failure::Exec(e) | Failure::Remote { error: e, .. } => classify::go_status(e),
            other => other.status(),
        }
    }

    /// Go `isRequestRetryRoundError`.
    fn retry_round(&self) -> bool {
        match &self.failure {
            Failure::Exec(e) | Failure::Remote { error: e, .. } => classify::is_retry_round(e),
            other => matches!(other.status(), 403 | 408 | 429 | 500 | 502 | 503 | 504),
        }
    }

    fn into_run_error(self) -> RunError {
        let retry_after = self.safe_retry_after();
        let failure = match (retry_after, self.failure) {
            (Some(_), Failure::Exec(error)) => Failure::Remote { error, retry_after },
            (_, failure) => failure,
        };
        RunError {
            failure,
            bootstrap: self.bootstrap,
        }
    }
}

/// Go `preferredExecutionAttemptError`: the latest upstream failure, under the current
/// round marker when the fallback ended a round.
fn preferred(fallback: RemoteFault, upstream: Option<&RemoteFault>) -> RemoteFault {
    let Some(upstream) = upstream else {
        return fallback;
    };
    let mut preferred = upstream.clone();
    preferred.exhausted = fallback.exhausted;
    preferred.upstream = true;
    preferred
}

/// Go `homeRetryRoundTiming`: the shortest positive retry-after seen in a round; zero
/// makes the next round immediate and a negative one invalid.
#[derive(Default)]
struct RoundTiming {
    retry_after: Option<Duration>,
    immediate: bool,
}

impl RoundTiming {
    fn observe(&mut self, fault: &RemoteFault) {
        if self.immediate {
            return;
        }
        match fault.retry_after() {
            Some(d) if d.is_zero() => {
                self.retry_after = None;
                self.immediate = true;
            }
            Some(d) if self.retry_after.is_none_or(|current| d < current) => self.retry_after = Some(d),
            _ => {}
        }
    }

    fn retry_after(&self) -> Option<Duration> {
        if self.immediate { None } else { self.retry_after }
    }
}

/// Go `executeHome`: rounds of [`remote_round`], retried within Home's request-retry
/// limit and the configured maximum wait.
async fn run_remote(
    cx: RemoteContext<'_>,
    remote: Arc<dyn crate::remote::RemoteDispatch>,
    base: Selection,
) -> Result<Done, RunError> {
    let pinned = cx.call.pinned().is_some_and(|p| !p.trim().is_empty());
    let max_wait = cx.policy.max_retry_interval;
    let default_retry = cx.policy.request_retry as i64;
    let allowed = |attempt: i64, limit: i64| home_retry_allowed(default_retry, attempt, limit);
    let mut limit: i64 = -1;
    let mut attempt: i64 = 0;
    let (mut pending, mut waited) = (false, false);
    let mut preferred_upstream: Option<RemoteFault> = None;
    loop {
        let error = match remote_round(&cx, remote.as_ref(), &base, attempt, &mut limit, pinned).await {
            Ok(done) => return Ok(done),
            Err(error) => *error,
        };
        if error.upstream {
            preferred_upstream = Some(error.clone());
        }
        if pending {
            // Go `pendingHomeRetryRoundDelay`: one wait for a round Home put on cooldown.
            if error.exhausted.is_none()
                && let crate::remote::RemoteErrorKind::Cooldown {
                    retry_after,
                    request_retry,
                } = error.kind
            {
                if !pinned && let Some(remote_limit) = request_retry {
                    limit = remote_limit;
                }
                if let Some(wait) = retry_after.filter(|w| !w.is_zero() && !max_wait.is_zero() && *w <= max_wait)
                    && allowed(attempt - 1, limit)
                {
                    if waited {
                        return Err(error.into_run_error());
                    }
                    tokio::time::sleep(wait).await;
                    waited = true;
                    continue;
                }
            }
        }
        // Go clears `retryRoundPending` here; every path below sets it again or returns.
        waited = false;
        if error.stop {
            return Err(error.into_run_error());
        }
        match remote_retry_wait(&error, attempt, max_wait, limit, pinned, &allowed) {
            Some(wait) => {
                if !wait.is_zero() {
                    tokio::time::sleep(wait).await;
                }
                attempt += 1;
                pending = true;
            }
            None => {
                let error = match (&preferred_upstream, error.exhausted) {
                    (Some(upstream), Some(_)) => preferred(error, Some(upstream)),
                    _ => error,
                };
                return Err(error.into_run_error());
            }
        }
    }
}

/// Go `homeRetryAllowed`: `attempt` is below the round limit, Home's when one was
/// observed (`limit >= 0`), else the configured request-retry.
fn home_retry_allowed(default_retry: i64, attempt: i64, limit: i64) -> bool {
    let limit = if limit < 0 { default_retry.max(0) } else { limit };
    attempt >= 0 && attempt < limit
}

/// Go `shouldRetryAfterErrorWithHomeRetryLimit` in Home mode.
fn remote_retry_wait(
    error: &RemoteFault,
    attempt: i64,
    max_wait: Duration,
    mut limit: i64,
    pinned: bool,
    allowed: &dyn Fn(i64, i64) -> bool,
) -> Option<Duration> {
    if matches!(error.kind, crate::remote::RemoteErrorKind::Busy { .. })
        || error.status() == 200
        || error.invalid
        || error.stop
    {
        return None;
    }
    if let crate::remote::RemoteErrorKind::Cooldown {
        request_retry: Some(remote_limit),
        ..
    } = error.kind
        && !pinned
    {
        limit = remote_limit;
    }
    let beyond = |wait: Duration| !wait.is_zero() && (max_wait.is_zero() || wait > max_wait);
    if let Some(exhausted) = error.exhausted {
        if !error.retry_round() || !allowed(attempt, limit) {
            return None;
        }
        if exhausted.retry_now {
            return Some(Duration::ZERO);
        }
        return match exhausted.retry_after {
            Some(wait) if beyond(wait) => None,
            Some(wait) => Some(wait),
            // Home answers with a cooldown next round if every credential still cools.
            None => Some(Duration::ZERO),
        };
    }
    if error.status() != 429 || !allowed(attempt, limit) {
        return None;
    }
    match error.retry_after() {
        Some(wait) if !wait.is_zero() && !beyond(wait) => Some(wait),
        _ => None,
    }
}

/// Go `executeHomeOnce`: picks until one credential succeeds, the round is exhausted,
/// or a failure ends the request. A failed credential's release is acknowledged before
/// anything else happens (Go `endHomeSelectionBeforeRedispatch`).
async fn remote_round(
    cx: &RemoteContext<'_>,
    remote: &dyn crate::remote::RemoteDispatch,
    base: &Selection,
    round: i64,
    limit: &mut i64,
    pinned: bool,
) -> Result<Done, Box<RemoteFault>> {
    // An async block: the fault is boxed once at the boundary.
    let round = async {
        let releases = crate::remote::PendingReleases::default();
        let settle = |releases: crate::remote::PendingReleases| async move {
            releases.settle().await.map_err(|error| {
                RemoteFault::local(Failure::Exec(ExecError::local(
                    503,
                    cpa_core::exec::FailureScope::Credential,
                    format!("home_unavailable: Home did not acknowledge credential release: {error}"),
                )))
            })
        };
        let max_credentials = cx.policy.max_retry_credentials;
        // Go's streaming Home loop: only `excluded` goes to Home, and a credential whose
        // stream died of a connection-lifecycle error (or a downstream-WebSocket 426)
        // may come back once. Buffered calls exclude everything tried.
        let stream = cx.call.stream;
        let websocket = cx.call.turn.is_some();
        let mut tried: Vec<String> = Vec::new();
        let mut excluded: Vec<String> = Vec::new();
        let mut same_retries: std::collections::HashMap<String, u32> = Default::default();
        let mut last_id = String::new();
        let mut same_pending = false;
        let mut last: Option<RemoteFault> = None;
        let mut upstream: Option<RemoteFault> = None;
        let mut timing = RoundTiming::default();
        // Go `homeAuthCount`: the buffered loop counts every pick; the streaming loop
        // counts only failed executions, so a same-credential retry still advances it.
        let mut count: i64 = 0;
        loop {
            if !stream || count == 0 {
                count += 1;
            }
            let allow_same =
                stream && same_pending && !last_id.is_empty() && same_retries.get(&last_id).copied().unwrap_or(0) == 0;
            let capped = max_credentials > 0 && tried.len() >= max_credentials;
            if capped && !allow_same {
                return Err(match last {
                    Some(last) => preferred(last, upstream.as_ref()).exhausted(timing.retry_after(), true),
                    None => RemoteFault::local(no_auth(base)),
                });
            }
            let mut selection = base.clone();
            selection.retry_round = round.max(0) as usize;
            selection.exclude = if stream { excluded.clone() } else { tried.clone() };
            let request = remote_request(cx.call, &selection, count, cx.trace);
            // Go `retainedHomeSessionSelection`: a WebSocket turn runs on the pick its
            // session's pooled socket kept, when it still fits.
            let kept = cx
                .call
                .turn
                .as_ref()
                .and_then(|turn| turn.home.as_ref())
                .and_then(|home| {
                    home.reuse(
                        count <= 1 && round <= 0,
                        &selection.model,
                        &selection.exclude,
                        cx.call.pinned(),
                        &releases,
                    )
                });
            let picked = match kept {
                Some((lease, user_key, request_retry)) => Ok((lease, request_retry, user_key)),
                None => {
                    let home = cx.call.turn.as_ref().and_then(|turn| turn.home.as_ref());
                    // Go `endHomeSelectionBeforeRedispatch` for a kept pick that ended.
                    if home.is_some() {
                        settle(releases.clone()).await?;
                    }
                    let picked = cx
                        .rt
                        .acquire_remote(remote, selection.clone(), request, &releases)
                        .await;
                    if let (Some(home), Ok((_, request_retry, _))) = (home, &picked) {
                        home.granted(*request_retry);
                    }
                    picked
                }
            };
            let (lease, request_retry, user_key) = match picked {
                Ok(picked) => picked,
                Err(error) => {
                    let code = error.code.clone();
                    let pick = RemoteFault::from_pick(error);
                    let Some(previous) = last else {
                        return Err(pick);
                    };
                    let fallback = preferred(previous, upstream.as_ref());
                    if let crate::remote::RemoteErrorKind::Cooldown {
                        retry_after,
                        request_retry,
                    } = pick.kind
                    {
                        if !pinned && let Some(remote_limit) = request_retry {
                            *limit = remote_limit;
                        }
                        return Err(fallback.exhausted(retry_after, false));
                    }
                    // Go `shouldReturnLastErrorOnPickFailure` in Home mode.
                    if matches!(
                        code.to_lowercase().as_str(),
                        "auth_not_found" | "auth_unavailable" | "request_retry_exceeded"
                    ) {
                        return Err(
                            fallback.exhausted(timing.retry_after(), code.eq_ignore_ascii_case("auth_unavailable"))
                        );
                    }
                    return Err(pick);
                }
            };
            // Go `observeHomeRetryLimit`.
            match request_retry.filter(|_| !pinned) {
                Some(home_limit) => *limit = home_limit,
                None => {
                    let local = cx.policy.retry_limit(&lease.credential) as i64;
                    if *limit < 0 || local > *limit {
                        *limit = local;
                    }
                }
            }
            let id = lease.credential.id.clone();
            if capped && id != last_id {
                // The cap allowed only the pending same-credential retry.
                drop(lease);
                settle(releases.clone()).await?;
                return Err(match last {
                    Some(last) => preferred(last, upstream.as_ref()).exhausted(timing.retry_after(), true),
                    None => RemoteFault::local(no_auth(base)),
                });
            }
            if stream && !last_id.is_empty() && id != last_id {
                same_pending = false;
            }
            if tried.contains(&id) {
                if !stream || excluded.contains(&id) {
                    drop(lease);
                    settle(releases.clone()).await?;
                    return Err(match last {
                        Some(last) => preferred(last, upstream.as_ref()).exhausted(timing.retry_after(), false),
                        None => RemoteFault::local(Failure::Exec(ExecError::local(
                            503,
                            cpa_core::exec::FailureScope::Credential,
                            "request_retry_exceeded: home returned a previously tried auth",
                        ))),
                    });
                }
                let retries = same_retries.entry(id.clone()).or_default();
                *retries += 1;
                if *retries > 1 {
                    // Once only: repeated failures still rotate away.
                    excluded.push(id);
                    drop(lease);
                    settle(releases.clone()).await?;
                    continue;
                }
            } else {
                tried.push(id.clone());
            }
            cx.trace.selected(&lease.credential);
            // Go `setHomeUserAPIKeyOnGinContext`: in Home mode the local client keys are
            // cleared, so the key Home authenticated is the caller for this attempt and
            // every later one (usage records, caller-scoped executor state). A pick
            // without one keeps the previous key.
            if !user_key.is_empty() {
                *cx.user_key.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = user_key;
            }
            let user_key = cx
                .user_key
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let home_caller = (!user_key.is_empty()).then(|| {
                let mut request = cx.request.clone();
                request.caller.principal = user_key.clone();
                let usage = cx.usage.map(|facts| {
                    let mut facts = (**facts).clone();
                    facts.client.api_key = user_key;
                    Arc::new(facts)
                });
                (request, usage)
            });
            let (request, usage) = match &home_caller {
                Some((request, usage)) => (request, usage.as_ref()),
                None => (cx.request, cx.usage),
            };
            // Go `executeHomeOnce`: Home's upstream model when it chose one, and only the
            // first model either way.
            let (mut models, mut alias) = registry::execution_models(cx.aliases, &lease.credential, &selection.model);
            if let Some(model) = lease
                .credential
                .attributes
                .get(crate::remote::UPSTREAM_MODEL)
                .map(|m| m.trim())
                .filter(|m| !m.is_empty())
            {
                models = vec![model.to_owned()];
            }
            models.truncate(1);
            // The dispatcher decided whether Home's alias mapping applies to this request
            // (Go `homeForceMappingAliasResult`); the response then reports the route model.
            if lease
                .credential
                .attributes
                .get(crate::remote::FORCE_MAPPING)
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
            {
                alias.force_mapping = true;
                alias.original_alias = selection.model.clone();
            }
            if models.is_empty() {
                drop(lease);
                settle(releases.clone()).await?;
                let fault = RemoteFault::local(Failure::Exec(ExecError::local(
                    503,
                    cpa_core::exec::FailureScope::Credential,
                    "auth_not_found: no execution models available",
                )));
                timing.observe(&fault);
                last = Some(fault);
                continue;
            }
            let selection_model = registry::selection_model(cx.aliases, &lease.credential, &selection.model);
            let target = Target {
                models: &models,
                selection_model: &selection_model,
                pooled: false,
                alias: &alias,
                compact: cx.compact,
                keep_model: cx.call.selection_model.is_some(),
                unauthorized: cx.unauthorized,
            };
            let cancelled = lease
                .remote_cancelled()
                .unwrap_or_else(|| Box::pin(std::future::pending()));
            // Go: draining the registry cancels the attempt context; the execution stops
            // and the round moves on (the registry then refuses new picks). A failed
            // preparation ends the attempt (Go reports it and redispatches), but it is
            // not an upstream error and does not advance the streaming count.
            let run = async {
                let mut lease = lease;
                match cx.rt.prepare_remote(&mut lease, cx.cfg).await {
                    Ok(()) => (
                        attempt(cx.rt, cx.cfg, cx.policy, cx.call, request, usage, lease, target, None).await,
                        false,
                    ),
                    Err(error) => {
                        lease.complete(Outcome::Failure(error.clone()));
                        (
                            Attempt::Next(Fault {
                                error,
                                bootstrap: false,
                            }),
                            true,
                        )
                    }
                }
            };
            let (outcome, prepare_failed) = tokio::select! {
                biased;
                outcome = run => outcome,
                _ = cancelled => (
                    Attempt::Next(Fault {
                        error: crate::remote::cancelled_error(),
                        bootstrap: false,
                    }),
                    false,
                ),
            };
            match outcome {
                Attempt::Done(done) => return Ok(done),
                Attempt::Stop(fault) => {
                    // Go's streaming loop acknowledges the release before honouring a
                    // stop; the buffered one ends without waiting.
                    if stream {
                        settle(releases.clone()).await?;
                    }
                    return Err(RemoteFault::from_attempt(fault, true));
                }
                Attempt::Next(fault) => {
                    if stream {
                        // Go `shouldExcludeHomeAuthAfterStreamError`.
                        let error = &fault.error;
                        let exclude = !(lifecycle(error) || (websocket && error.status == 426))
                            || same_retries.get(&id).copied().unwrap_or(0) > 0;
                        if exclude && !excluded.contains(&id) {
                            excluded.push(id.clone());
                        }
                        last_id = id.clone();
                        same_pending = !exclude;
                        if !prepare_failed {
                            count += 1;
                        }
                    }
                    let mut fault = RemoteFault::from_attempt(fault, false);
                    // A preparation never reached the upstream, in any round.
                    fault.upstream &= !prepare_failed;
                    if fault.upstream {
                        upstream = Some(fault.clone());
                    }
                    timing.observe(&fault);
                    last = Some(fault);
                    settle(releases.clone()).await?;
                }
            }
        }
    };
    round.await.map_err(Box::new)
}

/// Go `isConnectionLifecycleError` for a statusless error's text.
fn lifecycle(e: &ExecError) -> bool {
    if classify::go_status(e) != 0 {
        return false;
    }
    let lower = classify::error_text(e).trim().to_lowercase();
    matches!(
        lower.as_str(),
        "context canceled" | "context deadline exceeded" | "eof" | "unexpected eof"
    ) || [
        "websocket: close 1000",
        "websocket: close 1001",
        "websocket: close 1006",
        "unexpected eof",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Go's `&Error{Code: "auth_not_found", Message: "no auth available"}`.
fn no_auth(base: &Selection) -> Failure {
    Failure::Unavailable {
        code: "auth_not_found",
        providers: base.providers.clone(),
        model: base.model.clone(),
        cause: None,
        retry_after: None,
    }
}

/// Go's RPOP request for one pick: the route model, session hierarchy, downstream
/// headers (Home authenticates the client with them), and this round's exclusions.
/// The dispatcher canonicalizes the session (Go `homeDispatchSessionIDs`).
fn remote_request(call: &Call, selection: &Selection, count: i64, trace: &Trace) -> crate::remote::RemoteRequest {
    let headers = crate::remote::home_headers(&call.headers, Some(&call.caller));
    crate::remote::RemoteRequest {
        model: selection.model.clone(),
        session_id: selection.session.clone().unwrap_or_default(),
        parent_session_id: selection.session_parent.clone().unwrap_or_default(),
        headers,
        count,
        retry_round: selection.retry_round as i64,
        excluded: selection.exclude.clone(),
        pinned: call.pinned().unwrap_or_default().to_owned(),
        request_id: trace.request_id(),
        kind: if call.turn.is_some() {
            "websocket"
        } else if call.stream {
            "stream"
        } else {
            "http"
        },
        credential_policy: String::new(),
    }
}

/// Go `jitteredCooldownWait`: up to a quarter of the wait (at most 2s) extra, never past
/// the configured maximum, so synchronized clients spread out.
pub fn jitter(wait: Duration, max: Duration) -> Duration {
    let mut range = (wait / 4).min(Duration::from_secs(2));
    if !max.is_zero() {
        range = range.min(max.saturating_sub(wait));
    }
    if range.is_zero() {
        return wait;
    }
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    wait + Duration::from_nanos(hasher.finish() % range.as_nanos().max(1) as u64)
}

enum Attempt {
    Done(Done),
    /// Terminal for the whole request.
    Stop(Fault),
    /// Failed over; try the next credential.
    Next(Fault),
}

/// What one selected credential executes.
struct Target<'a> {
    /// Upstream model candidates (more than one is an alias pool).
    models: &'a [String],
    selection_model: &'a str,
    pooled: bool,
    alias: &'a AliasResult,
    compact: bool,
    /// The request model is not the selection model (Interactions agents): execute
    /// the request model, keep cooldown state on the selection model.
    keep_model: bool,
    /// Record an upstream 401 as a Home result (Go `reportHomeUnauthorized` for Home
    /// count-tokens attempts, which no executor reporter records).
    unauthorized: Option<&'a Arc<crate::usage_record::Facts>>,
}

#[allow(clippy::too_many_arguments)]
async fn attempt(
    rt: &Arc<Runtime>,
    cfg: &Config,
    policy: &Policy,
    call: &Call,
    request: &ExecRequest,
    usage: Option<&Arc<crate::usage_record::Facts>>,
    mut lease: Lease,
    target: Target<'_>,
    hooks: Option<&crate::plugins::interceptors::Hooks>,
) -> Attempt {
    let route_model = lease.selection.model.clone();
    let mut last = None;
    let mut refreshed = false;
    // Go `Options.ExecutionLifecycle`: a session turn's Home pick, which a pooled
    // upstream socket may keep beyond the turn.
    let home = call
        .turn
        .as_ref()
        .and_then(|turn| turn.home.as_ref())
        .filter(|_| lease.is_remote());
    let pick = home.map(|home| home.pick(&lease.credential.id));
    let mut guard = pick.clone().map(crate::home_session::PickGuard::new);
    let session = call.turn.as_ref().map(|turn| cpa_core::exec::ExecSession {
        lease: pick.as_ref().map(crate::home_session::Pick::lease),
        ..turn.session.clone()
    });
    for (i, upstream) in target.models.iter().enumerate() {
        // Go `stateModelForExecution`: the upstream model when Home chose one.
        let home_model = lease
            .credential
            .attributes
            .get(crate::remote::UPSTREAM_MODEL)
            .map(|m| m.trim())
            .filter(|m| !m.is_empty());
        let state = match home_model {
            Some(home) if upstream.trim().is_empty() => home.to_owned(),
            Some(_) => upstream.trim().to_owned(),
            None => registry::state_model(target.selection_model, &route_model, upstream, target.pooled),
        };
        lease.execution_model = state.clone();
        let mut req = attempt_request(
            request,
            cfg,
            &lease.credential,
            &route_model,
            upstream,
            target.keep_model,
            call.stream,
        );
        // Go `syncMetadataSessionToContext`: an LCP pick's session is the attempt's
        // canonical session, for `$CPA-SESSION-ID` and the usage record.
        let lcp = lease.lcp.clone();
        if let Some(m) = &lcp {
            req.session = Some(cpa_common::session::bound_session_identity(&m.session));
        }
        if let Some(on_selected) = call.on_selected() {
            on_selected(&lease.credential);
        }
        // Go `applyRequestAfterAuthInterceptor`: a termination ends the request before
        // the executor runs, without a result for the credential.
        if let Some(hooks) = hooks
            && let Err(error) = hooks
                .after_auth(
                    &mut req,
                    &lease.credential,
                    &route_model,
                    call.media.as_ref().map(|m| m.kind),
                )
                .await
        {
            lease.complete(Outcome::Neutral(error.clone()));
            return Attempt::Stop(Fault {
                error,
                bootstrap: false,
            });
        }
        let start = |credential: &cpa_core::credential::Credential, req: &mut ExecRequest| {
            usage.map(|facts| {
                let mut tracker = crate::usage_record::Tracker::start(rt, facts, credential, upstream);
                if let Some(m) = &lcp {
                    tracker.lcp_session(m);
                }
                req.usage = tracker.sink().with_capture(req.capture().clone());
                tracker
            })
        };
        let mut tracker = start(&lease.credential, &mut req);
        let session = session.as_ref();
        let execute = |credential: Arc<cpa_core::credential::Credential>, req: ExecRequest| async move {
            let credential = rt.for_executor(&credential);
            match (session, call.media.as_ref().map(|m| m.kind)) {
                (Some(session), _) => rt.executors.execute_in_session(&credential, req, cfg, session).await,
                // An after-auth interceptor may rewrite the path (Go's request_path metadata).
                (None, Some(MediaKind::Images)) => {
                    let path = req.request_path.clone();
                    rt.executors.images(&credential, req, &path, cfg).await
                }
                (None, Some(MediaKind::Videos)) => {
                    let path = req.request_path.clone();
                    rt.executors.videos(&credential, req, &path, cfg).await
                }
                (None, None) => rt.executors.execute(&credential, req, cfg).await,
            }
        };
        tracing::trace!(target: "cpa_latency", stage = "selected");
        let mut executed = execute(lease.credential.clone(), req.clone()).await;
        // Go `tryRefreshAfterUnauthorized`: one refresh-and-retry per credential. Home
        // credentials never refresh here (Go home_unauthorized_refresh): the 401 goes to
        // Home and the round moves on, even when a local credential shares the ID.
        if let Err(error) = &executed
            && !refreshed
            && !lease.is_remote()
            && classify::is_unauthorized(error)
            && let Some(current) = rt.refresh_after_unauthorized(&lease.credential, cfg).await
        {
            refreshed = true;
            if let Some(t) = tracker.take() {
                t.fail(error);
            }
            lease.credential = current;
            tracker = start(&lease.credential, &mut req);
            executed = execute(lease.credential.clone(), req).await;
        }
        if let (Ok(response), Some(t)) = (&executed, tracker.as_mut()) {
            t.arrived(&response.headers);
        }
        let fault = match executed {
            Ok(response) => match finish(call, response).await {
                Ok(done) => {
                    let done = track_usage(done, tracker.take());
                    let done = if target.alias.force_mapping && !target.alias.original_alias.is_empty() {
                        rewrite_model(done, &target.alias.original_alias)
                    } else {
                        done
                    };
                    let user_key = &request.caller.principal;
                    let Some(lease) = keep_home(home, guard.take(), lease, user_key) else {
                        return Attempt::Done(done);
                    };
                    return Attempt::Done(match done {
                        Done::Stream { headers, first, rest } => {
                            // A good first chunk answers a bounded window's probe now,
                            // not when the stream ends.
                            lease.accepted();
                            Done::Stream {
                                headers,
                                first,
                                rest: Completing::new(rest, lease).boxed(),
                            }
                        }
                        buffered => {
                            lease.complete(Outcome::Success);
                            buffered
                        }
                    });
                }
                Err(fault) => {
                    if let Some(t) = tracker.take() {
                        t.fail(&fault.error);
                    }
                    fault
                }
            },
            Err(error) => {
                if let Some(t) = tracker.take() {
                    t.fail(&error);
                }
                Fault {
                    error,
                    bootstrap: false,
                }
            }
        };
        let error = &fault.error;
        if let Some(facts) = target.unauthorized
            && classify::is_unauthorized(error)
        {
            crate::usage_record::publish_home_unauthorized(
                rt,
                facts,
                &lease.credential,
                &crate::remote::selection_provider(&lease.credential),
                &state,
                &classify::error_text(error),
            );
        }
        let action = policy.error_action(&lease.credential, error);
        let neutral = (target.compact && classify::is_compact_neutral(error) && !action.force_cooldown)
            || (call.operation == Operation::CountTokens
                && classify::is_count_endpoint_missing(error, upstream)
                && !action.force_cooldown);
        let outcome = if neutral {
            Outcome::Neutral(error.clone())
        } else {
            Outcome::Failure(error.clone())
        };
        let stop = if action.matched {
            action.stop
        } else {
            (target.compact && classify::is_compact_fault(error)) || classify::is_request_invalid(error)
        };
        // A credential-wide quota ends this credential's model pool.
        if stop || i + 1 == target.models.len() || classify::credential_scoped(error) {
            // Go `End`: the socket that retained the pick closes before the release.
            drop(guard.take());
            lease.complete(outcome);
            return if stop {
                Attempt::Stop(fault)
            } else {
                Attempt::Next(fault)
            };
        }
        lease.note(&state, &outcome);
        last = Some(fault);
    }
    Attempt::Next(last.expect("at least one model was attempted"))
}

/// Go `retainHomeWebsocketSelection`: after a successful turn, the Home lease moves to
/// the pick a pooled upstream socket retained, which the session keeps for its next
/// turn. Returns the lease when no socket retained the pick.
fn keep_home(
    home: Option<&Arc<crate::home_session::SessionHome>>,
    guard: Option<crate::home_session::PickGuard>,
    lease: Lease,
    user_key: &str,
) -> Option<Lease> {
    let (Some(home), Some(mut guard)) = (home, guard) else {
        return Some(lease);
    };
    let pick = guard.defuse()?;
    let (credential, route) = (lease.credential.id.clone(), lease.selection.model.clone());
    match pick.keep(lease) {
        None => {
            home.keep(pick, &credential, &route, user_key.to_owned());
            None
        }
        Some(lease) => {
            pick.close();
            Some(lease)
        }
    }
}

/// Go's client request metadata for usage records (handlers.go `GetContextWithCancel`
/// and `syncMetadataSessionToContext`).
fn usage_client(
    cfg: &Config,
    call: &Call,
    trace: &Trace,
    session: &crate::session::Session,
) -> crate::usage_record::Client {
    crate::usage_record::Client {
        session_id: session.id.clone().unwrap_or_default(),
        parent_session_id: session.parent.clone().unwrap_or_default(),
        is_fork: session.fork,
        node_kind: String::new(),
        is_compaction: false,
        request_id: trace.request_id(),
        // Session turns arrive on the WebSocket upgrade (a GET).
        endpoint: format!(
            "{} {}",
            if call.turn.is_some() { "GET" } else { "POST" },
            call.request_path
        ),
        api_key: call.caller.principal.clone(),
        ..request_client(cfg, &call.headers, call.peer)
    }
}

/// The downstream peer's part of Go's client request metadata: addresses and agent.
// ponytail: `server.trusted-proxies` is read from the request's config snapshot; Go
// applies it at startup only.
pub(crate) fn request_client(
    cfg: &Config,
    headers: &axum::http::HeaderMap,
    peer: Option<std::net::SocketAddr>,
) -> crate::usage_record::Client {
    let trusted = cfg.derived(|c| cpa_core::config::TrustedProxies::new(&c.trusted_proxies));
    let text = |v: &axum::http::HeaderValue| String::from_utf8_lossy(v.as_bytes()).into_owned();
    let forwarded: Vec<String> = headers.get_all("x-forwarded-for").iter().map(text).collect();
    crate::usage_record::Client {
        client_ip: peer.map(|p| p.ip().to_string()).unwrap_or_default(),
        resolved_client_ip: trusted
            .client_ip(peer, |name| headers.get(name).map(|v| v.as_bytes()))
            .trim()
            .to_owned(),
        x_forwarded_for: forwarded.join(", ").trim().to_owned(),
        user_agent: headers
            .get("user-agent")
            .map(text)
            .unwrap_or_default()
            .trim()
            .to_owned(),
        ..Default::default()
    }
}

/// Feeds a finished attempt's client-format response to its usage record: a buffered
/// body publishes now, a stream when it ends, fails or is dropped.
fn track_usage(done: Done, tracker: Option<crate::usage_record::Tracker>) -> Done {
    let Some(mut tracker) = tracker else { return done };
    match done {
        Done::Buffered { headers, body } => {
            tracker.body(&body);
            tracker.succeed();
            Done::Buffered { headers, body }
        }
        Done::Stream { headers, first, rest } => {
            if let Some(first) = &first {
                tracker.event(first);
            }
            Done::Stream {
                headers,
                first,
                rest: Tracked {
                    inner: rest,
                    tracker: Some(tracker),
                }
                .boxed(),
            }
        }
    }
}

/// A client stream that reports its events to the attempt's usage record.
struct Tracked {
    inner: ExecStream,
    tracker: Option<crate::usage_record::Tracker>,
}

impl futures_util::Stream for Tracked {
    type Item = Result<Bytes, ExecError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let item = std::task::ready!(self.inner.poll_next_unpin(cx));
        match &item {
            Some(Ok(event)) => {
                if let Some(t) = self.tracker.as_mut() {
                    t.event(event);
                }
            }
            Some(Err(error)) => {
                if let Some(t) = self.tracker.take() {
                    t.fail(error);
                }
            }
            None => {
                if let Some(t) = self.tracker.take() {
                    t.succeed();
                }
            }
        }
        std::task::Poll::Ready(item)
    }
}

/// The request one upstream model attempt executes: the upstream model (unless the
/// request keeps its own model), with Go's `attachResolvedExecutionModelInfo` binding,
/// then `attachResolvedHomeModelInfo` for a Home credential executing the selection's
/// model. Go's stream path binds Home's model before the local one, so the local
/// binding never lends it configuration-update support there.
fn attempt_request(
    request: &ExecRequest,
    cfg: &Config,
    credential: &cpa_core::credential::Credential,
    route_model: &str,
    upstream: &str,
    keep_model: bool,
    stream: bool,
) -> ExecRequest {
    let mut req = request.clone();
    if !keep_model {
        req.model = upstream.to_owned();
    }
    req.resolved_model = crate::capabilities::resolve_attempt(
        cfg,
        credential,
        route_model,
        upstream,
        keep_model.then_some(request.model.as_str()),
    );
    let local = req.resolved_model.as_ref().filter(|_| !stream);
    if !keep_model && let Some(home) = crate::capabilities::bind_home(credential, local) {
        req.resolved_model = Some(home);
    }
    req
}

/// Bootstraps a stream (first event before committing) or buffers a body.
async fn finish(call: &Call, response: cpa_core::exec::ExecResponse) -> Result<Done, Fault> {
    let fault = |error| Fault { error, bootstrap: true };
    match response.body {
        ResponseBody::Buffered(body) => Ok(Done::Buffered {
            headers: response.headers,
            body,
        }),
        ResponseBody::Stream(mut stream) if call.stream => {
            let first = loop {
                match stream.next().await {
                    Some(Ok(bytes)) if bytes.is_empty() => continue,
                    Some(Ok(bytes)) => break bytes,
                    Some(Err(error)) => return Err(fault(error)),
                    // Go conductor_stream.go: an empty stream is a failed attempt.
                    None => return Err(fault(classify::empty_stream())),
                }
            };
            Ok(Done::Stream {
                headers: response.headers,
                first: Some(first),
                rest: stream,
            })
        }
        ResponseBody::Stream(mut stream) => {
            let mut body = BytesMut::new();
            while let Some(event) = stream.next().await {
                body.extend_from_slice(&event.map_err(|error| Fault {
                    error,
                    bootstrap: false,
                })?);
            }
            Ok(Done::Buffered {
                headers: response.headers,
                body: body.freeze(),
            })
        }
    }
}

/// Go's force-mapped model rewrite: `rewriteModelInResponse` over a buffered body and
/// `StreamRewriter.RewriteChunk` over each stream event.
fn rewrite_model(done: Done, target: &str) -> Done {
    match done {
        Done::Buffered { headers, body } => Done::Buffered {
            headers,
            body: rewrite_body(&body, target),
        },
        Done::Stream { headers, first, rest } => {
            let target_owned = target.to_owned();
            let rest = rest
                .map(move |item| item.map(|event| rewrite_event(&event, &target_owned)))
                .boxed();
            Done::Stream {
                headers,
                first: first.map(|event| rewrite_event(&event, target)),
                rest,
            }
        }
    }
}

const MODEL_PATHS: [&str; 5] = [
    "model",
    "modelVersion",
    "response.model",
    "response.modelVersion",
    "message.model",
];

/// Go `rewriteModelInResponse`: `sjson.SetBytes` on each model path that exists, so
/// every other byte is kept.
fn rewrite_json(data: &[u8], target: &str) -> Vec<u8> {
    let mut out = data.to_vec();
    for path in MODEL_PATHS {
        if cpa_common::json::get(&out, path).exists() {
            cpa_common::json::set_str(&mut out, path, target);
        }
    }
    out
}

/// A buffered body: a JSON document, or SSE text rewritten line by line.
fn rewrite_body(body: &Bytes, target: &str) -> Bytes {
    if body.trim_ascii_start().starts_with(b"{") {
        Bytes::from(rewrite_json(body, target))
    } else {
        rewrite_lines(body, target)
    }
}

/// Go `StreamRewriter.RewriteChunk` for one complete event: a bare JSON chunk comes
/// back trimmed and rewritten, SSE text line by line.
// ponytail: executors emit whole events, so Go's buffering of partial and glued events
// is not ported; an invalid `data:` JSON line passes through unchanged.
fn rewrite_event(event: &Bytes, target: &str) -> Bytes {
    let trimmed = event.trim_ascii();
    if trimmed.starts_with(b"{") && cpa_common::json::valid(trimmed) {
        return Bytes::from(rewrite_json(trimmed, target));
    }
    rewrite_lines(event, target)
}

/// Go `rewriteSSEPayloadLines`: `data: {..}` / `data:{..}` lines holding valid JSON.
fn rewrite_lines(payload: &Bytes, target: &str) -> Bytes {
    let mut changed = false;
    let mut out = Vec::with_capacity(payload.len());
    for (i, line) in payload.split(|&b| b == b'\n').enumerate() {
        if i > 0 {
            out.push(b'\n');
        }
        let data = line.strip_prefix(b"data: ").or_else(|| line.strip_prefix(b"data:"));
        match data {
            Some(json) if json.starts_with(b"{") && cpa_common::json::valid(json) => {
                out.extend_from_slice(&line[..line.len() - json.len()]);
                let rewritten = rewrite_json(json, target);
                changed |= rewritten != json;
                out.extend_from_slice(&rewritten);
            }
            _ => out.extend_from_slice(line),
        }
    }
    if changed { Bytes::from(out) } else { payload.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go finds a direct response through the Home round marker: the raw upstream
    /// answer goes out unchanged, without the safe `Retry-After`.
    #[test]
    fn direct_errors_stay_direct_under_a_home_round_marker() {
        let mut raw = ExecError::local(429, cpa_core::exec::FailureScope::Credential, "raw fast-mode body");
        raw.direct = true;
        raw.headers
            .insert(axum::http::header::CONTENT_TYPE, "text/plain".parse().unwrap());
        let failure = Failure::Remote {
            error: raw,
            retry_after: Some(2),
        };
        assert!(failure.direct().is_some());
        let response = crate::errors::write(&failure, |_, _| unreachable!());
        assert_eq!(response.status().as_u16(), 429);
        assert_eq!(response.headers()["content-type"], "text/plain");
        assert!(response.headers().get("retry-after").is_none());
    }

    /// Go `isConnectionLifecycleError` on the texts executors produce.
    #[test]
    fn lifecycle_errors_follow_go() {
        let transport = |text: &str| ExecError::local(502, cpa_core::exec::FailureScope::Transport, text);
        assert!(lifecycle(&transport("websocket: close 1000 (normal)")));
        assert!(lifecycle(&transport(
            "websocket: close 1006 (abnormal closure): unexpected EOF"
        )));
        assert!(lifecycle(&transport("EOF")));
        assert!(!lifecycle(&transport("websocket: close 1011 (internal server error)")));
        assert!(!lifecycle(&transport("read failed")));
        // A status keeps the credential/status path, whatever the text says.
        let statused = ExecError::local(500, cpa_core::exec::FailureScope::Credential, "unexpected EOF");
        assert!(!lifecycle(&statused));
    }

    /// Go starts the non-stream keep-alive in the non-stream generate handlers only, and
    /// `NonStreamingKeepAliveInterval` treats 0 and below as off.
    #[test]
    fn nonstream_keepalive_only_for_nonstream_generate() {
        let call = |operation, stream| Call {
            entry: Format::Claude,
            response: Format::Claude,
            operation,
            model: "m".into(),
            body: Bytes::new(),
            stream,
            alt: None,
            headers: HeaderMap::new(),
            caller: Caller {
                principal: String::new(),
                source: "",
            },
            forced_provider: None,
            selection_model: None,
            execution_session: None,
            request_path: String::new(),
            peer: None,
            turn: None,
            media: None,
        };
        let cfg = |yaml: &str| Config::parse(yaml).unwrap();
        let on = cfg("requests:\n  nonstream-keepalive-interval: 3\n");
        assert_eq!(
            nonstream_keepalive(&call(Operation::Generate, false), &on),
            Some(Duration::from_secs(3))
        );
        assert_eq!(nonstream_keepalive(&call(Operation::Generate, true), &on), None);
        assert_eq!(nonstream_keepalive(&call(Operation::CountTokens, false), &on), None);
        for off in [
            "{}\n",
            "requests:\n  nonstream-keepalive-interval: 0\n",
            "requests:\n  nonstream-keepalive-interval: -5\n",
        ] {
            assert_eq!(
                nonstream_keepalive(&call(Operation::Generate, false), &cfg(off)),
                None,
                "{off}"
            );
        }
        // The v7 top-level key migrates to requests.nonstream-keepalive-interval.
        let legacy = cfg("nonstream-keepalive-interval: 2\n");
        assert_eq!(
            nonstream_keepalive(&call(Operation::Generate, false), &legacy),
            Some(Duration::from_secs(2))
        );
    }

    /// Each attempt carries Go's binding for its credential and upstream model; a kept
    /// request model (Go `restoreExecutionModel`) routes Codex by that model. Expected
    /// values are the Go goldens' "codex configured" and "codex restore" cases.
    #[test]
    fn attempts_bind_resolved_model_like_go() {
        let cfg = Config::parse(
            "config-version: 8\napi-keys:\n  codex:\n    - base-url: https://codex.example\n      models:\n        - name: gpt-6-sol\n          alias: sol\n          is-compat: true\n      keys:\n        - api-key: xk1\n",
        )
        .unwrap();
        let credential = cpa_core::config::credentials::from_config(&cfg).remove(0);
        let request = ExecRequest {
            operation: Operation::Generate,
            source_format: Format::OpenAI,
            response_format: Format::OpenAI,
            requested_model: "sol".into(),
            model: "sol".into(),
            original_body: Bytes::new(),
            body: Bytes::new(),
            stream: false,
            alt: None,
            session: None,
            execution_session: None,
            derived_session: None,
            request_path: String::new(),
            headers: HeaderMap::new(),
            caller: Caller {
                principal: String::new(),
                source: "",
            },
            resolved_model: None,
            usage: Default::default(),
        };
        let req = attempt_request(&request, &cfg, &credential, "sol", "gpt-6-sol", false, false);
        assert_eq!(req.model, "gpt-6-sol");
        let bound = req.resolved_model.unwrap();
        assert_eq!(
            (bound.info.id.as_str(), bound.source, bound.is_compat()),
            ("gpt-6-sol", cpa_core::exec::ResolvedSource::ApiKey, true)
        );
        let kept = attempt_request(&request, &cfg, &credential, "selection", "gpt-6-sol", true, false);
        assert_eq!(kept.model, "sol");
        let bound = kept.resolved_model.unwrap();
        assert_eq!((bound.info.id.as_str(), bound.is_compat()), ("sol", false));

        // Go `attachResolvedHomeModelInfo` after the local binding: Home's definition
        // replaces it, lends local configuration-update support only off the stream path,
        // and is skipped when the attempt keeps the request's own model.
        let mut home = credential.clone();
        home.attributes
            .insert(crate::remote::MODEL_INFO.into(), r#"{"id":"gpt-6-sol"}"#.into());
        let caps = |req: &ExecRequest| {
            let bound = req.resolved_model.as_ref().unwrap();
            let caps = cpa_common::thinking::ModelCaps::from(&bound.info);
            (bound.source, caps.support_configuration_update, bound.is_compat())
        };
        let cfg = Config::parse(
            "config-version: 8\napi-keys:\n  codex:\n    - base-url: https://codex.example\n      models:\n        - name: gpt-6-sol\n          alias: sol\n          is-compat: true\n          support-configuration-update: true\n      keys:\n        - api-key: xk1\n",
        )
        .unwrap();
        let home_source = cpa_core::exec::ResolvedSource::Home;
        let buffered = attempt_request(&request, &cfg, &home, "sol", "gpt-6-sol", false, false);
        assert_eq!(
            caps(&buffered),
            (home_source, true, false),
            "local is-compat never carries"
        );
        let streamed = attempt_request(&request, &cfg, &home, "sol", "gpt-6-sol", false, true);
        assert_eq!(caps(&streamed), (home_source, false, false));
        let kept = attempt_request(&request, &cfg, &home, "selection", "gpt-6-sol", true, false);
        assert_eq!(
            kept.resolved_model.unwrap().source,
            cpa_core::exec::ResolvedSource::ApiKey
        );
    }

    #[test]
    fn failure_texts_match_go_shapes() {
        assert_eq!(
            Failure::UnknownModel("claude-sonnet-4-6".into()).text(),
            r#"{"error":{"message":"unknown provider for model claude-sonnet-4-6","type":"invalid_request_error","code":"model_not_found","param":"model"}}"#
        );
        let cooldown = Failure::Cooldown {
            model: "m".into(),
            provider: "claude".into(),
            wait: Duration::from_millis(59_400),
            cause: Some(r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#.into()),
        };
        assert_eq!(
            cooldown.text(),
            r#"{"error":{"code":"model_cooldown","last_upstream_error":"rate_limit_error: slow down","message":"All credentials for model m are cooling down via provider claude (last error: rate_limit_error: slow down)","model":"m","provider":"claude","reset_seconds":60,"reset_time":"59s"}}"#
        );
        assert_eq!(cooldown.retry_after(), Some(60));
        assert_eq!(
            (cooldown.status(), Failure::UnknownModel(String::new()).status()),
            (429, 400)
        );
        let none = Failure::Unavailable {
            code: "auth_not_found",
            providers: vec!["claude".into()],
            model: " m ".into(),
            cause: None,
            retry_after: None,
        };
        assert_eq!(
            none.text(),
            "auth_not_found: no auth available (providers=claude, model=m); check Claude auth/key session and cooldown state via /v0/management/auth-files"
        );
        assert_eq!((none.status(), none.retry_after()), (503, None));
    }

    #[test]
    fn force_mapping_rewrites_json_and_sse_model_fields() {
        let body = Bytes::from_static(br#"{"id":"x","model":"claude-opus-5","message":{"model":"claude-opus-5"}}"#);
        assert_eq!(
            rewrite_body(&body, "opus"),
            r#"{"id":"x","model":"opus","message":{"model":"opus"}}"#
        );
        let event = Bytes::from_static(b"event: message_start\ndata: {\"message\":{\"model\":\"up\"}}\n\n");
        assert_eq!(
            rewrite_event(&event, "alias"),
            "event: message_start\ndata: {\"message\":{\"model\":\"alias\"}}\n\n"
        );
        let untouched = Bytes::from_static(b"data: [DONE]\n\n");
        assert_eq!(rewrite_event(&untouched, "alias"), untouched);
        // sjson keeps every other byte: spacing, escapes, number spelling, CRLF.
        let crlf = Bytes::from_static(b"data: {\"model\" : \"up\", \"x\":\"\\u00e9\", \"n\":1.50}\r\n\r\n");
        assert_eq!(
            rewrite_event(&crlf, "alias"),
            "data: {\"model\" : \"alias\", \"x\":\"\\u00e9\", \"n\":1.50}\r\n\r\n"
        );
        // A path that is absent is not added; `data:` needs `{` right after the prefix.
        let spaced = Bytes::from_static(b"data:  {\"model\":\"up\"}\n\n");
        assert_eq!(rewrite_event(&spaced, "alias"), spaced);
        // Go trims a bare JSON stream chunk.
        let bare = Bytes::from_static(b" {\"modelVersion\":\"up\"}\n");
        assert_eq!(rewrite_event(&bare, "alias"), r#"{"modelVersion":"alias"}"#);
    }

    /// A Home `model_cooldown` pick failure.
    fn home_cooldown(retry_after: Duration, request_retry: Option<i64>) -> RemoteFault {
        RemoteFault::from_pick(crate::remote::RemoteError {
            error: ExecError::local(
                429,
                cpa_core::exec::FailureScope::Credential,
                "model_cooldown: all Home credentials are cooling down",
            ),
            code: "model_cooldown".into(),
            kind: crate::remote::RemoteErrorKind::Cooldown {
                retry_after: Some(retry_after),
                request_retry,
            },
        })
    }

    /// [`remote_retry_wait`] as Go `shouldRetryAfterErrorWithHomeRetryLimit` asks it,
    /// with `request-retry` configured.
    fn retry_wait(
        error: &RemoteFault,
        request_retry: i64,
        attempt: i64,
        max_wait: Duration,
        limit: i64,
        pinned: bool,
    ) -> Option<Duration> {
        let allowed = |attempt: i64, limit: i64| home_retry_allowed(request_retry, attempt, limit);
        remote_retry_wait(error, attempt, max_wait, limit, pinned, &allowed)
    }

    /// Go `TestHomeRetryPolicyAllowsRemoteCooldownWithoutLocalCredentials`: a Home
    /// cooldown is retried within request-retry and the maximum wait, and an exhausted
    /// round without timing starts the next one at once, even with no maximum wait.
    #[test]
    fn home_cooldowns_retry_within_request_retry_and_the_maximum_wait() {
        let ms = Duration::from_millis;
        let cooldown = home_cooldown(ms(10), None);
        assert_eq!(
            retry_wait(&cooldown, 1, 0, Duration::from_secs(1), -1, false),
            Some(ms(10))
        );
        assert_eq!(
            retry_wait(&cooldown, 1, 1, Duration::from_secs(1), -1, false),
            None,
            "after the configured round"
        );
        assert_eq!(
            retry_wait(&cooldown, 1, 0, Duration::ZERO, -1, false),
            None,
            "no maximum wait"
        );
        let unavailable = ExecError::local(502, cpa_core::exec::FailureScope::Credential, "upstream unavailable");
        let round = RemoteFault::local(Failure::Exec(unavailable)).exhausted(None, false);
        assert_eq!(
            retry_wait(&round, 1, 0, Duration::ZERO, -1, false),
            Some(Duration::ZERO)
        );
        // ponytail: Go's negative-retry-after case (`homeRetryRoundTiming.invalid`) has no
        // Rust counterpart: Duration is unsigned, Home's `retry_after_ms` is kept only
        // when positive (as in Go `home_concurrency.go`), and the upstream Retry-After
        // parsers drop negative values; only Go's test-made errors carry one.
    }

    /// Go `TestHomeRetryPolicyUsesRemoteCredentialOverrideBeforeSelection`: a cooldown's
    /// request_retry replaces the configured one before any credential was picked,
    /// including an explicit 0, but never for a pinned request.
    #[test]
    fn a_cooldown_request_retry_overrides_the_configured_one_unless_pinned() {
        let ms = Duration::from_millis;
        let second = Duration::from_secs(1);
        let one = home_cooldown(ms(10), Some(1));
        assert_eq!(retry_wait(&one, 0, 0, second, -1, false), Some(ms(10)));
        assert_eq!(
            retry_wait(&one, 0, 1, second, -1, false),
            None,
            "one additional round only"
        );
        assert_eq!(
            retry_wait(&one, 0, 0, second, -1, true),
            None,
            "pinned keeps its own limit"
        );
        let zero = home_cooldown(ms(10), Some(0));
        assert_eq!(retry_wait(&zero, 3, 0, second, -1, false), None, "an explicit 0 wins");
    }

    /// Go `TestHomeRetryRoundCredentialLimitStartsNextRoundImmediately`: a round ended
    /// by max-retry-credentials starts the next one at once, while the client still
    /// sees the round's retry-after.
    #[test]
    fn a_credential_limit_starts_the_next_round_immediately() {
        let mut limited = ExecError::local(429, cpa_core::exec::FailureScope::Credential, "credential rate limited");
        limited.retry_after = Some(Duration::from_secs(5));
        let round = RemoteFault::local(Failure::Exec(limited)).exhausted(Some(Duration::from_secs(5)), true);
        assert_eq!(
            retry_wait(&round, 1, 0, Duration::from_secs(1), -1, false),
            Some(Duration::ZERO)
        );
        assert_eq!(round.safe_retry_after(), Some(5));
    }

    #[test]
    fn jitter_stays_within_go_bounds() {
        for _ in 0..50 {
            let w = jitter(Duration::from_secs(4), Duration::ZERO);
            assert!(w >= Duration::from_secs(4) && w < Duration::from_secs(5));
            let capped = jitter(Duration::from_secs(20), Duration::from_secs(21));
            assert!(capped >= Duration::from_secs(20) && capped < Duration::from_secs(21));
        }
        assert_eq!(
            jitter(Duration::from_secs(5), Duration::from_secs(5)),
            Duration::from_secs(5)
        );
        assert_eq!(jitter(Duration::ZERO, Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn summaries_follow_go_extract() {
        assert_eq!(
            upstream_summary(r#"{"error":{"code":"x","message":"boom"}}"#),
            "x: boom"
        );
        assert_eq!(
            upstream_summary(r#"{"error":{"type":"overloaded","message":"overloaded now"}}"#),
            "overloaded now"
        );
        assert_eq!(upstream_summary(r#"{"error":"flat"}"#), "flat");
        assert_eq!(upstream_summary("plain text"), "plain text");
        assert_eq!(upstream_summary("status 500: {\"message\":\"m\"}"), "m");
    }
}
