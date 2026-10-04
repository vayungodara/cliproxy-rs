//! The execution envelope: what the runtime hands an executor and what comes back.
//!
//! Transport-neutral on purpose. Executors own their HTTP clients; nothing here knows
//! about wreq or axum (sdk/cliproxy/executor/types.go).
//!
//! Ownership: the executor owns request translation, provider preparation (cloak,
//! aliases) and response restoration, in that order. Nothing before the executor
//! translates; nothing after it un-aliases.

use std::fmt;
use std::time::Duration;

use bytes::Bytes;
use futures_util::stream::BoxStream;
use http::HeaderMap;

use crate::format::Format;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Generate a response (Messages, Chat Completions, Responses, generateContent).
    Generate,
    CountTokens,
}

/// The authenticated downstream caller. Private request context: it seeds stable
/// per-caller state such as Claude tool aliases and must never be sent upstream.
#[derive(Clone, PartialEq, Eq)]
pub struct Caller {
    /// The configured client key that matched, or empty when client auth is disabled.
    pub principal: String,
    /// Where the key came from: `authorization`, `x-api-key`, `query-key`, ...
    pub source: &'static str,
}

impl fmt::Debug for Caller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Caller")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct ExecRequest {
    pub operation: Operation,
    /// Format of the inbound request.
    pub source_format: Format,
    /// Format the client expects back.
    pub response_format: Format,
    /// Model named by the client, trimmed.
    pub requested_model: String,
    /// Model after alias resolution, sent upstream.
    pub model: String,
    /// Inbound body exactly as received.
    pub original_body: Bytes,
    /// Body in `source_format`, after request-level rewrites (payload rules) but before
    /// any translation. The executor translates it.
    pub body: Bytes,
    /// The client asked for a stream. The upstream mode is the executor's decision.
    pub stream: bool,
    /// Alternate operation from `alt` / `$alt` (for example `responses/compact`).
    pub alt: Option<String>,
    /// Session key for affinity and provider session identity, when one was derived:
    /// Go `ExtractSessionID` (see `cpa_common::session`). Explicit client sessions carry
    /// their family prefix (`claude:`, `codex:`, `header:`, `pck:`, ...); `derived:<id>`
    /// means Go's `derived_session_id` metadata is `<id>`; `msg:` is the first-messages
    /// hash.
    pub session: Option<String>,
    /// Go `execution_session_id` metadata: a long-lived execution session that outlives
    /// one HTTP request. Set only by transports that own such a session (the Responses
    /// WebSocket uses its connection's session ID); `None` for ordinary HTTP routes.
    /// Provider replay caches (Codex and Kimi reasoning replay) key on
    /// `execution:<id>` before falling back to payload or header identity.
    pub execution_session: Option<String>,
    /// Go `derived_session_id` metadata (`ctx:v1:<sha256>`, from
    /// `cpa_common::session::derive_id`): set only when the request carries no explicit
    /// session signal and no execution session, exactly when Go's `session.Enrich` sets
    /// it. Go `helps.ProviderSessionUUID` falls back to it after the execution session.
    pub derived_session: Option<String>,
    /// Go `request_path` metadata (gin `FullPath()`): the matched route template, for
    /// example `/v1/chat/completions` or `/v1beta/models/*action`; the URI path when no
    /// route matched; empty for internal executions. Pass it to
    /// `cpa_common::payload::Request::request_path` (the `disable-image-generation: chat`
    /// gate keeps image generation on `/v1/images/*`).
    pub request_path: String,
    /// Inbound headers. Executors forward only what their provider profile allows.
    /// Contains client credentials: never log or forward wholesale.
    pub headers: HeaderMap,
    pub caller: Caller,
    /// Go `cliproxyauth.ResolvedModelInfo(req)`: the model capabilities the dispatch
    /// loop bound to this attempt for the selected credential and upstream model (Go
    /// `attachResolvedExecutionModelInfo`, sdk/cliproxy/auth/api_key_model_capabilities.go).
    /// `None` when Go binds nothing; executors then fall back to the registry lookup
    /// (`cpa_core::registry::lookup_model`), as Go's helpers do. Callers outside the
    /// dispatch loop set `None`.
    pub resolved_model: Option<ResolvedModel>,
    /// Usage accounting for this attempt (Go's per-executor `UsageReporter`). Optional:
    /// without reports the server parses the client-format response, which matches Go
    /// whenever the upstream speaks the client's format. Executors that translate
    /// should forward the upstream payloads Go parses; `UsageSink::default()` is a
    /// no-op.
    pub usage: UsageSink,
}

