//! Translation between wire formats (internal/translator, sdk/translator).
//!
//! Executors call [`pair`] to find the transforms for `(client format, upstream format)`.
//! Same-format traffic needs no pair and is passed through untouched.
//!
//! Contract for every pair:
//! - `request` turns a client body into an upstream body.
//! - `non_stream` turns one buffered upstream body into one client body.
//! - `stream` builds request-local state that turns framed upstream events (see
//!   [`sse::Framer`]) into zero or more client events, then flushes on `finish`.
//! - Transforms see the client's original request and the translated request before any
//!   provider-specific rewriting (tool aliases, cloak), because executors reverse those
//!   first (internal/runtime/executor/claude_executor_execute.go).

pub mod sse;

use bytes::Bytes;
use cpa_core::format::Format;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// Request-scoped inputs available to response transforms.
pub struct Ctx<'a> {
    pub model: &'a str,
    pub original_request: &'a [u8],
    pub translated_request: &'a [u8],
}

pub trait StreamTranslator: Send {
    /// One complete upstream event in, zero or more complete client events out.
    fn event(&mut self, event: &[u8]) -> Result<Vec<Bytes>, Error>;
    /// Upstream ended cleanly; emit any closing events.
    fn finish(&mut self) -> Result<Vec<Bytes>, Error>;
}

pub type RequestFn = fn(model: &str, body: &[u8], stream: bool) -> Result<Vec<u8>, Error>;
pub type NonStreamFn = fn(ctx: &Ctx<'_>, body: &[u8]) -> Result<Vec<u8>, Error>;
pub type StreamFn = fn(ctx: &Ctx<'_>) -> Box<dyn StreamTranslator>;

pub struct Pair {
    pub request: RequestFn,
    pub non_stream: NonStreamFn,
    pub stream: StreamFn,
}

/// Transforms for a client format talking to an upstream format, or `None` when the
/// pair is not supported. Never called for `from == to`.
pub fn pair(client: Format, upstream: Format) -> Option<&'static Pair> {
    // ponytail: static match. Plugin-registered translators (M6) need a runtime table.
    #[allow(clippy::match_single_binding)] // pairs are added here as match arms
    match (client, upstream) {
        _ => None,
    }
}
