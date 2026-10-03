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
}

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
    fn pinned(&self) -> Option<&str> {
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

#[derive(Debug)]
pub enum Failure {
    /// An executor or upstream error, rendered by the route's error shape.
    Exec(ExecError),
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
            Failure::Exec(e) => classify::response_status(e),
            Failure::UnknownModel(_) => 400,
            Failure::ImageOnly(_) | Failure::Unavailable { .. } => 503,
            Failure::Cooldown { .. } => 429,
        }
    }

    /// Go `err.Error()`: what route error writers render.
    pub fn text(&self) -> String {
        match self {
            Failure::Exec(e) => classify::error_text(e),
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
            Failure::Unavailable {
                retry_after: Some(wait),
                ..
            } if !wait.is_zero() => Some(ceil_seconds(*wait).max(1)),
            _ => None,
        }
    }

    /// An error the route returns untouched (claude_executor_fast_error.go).
    pub fn direct(&self) -> Option<&ExecError> {
        match self {
            Failure::Exec(e) if e.direct => Some(e),
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
fn route(rt: &Runtime, registry: &Registry, call: &Call) -> Result<(Vec<String>, String), RunError> {
    if let Some(provider) = &call.forced_provider {
        return Ok((vec![provider.clone()], gojson::trim(&call.model).to_owned()));
    }
    let model = call.model.as_str();
    let base = match model.rfind('(') {
        Some(i) if model.ends_with(')') => &model[..i],
        _ => model,
    };
    let resolved = if base == "auto" {
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
    let mut providers = registry.providers(&base);
    if providers.is_empty() && base != resolved {
        providers = registry.providers(&resolved);
    }
    if providers.is_empty() {
        return Err(Failure::UnknownModel(call.model.clone()).into());
    }
    // Go `adjustExecutionProvidersForEntryProtocol`.
    match call.entry {
        Format::Interactions => {
            if let Some(i) = providers.iter().position(|p| p == "gemini-interactions") {
                let p = providers.remove(i);
                providers.insert(0, p);
            }
        }
        Format::OpenAI | Format::OpenAIResponse | Format::Claude | Format::Gemini => {}
        _ => providers.retain(|p| p != "gemini-interactions"),
    }
    Ok((providers, resolved))
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
/// interval until the rendered body follows. The rendered status and headers are lost
/// then, as in Go, where they are written after the first keep-alive flush.
pub async fn serve<F, Fut>(rt: &Arc<Runtime>, call: Call, render: F) -> axum::response::Response
where
    F: FnOnce(Result<Done, Failure>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = axum::response::Response> + Send,
{
    let interval = if call.stream || call.operation != Operation::Generate {
        None
    } else {
        nonstream_keepalive(&rt.config())
    };
    let trace = Arc::new(Trace::default());
    let mut work = Box::pin({
        let (rt, trace) = (rt.clone(), trace.clone());
        async move {
            let result = run_with_bootstrap_retries(&rt, call, &trace).await;
            let mut response = render(result).await;
            if let Some(value) = trace.header() {
                response.headers_mut().insert("x-cpa-trace-id", value);
            }
            response
        }
    });
    let Some(interval) = interval else {
        return work.await;
    };
    tokio::select! {
        biased;
        response = &mut work => return response,
        () = tokio::time::sleep(interval) => {}
    }
    let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    enum Event {
        Tick,
        Done(axum::response::Response),
    }
    let events = futures_util::stream::unfold(Some((work, ticks)), |state| async move {
        let (mut work, mut ticks) = state?;
        tokio::select! {
            biased;
            response = &mut work => Some((Event::Done(response), None)),
            _ = ticks.tick() => Some((Event::Tick, Some((work, ticks)))),
        }
    });
    let newline = || Ok::<_, axum::Error>(Bytes::from_static(b"\n"));
    let body = futures_util::stream::iter([newline()]).chain(events.flat_map(move |event| match event {
        Event::Tick => futures_util::stream::iter([newline()]).left_stream(),
        Event::Done(response) => response.into_body().into_data_stream().right_stream(),
    }));
    let mut response = axum::response::Response::new(axum::body::Body::from_stream(body));
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    // Go cpa_trace.go applies the trace ID when the first keep-alive commits headers.
    if let Some(value) = trace.header() {
        headers.insert("x-cpa-trace-id", value);
    }
    response
}

/// `requests.nonstream-keepalive-interval` seconds (Go `NonStreamingKeepAliveInterval`;
/// 0 or below disables it).
fn nonstream_keepalive(cfg: &Config) -> Option<Duration> {
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
    let max = if call.stream {
        bootstrap_retries(&rt.config())
    } else {
        0
    };
    let mut result = run(rt, call.clone(), trace).await;
    for _ in 0..max {
        let original = match &result {
            Err(RunError {
                failure: Failure::Exec(e),
                bootstrap: true,
            }) if bootstrap_eligible(classify::go_status(e)) => e.clone(),
            _ => break,
        };
        result = match run(rt, call.clone(), trace).await {
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

fn bootstrap_eligible(status: u16) -> bool {
    matches!(status, 0 | 401 | 402 | 403 | 408 | 429) || status >= 500
}

/// The trace ID of the last credential selected for a request.
#[derive(Default)]
pub struct Trace(std::sync::Mutex<Option<String>>, std::sync::OnceLock<String>);

impl Trace {
    /// The request ID (Go `logging.GetRequestID`), created on first use.
    fn request_id(&self) -> String {
        self.1.get_or_init(request_id).clone()
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
        // ponytail: UTC; Go formats the selection time in the process time zone.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let stamp: String = crate::models::rfc3339(now.as_secs() as i64)
            .chars()
            .filter(char::is_ascii_digit)
            .collect();
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
    let (cfg, policy) = rt.request_snapshot();
    let registry = rt.registry();
    let (providers, model) = route(rt, &registry, &call)?;
    let aliases = registry::global_aliases(&cfg);
    let session = crate::session::resolve(
        call.entry,
        &call.headers,
        &call.body,
        call.execution_session.as_deref(),
        &call.caller.principal,
    );
    // Go's usage reporter: one record per Generate attempt while the queue accepts.
    let usage = (call.operation == Operation::Generate && rt.usage_queue().accepts()).then(|| {
        Arc::new(crate::usage_record::Facts::new(
            usage_client(&cfg, &call, trace, &session),
            call.entry,
            call.response,
            &call.model,
            &call.body,
            call.stream,
        ))
    });
    // `WithPinnedAuthID`: every other credential is excluded in every round.
    let pinned_exclusion: Vec<String> = match call.pinned() {
        Some(pinned) if !pinned.is_empty() => rt
            .store()
            .snapshot()
            .iter()
            .filter(|c| c.id != pinned)
            .map(|c| c.id.clone())
            .collect(),
        _ => Vec::new(),
    };
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
        usage: Default::default(),
        request_path: call.request_path.clone(),
        headers: call.headers.clone(),
        caller: call.caller.clone(),
    };
    let compact = call.alt.as_deref() == Some("responses/compact");
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
            };
            match attempt(rt, &cfg, &policy, &call, &request, usage.as_ref(), lease, target).await {
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
) -> Attempt {
    let route_model = lease.selection.model.clone();
    let mut last = None;
    let mut refreshed = false;
    for (i, upstream) in target.models.iter().enumerate() {
        let state = registry::state_model(target.selection_model, &route_model, upstream, target.pooled);
        lease.execution_model = state.clone();
        let mut req = attempt_request(
            request,
            cfg,
            &lease.credential,
            &route_model,
            upstream,
            target.keep_model,
        );
        let start = |credential: &cpa_core::credential::Credential, req: &mut ExecRequest| {
            usage.map(|facts| {
                let tracker = crate::usage_record::Tracker::start(rt, facts, credential, upstream);
                req.usage = tracker.sink();
                tracker
            })
        };
        let mut tracker = start(&lease.credential, &mut req);
        if let Some(on_selected) = call.on_selected() {
            on_selected(&lease.credential);
        }
        let execute = |credential: Arc<cpa_core::credential::Credential>, req: ExecRequest| async move {
            match (call.turn.as_ref(), call.media.as_ref().map(|m| m.kind)) {
                (Some(turn), _) => {
                    rt.executors
                        .execute_in_session(&credential, req, cfg, &turn.session)
                        .await
                }
                (None, Some(MediaKind::Images)) => {
                    rt.executors.images(&credential, req, &call.request_path, cfg).await
                }
                (None, Some(MediaKind::Videos)) => {
                    rt.executors.videos(&credential, req, &call.request_path, cfg).await
                }
                (None, None) => rt.executors.execute(&credential, req, cfg).await,
            }
        };
        let mut executed = execute(lease.credential.clone(), req.clone()).await;
        // Go `tryRefreshAfterUnauthorized`: one refresh-and-retry per credential.
        if let Err(error) = &executed
            && !refreshed
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
                    return Attempt::Done(match done {
                        Done::Stream { headers, first, rest } => Done::Stream {
                            headers,
                            first,
                            rest: Completing::new(rest, lease).boxed(),
                        },
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

/// Go's client request metadata for usage records (handlers.go `GetContextWithCancel`
/// and `syncMetadataSessionToContext`).
// ponytail: `server.trusted-proxies` is read from the request's config snapshot; Go
// applies it at startup only.
fn usage_client(
    cfg: &Config,
    call: &Call,
    trace: &Trace,
    session: &crate::session::Session,
) -> crate::usage_record::Client {
    let trusted = cfg.derived(|c| cpa_core::config::TrustedProxies::new(&c.trusted_proxies));
    let text = |v: &axum::http::HeaderValue| String::from_utf8_lossy(v.as_bytes()).into_owned();
    let forwarded: Vec<String> = call.headers.get_all("x-forwarded-for").iter().map(text).collect();
    crate::usage_record::Client {
        client_ip: call.peer.map(|p| p.ip().to_string()).unwrap_or_default(),
        resolved_client_ip: trusted
            .client_ip(call.peer, |name| call.headers.get(name).map(|v| v.as_bytes()))
            .trim()
            .to_owned(),
        x_forwarded_for: forwarded.join(", ").trim().to_owned(),
        user_agent: call
            .headers
            .get("user-agent")
            .map(text)
            .unwrap_or_default()
            .trim()
            .to_owned(),
        session_id: session.id.clone().unwrap_or_default(),
        parent_session_id: session.parent.clone().unwrap_or_default(),
        is_fork: session.fork,
        request_id: trace.request_id(),
        // Session turns arrive on the WebSocket upgrade (a GET).
        endpoint: format!(
            "{} {}",
            if call.turn.is_some() { "GET" } else { "POST" },
            call.request_path
        ),
        api_key: call.caller.principal.clone(),
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
/// request keeps its own model), with Go's `attachResolvedExecutionModelInfo` binding.
fn attempt_request(
    request: &ExecRequest,
    cfg: &Config,
    credential: &cpa_core::credential::Credential,
    route_model: &str,
    upstream: &str,
    keep_model: bool,
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
        let req = attempt_request(&request, &cfg, &credential, "sol", "gpt-6-sol", false);
        assert_eq!(req.model, "gpt-6-sol");
        let bound = req.resolved_model.unwrap();
        assert_eq!(
            (bound.info.id.as_str(), bound.source, bound.is_compat()),
            ("gpt-6-sol", cpa_core::exec::ResolvedSource::ApiKey, true)
        );
        let kept = attempt_request(&request, &cfg, &credential, "selection", "gpt-6-sol", true);
        assert_eq!(kept.model, "sol");
        let bound = kept.resolved_model.unwrap();
        assert_eq!((bound.info.id.as_str(), bound.is_compat()), ("sol", false));
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