/// Receives the upstream payloads Go's executors feed their usage reporter.
pub trait UsageObserver: Send + Sync {
    /// A whole upstream response body in `format` (Go `Parse*Usage` on the body).
    fn response_body(&self, format: Format, body: &[u8]);
    /// One upstream stream line in `format`, as received (Go feeds each scanner line to
    /// its `StreamUsageBuffer` and `ObserveResponseModel`).
    fn response_line(&self, format: Format, line: &[u8]);
    /// The translated payload sent upstream in `format` (Go
    /// `SetTranslatedReasoningEffort`: the record's `reasoning_effort`).
    fn request(&self, format: Format, payload: &[u8]);
    /// [`UsageObserver::request`] where Go passes an executor identifier instead of a
    /// format (Kimi passes `kimi`).
    fn request_for(&self, identifier: &str, payload: &[u8]) {
        let _ = (identifier, payload);
    }
    /// Go `SetUpstreamModel`: the model the upstream is expected to serve, for the
    /// substitution warning only.
    fn upstream_model(&self, model: &str) {
        let _ = model;
    }
    /// Go `SetResponseModel`: the served model, unless a terminal event fixed it.
    fn response_model(&self, model: &str) {
        let _ = model;
    }
    /// Go `StartResponseTTFT`: the upstream request is being sent.
    fn round_trip_started(&self) {}
    /// Go `MarkFirstResponseByte`: the first upstream body byte arrived.
    fn first_byte(&self) {}
    /// Go `ObserveTokenEvent`: an upstream frame arrived; `is_token` when it carried
    /// output (Codex SSE marks TTFT at the first token, the first frame as fallback).
    fn token_event(&self, is_token: bool) {
        let _ = is_token;
    }
    /// Go `Publish`/`EnsurePublished` now, with the usage reported so far. The first
    /// publish of an attempt wins (Go `once.Do`); the server's later outcome is ignored.
    fn publish(&self) {}
    /// Go `PublishFailure` now with `status` (0 when the error carries none) and
    /// `body`; no usage. The first publish of an attempt wins.
    fn publish_failure(&self, status: u16, body: &str) {
        let _ = (status, body);
    }
    /// The Go path has no `EnsurePublished`: an attempt that ends without reported
    /// usage (a reported body, or a stream line carrying usage) publishes nothing
    /// unless it fails.
    fn usage_required(&self) {}
    /// Go returned before creating its usage reporter (Claude's `responses/compact`
    /// 501): this attempt publishes no record at all, whatever its outcome.
    fn discard(&self) {}
}

/// A handle executors report usage through; cloning shares the observer.
#[derive(Clone, Default)]
pub struct UsageSink(Option<std::sync::Arc<dyn UsageObserver>>, CaptureSink);

impl UsageSink {
    pub fn new(observer: std::sync::Arc<dyn UsageObserver>) -> Self {
        Self(Some(observer), CaptureSink::default())
    }

    /// Attach request capture without changing ExecRequest construction sites.
    pub fn with_capture(mut self, capture: CaptureSink) -> Self {
        self.1 = capture;
        self
    }

    /// Whether anyone listens; executors can skip the work otherwise.
    pub fn enabled(&self) -> bool {
        self.0.is_some()
    }

    pub fn response_body(&self, format: Format, body: &[u8]) {
        if let Some(o) = &self.0 {
            o.response_body(format, body);
        }
    }

    pub fn response_line(&self, format: Format, line: &[u8]) {
        if let Some(o) = &self.0 {
            o.response_line(format, line);
        }
    }

    pub fn request(&self, format: Format, payload: &[u8]) {
        if let Some(o) = &self.0 {
            o.request(format, payload);
        }
    }

    pub fn request_for(&self, identifier: &str, payload: &[u8]) {
        if let Some(o) = &self.0 {
            o.request_for(identifier, payload);
        }
    }

    pub fn upstream_model(&self, model: &str) {
        if let Some(o) = &self.0 {
            o.upstream_model(model);
        }
    }

    pub fn response_model(&self, model: &str) {
        if let Some(o) = &self.0 {
            o.response_model(model);
        }
    }

    pub fn round_trip_started(&self) {
        if let Some(o) = &self.0 {
            o.round_trip_started();
        }
    }

    pub fn first_byte(&self) {
        if let Some(o) = &self.0 {
            o.first_byte();
        }
    }

    pub fn token_event(&self, is_token: bool) {
        if let Some(o) = &self.0 {
            o.token_event(is_token);
        }
    }

    pub fn publish(&self) {
        if let Some(o) = &self.0 {
            o.publish();
        }
    }

    pub fn publish_failure(&self, status: u16, body: &str) {
        if let Some(o) = &self.0 {
            o.publish_failure(status, body);
        }
    }

    pub fn usage_required(&self) {
        if let Some(o) = &self.0 {
            o.usage_required();
        }
    }

    /// See [`UsageObserver::discard`].
    pub fn discard(&self) {
        if let Some(o) = &self.0 {
            o.discard();
        }
    }
}

