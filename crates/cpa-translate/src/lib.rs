//! Translation between wire formats (internal/translator, sdk/translator).
//!
//! Executors call [`pair`] to find the transforms for `(client format, upstream format)`.
//! When no pair is registered and the formats are equal, traffic passes through
//! untouched. Same-format pairs may still be registered for normalization (Go registers
//! OpenAI -> OpenAI).
//!
//! Contract for every pair:
//! - `request` turns a client body into an upstream body.
//! - `non_stream` turns one buffered upstream body into one client body. The input may
//!   be buffered SSE when the executor streamed upstream for a non-streaming client
//!   (claude_executor_execute.go asks Claude for SSE whenever formats differ).
//! - `stream` builds request-local state that turns framed upstream events (see
//!   [`sse::Framer`]) into zero or more client events, then flushes on `finish`.
//! - `count_tokens` turns an upstream token-count body into the client's shape.
//! - Response transforms run after the executor has reversed provider rewrites (tool
//!   aliases, cloak) and see the client's original request and the translated request
//!   from before those rewrites.

mod claude_chat_request;
mod claude_chat_response;
mod json;
pub mod gj;
mod openai;
pub mod sse;

pub use claude_chat_request::request_with_compat as openai_to_claude_with_compat;

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

/// Request-scoped inputs available to request transforms.
pub struct RequestCtx<'a> {
    pub model: &'a str,
    pub stream: bool,
}

/// Request-scoped inputs available to response transforms.
pub struct ResponseCtx<'a> {
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

pub type RequestFn = fn(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error>;
pub type NonStreamFn = fn(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error>;
pub type StreamFn = fn(ctx: &ResponseCtx<'_>) -> Box<dyn StreamTranslator>;
pub type CountTokensFn = fn(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error>;

pub struct Pair {
    pub request: RequestFn,
    pub non_stream: NonStreamFn,
    pub stream: StreamFn,
    pub count_tokens: Option<CountTokensFn>,
}

/// Transforms for a client format talking to an upstream format, or `None` when the
/// pair is not registered.
pub fn pair(client: Format, upstream: Format) -> Option<&'static Pair> {
    // ponytail: static match. Plugin-registered translators (M6) need a runtime table.
    match (client, upstream) {
        (Format::OpenAI, Format::OpenAI) => Some(&openai::PAIR),
        (Format::OpenAI, Format::Claude) => Some(&claude_chat_request::PAIR),
        _ => None,
    }
}
