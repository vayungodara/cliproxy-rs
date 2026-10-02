//! The execution envelope: what the runtime hands an executor and what comes back.
//!
//! Transport-neutral on purpose. Executors own their HTTP clients; nothing here knows
//! about wreq or axum (sdk/cliproxy/executor/types.go).

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
    /// The matched client key, or empty when client auth is disabled.
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

#[derive(Debug, Clone)]
pub struct ExecRequest {
    pub operation: Operation,
    /// Format of the inbound request.
    pub source_format: Format,
    /// Format the client expects back.
    pub response_format: Format,
    /// Model named by the client.
    pub requested_model: String,
    /// Model after alias resolution, sent upstream.
    pub model: String,
    /// Inbound body before any translation.
    pub original_body: Bytes,
    /// Body to send, already translated when formats differ.
    pub body: Bytes,
    /// The client asked for a stream. The upstream mode is the executor's decision.
    pub stream: bool,
    /// Inbound headers. Executors forward only what their provider profile allows.
    pub headers: HeaderMap,
    pub caller: Caller,
}

/// One framed unit of a streaming response (for SSE: one complete event, bytes intact).
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
}

#[derive(Debug, Clone)]
pub struct ExecError {
    pub status: u16,
    pub scope: FailureScope,
    /// Upstream error body, or a message for locally generated errors.
    pub body: Bytes,
    pub retry_after: Option<Duration>,
}

impl ExecError {
    pub fn local(status: u16, scope: FailureScope, message: impl Into<String>) -> Self {
        Self {
            status,
            scope,
            body: Bytes::from(message.into()),
            retry_after: None,
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