impl fmt::Debug for UsageSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("UsageSink").field(&self.enabled()).finish()
    }
}

impl ExecRequest {
    /// Optional per-call wire capture, independent of usage accounting. Capture
    /// the handle before spawning a stream task; clones retain the same observer.
    pub fn capture(&self) -> &CaptureSink {
        &self.usage.1
    }
}

/// Go helps.UpstreamRequestLog. Credentials and bodies intentionally have no
/// Debug implementation. The observer owns Go's masking and formatting policy.
#[derive(Default)]
pub struct UpstreamRequest<'a> {
    pub url: &'a str,
    pub method: &'a str,
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
    pub provider: &'a str,
    pub auth_id: &'a str,
    pub auth_label: &'a str,
    pub auth_type: &'a str,
    pub auth_value: &'a str,
}

pub enum CaptureEvent<'a> {
    Request(UpstreamRequest<'a>),
    ResponseMetadata(u16, &'a [(String, String)]),
    ResponseError(&'a str),
    ResponseChunk(&'a [u8]),
    WebsocketRequest(UpstreamRequest<'a>),
    WebsocketHandshake(u16, &'a [(String, String)]),
    WebsocketResponse(&'a [u8]),
    WebsocketError { stage: &'a str, error: &'a str },
}

pub trait CaptureObserver: Send + Sync {
    fn record(&self, event: CaptureEvent<'_>);
    /// Whether response events are kept right now (Go's `requestLogCaptureEnabled`).
    /// An observer that drops them while logging is off says so, so executors need not
    /// buffer data that exists only to be logged.
    fn logs_responses(&self) -> bool {
        true
    }
}

/// Default no-op; providers call this at Go's logging_helpers call sites.
#[derive(Clone, Default)]
pub struct CaptureSink(Option<std::sync::Arc<dyn CaptureObserver>>);

impl CaptureSink {
    pub fn new(observer: std::sync::Arc<dyn CaptureObserver>) -> Self {
        Self(Some(observer))
    }
    pub fn enabled(&self) -> bool {
        self.0.is_some()
    }
    /// Whether response chunks reach a log now: an observer is attached and keeps them.
    pub fn logs_responses(&self) -> bool {
        self.0.as_ref().is_some_and(|observer| observer.logs_responses())
    }
    pub fn record(&self, event: CaptureEvent<'_>) {
        if let Some(observer) = &self.0 {
            observer.record(event);
        }
    }
}

/// Capabilities bound to one execution attempt (Go `*registry.ModelInfo` stored under a
/// request metadata key).
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    /// Go's `modelconfig.ResolveModelInfo` snapshot for configured API-key models (the
    /// static definition of the suffix-free upstream name, renamed to it, typed for the
    /// provider, configured thinking normalized, never user-defined), or the static
    /// Codex plan catalog entry for Codex OAuth. `info.raw` carries `is_compat` and
    /// `support_configuration_update` when set; `info.is_compat()` is Go's
    /// `helps.APIKeyModelIsCompat`.
    pub info: crate::registry::ModelInfo,
    pub source: ResolvedSource,
}

impl ResolvedModel {
    /// Go `ModelInfo.IsCompat` of the bound model (`helps.APIKeyModelIsCompat`).
    pub fn is_compat(&self) -> bool {
        self.info.is_compat()
    }
}

/// Which request metadata key Go stores the binding under. Go's `ResolvedModelInfo`
/// reads either; `ResolvedAPIKeyModelInfo` reads only [`ResolvedSource::ApiKey`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedSource {
    /// `cliproxy.resolved_api_key_model_info`: a configured API-key model (any
    /// family, OpenAI compatibility included) or an unlisted Codex API-key model.
    ApiKey,
    /// `cliproxy.resolved_codex_oauth_model_info`: a Codex OAuth credential's plan
    /// catalog model.
    CodexOAuth,
    /// `cliproxy.resolved_home_model_info`: Home's definition of the model it
    /// dispatched (Go `attachResolvedHomeModelInfo`); Go's `ResolvedModelInfo` reads it
    /// before the other two.
    Home,
}

impl fmt::Debug for ExecRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecRequest")
            .field("operation", &self.operation)
            .field("source_format", &self.source_format)
            .field("response_format", &self.response_format)
            .field("model", &self.model)
            .field("stream", &self.stream)
            .field("alt", &self.alt)
            .field("execution_session", &self.execution_session)
            .field("derived_session", &self.derived_session)
            .field("request_path", &self.request_path)
            .field(
                "resolved_model",
                &self.resolved_model.as_ref().map(|r| (&r.info.id, r.source)),
            )
            .field("body_len", &self.body.len())
            .finish_non_exhaustive()
    }
}

/// One framed unit of a streaming response (for SSE: one complete event).
/// An `Err` item is terminal: consumers stop polling after it.
pub type ExecStream = BoxStream<'static, Result<Bytes, ExecError>>;

pub enum ResponseBody {
    Buffered(Bytes),
    Stream(ExecStream),
}

pub struct ExecResponse {
    pub status: u16,
    /// Upstream headers. The server decides which, if any, reach the client.
    pub headers: HeaderMap,
    pub body: ResponseBody,
}

/// What a failure says about the attempt, for retry and cooldown decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureScope {
    /// The request itself is bad; retrying with another credential will not help.
    Request,
    /// This credential cannot serve this model right now.
    Model,
    /// This credential is unusable right now (auth, quota, upstream fault).
    Credential,
    /// Connection/transport lifecycle fault: may fail over, must not cool credentials.
    Transport,
}

#[derive(Debug, Clone)]
pub struct ExecError {
    pub status: u16,
    pub scope: FailureScope,
    /// Upstream error body (bounded), or a message for locally generated errors.
    pub body: Bytes,
    /// Upstream response headers, for the scheduler and passthrough policy. The server
    /// never emits these by default. Boxed: errors are the cold path.
    pub headers: Box<HeaderMap>,
    /// Scheduler hint parsed from upstream. Not permission to send `Retry-After` downstream.
    pub retry_after: Option<Duration>,
    /// Send status, body and filtered headers to the client unchanged instead of the
    /// route's normal error shape (claude_executor_fast_error.go).
    pub direct: bool,
}

impl ExecError {
    pub fn local(status: u16, scope: FailureScope, message: impl Into<String>) -> Self {
        Self {
            status,
            scope,
            body: Bytes::from(message.into()),
            headers: Box::default(),
            retry_after: None,
            direct: false,
        }
    }
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({:?}): {}",
            self.status,
            self.scope,
            String::from_utf8_lossy(&self.body)
        )
    }
}

