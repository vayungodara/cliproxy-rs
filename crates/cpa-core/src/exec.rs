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
    /// Inbound headers. Executors forward only what their provider profile allows.
    /// Contains client credentials: never log or forward wholesale.
    pub headers: HeaderMap,
    pub caller: Caller,
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