impl std::error::Error for ExecError {}

/// A long-lived downstream connection (the Responses WebSocket, `GET /v1/responses`)
/// whose turns can share upstream state. It travels next to an [`ExecRequest`] in
/// `cpa_exec::Executors::execute_in_session`. Executors that pool upstream sockets key
/// them by `id` and release them in `Executors::close_session` when the downstream
/// connection ends (Go: execution session id and `CloseExecutionSession`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecSession {
    /// Stable for the whole downstream connection.
    pub id: String,
    /// This turn continues upstream state (`previous_response_id` or `response.append`
    /// passed through unchanged), so it must run on the session's current upstream socket.
    /// An executor that cannot honour that fails with [`ExecError::replay_required`]
    /// instead of opening a fresh connection (Go: `RequiredUpstreamWebsocket`).
    pub continuation: bool,
    /// The Home pick this attempt runs on, when it came from Home (Go
    /// `Options.ExecutionLifecycle`). An executor that pools the upstream socket hands
    /// the pick to it; the pick then lasts as long as the socket.
    pub lease: Option<SessionLease>,
}

/// A Home pick a pooled upstream socket can keep beyond the turn (Go
/// `ExecutionLifecycle` with `Retain`).
pub trait Retainable: Send + Sync {
    /// Go `Bind` + `Retain`: the socket takes the pick over. Home draining the pick runs
    /// `close` once, before the pick is released. False when the pick already ended; the
    /// socket must not be kept then.
    fn retain(&self, close: Box<dyn FnOnce() + Send>) -> bool;
    /// Go `End`: the socket that kept the pick was invalidated, replaced or closed.
    fn end(&self);
}

/// A shared [`Retainable`], compared by identity.
#[derive(Clone)]
pub struct SessionLease(pub std::sync::Arc<dyn Retainable>);

impl SessionLease {
    pub fn same(&self, other: &SessionLease) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

impl PartialEq for SessionLease {
    fn eq(&self, other: &Self) -> bool {
        self.same(other)
    }
}

impl Eq for SessionLease {}

impl fmt::Debug for SessionLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionLease")
    }
}

impl ExecError {
    /// Body of the replay signal (Go `UpstreamWebsocketReplayRequiredError`).
    pub const REPLAY_REQUIRED: &'static str = r#"{"error":{"message":"upstream transport requires full HTTP replay","type":"server_error","code":"upstream_http_replay_required","status":426}}"#;

    /// A continuation turn lost its upstream socket: the client must resend the full
    /// conversation. Request-scoped: no credential is at fault.
    pub fn replay_required() -> Self {
        Self::local(426, FailureScope::Request, Self::REPLAY_REQUIRED)
    }

    pub fn is_replay_required(&self) -> bool {
        self.status == 426
            && self.scope == FailureScope::Request
            && self.body.as_ref() == Self::REPLAY_REQUIRED.as_bytes()
    }
}
