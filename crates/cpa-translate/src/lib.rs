//! Translation between wire formats (internal/translator, sdk/translator).
//!
//! Executors call [`translate_request`] (sdk/translator's TranslateRequest: the registered
//! pair wrapped in the summary pipeline, or the model-rewrite fallback) and [`pair`] for
//! the response transforms of `(client format, upstream format)`. Same-format pairs may
//! be registered for normalization (Go registers OpenAI -> OpenAI).
//!
//! Contract for every pair:
//! - `request` turns a client body into an upstream body (the bare Go converter; use
//!   [`translate_request`] for the registry wrapper).
//! - `non_stream` turns one buffered upstream body into one client body. The input may
//!   be buffered SSE when the executor streamed upstream for a non-streaming client
//!   (claude_executor_execute.go asks Claude for SSE whenever formats differ).
//! - `stream` builds request-local state that turns framed upstream events (see
//!   [`sse::Framer`]) into zero or more client events, then flushes on `finish`. Output
//!   bytes are framed the way the client's Go route handler writes them ([`stream::frame`]).
//! - [`token_count`] renders an upstream token count in the client's shape (Go's
//!   TokenCount); `count_tokens` is the older body-based form and stays unset.
//! - Response transforms run after the executor has reversed provider rewrites (tool
//!   aliases, cloak) and see the client's original request and the translated request
//!   from before those rewrites.
//!
//! JSON is read and edited with `cpa_common::json`, a byte-exact port of the gjson/sjson
//! behaviour Go's translators depend on, so malformed bytes, coercions and escaping
//! match Go.

mod claude_chat_request;
mod apply_patch;
mod claude_chat_response;
mod claude_responses;
mod codex_responses;
mod common;
mod openai;
pub mod sse;
pub mod stream;
mod thinking;

pub use claude_chat_request::request_with_compat as openai_to_claude_with_compat;

use bytes::Bytes;
use cpa_common::json as gj;
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
/// Go's `TokenCount(ctx, count)`: an input-token count in the client's response shape.
pub type TokenCountFn = fn(count: i64) -> Vec<u8>;
#[doc(hidden)]
pub type GoStreamFn = fn(ctx: &ResponseCtx<'_>) -> Box<dyn stream::GoStream>;

pub struct Pair {
    pub request: RequestFn,
    pub non_stream: NonStreamFn,
    pub stream: StreamFn,
    pub count_tokens: Option<CountTokensFn>,
}

/// One Go registration: the public transforms plus what [`token_count`] and
/// [`go_stream`] expose.
pub(crate) struct Registered {
    pub pair: Pair,
    pub token_count: Option<TokenCountFn>,
    pub go_stream: GoStreamFn,
}

fn registered(client: Format, upstream: Format) -> Option<&'static Registered> {
    // ponytail: static match. Plugin-registered translators (M6) need a runtime table.
    match (client, upstream) {
        (Format::OpenAI, Format::OpenAI) => Some(&openai::PAIR),
        (Format::OpenAI, Format::Claude) => Some(&claude_chat_request::PAIR),
        (Format::OpenAIResponse, Format::Codex) => Some(&codex_responses::PAIR),
        _ => None,
    }
}

/// Transforms for a client format talking to an upstream format, or `None` when the
/// pair is not registered.
pub fn pair(client: Format, upstream: Format) -> Option<&'static Pair> {
    registered(client, upstream).map(|r| &r.pair)
}

/// Go's TokenCount for the pair: renders an upstream input-token count in the client's
/// shape. `None` when Go registers none (the upstream body is then returned as is).
pub fn token_count(client: Format, upstream: Format) -> Option<TokenCountFn> {
    registered(client, upstream).and_then(|r| r.token_count)
}

/// The Go-shaped line translator behind a pair's `stream`, for golden tests.
#[doc(hidden)]
pub fn go_stream(client: Format, upstream: Format) -> Option<GoStreamFn> {
    registered(client, upstream).map(|r| r.go_stream)
}

/// sdk/translator TranslateRequest. A registered pair runs between summary extraction
/// (from the client body) and summary application (to the upstream body); without one,
/// only a differing top-level `model` is rewritten.
// ponytail: plugin NormalizeRequest/TranslateRequest hooks (M6) are not ported.
pub fn translate_request(
    client: Format,
    upstream: Format,
    ctx: &RequestCtx<'_>,
    body: &[u8],
) -> Result<Vec<u8>, Error> {
    if let Some(pair) = pair(client, upstream) {
        let summary = thinking::extract_translated_summary(body, client.as_str(), upstream.as_str());
        let out = (pair.request)(ctx, body)?;
        return Ok(thinking::apply_summary_for_model(
            out,
            upstream.as_str(),
            ctx.model,
            summary,
        ));
    }
    let mut out = body.to_vec();
    if !ctx.model.is_empty() && *gj::get(body, "model").bytes() != *ctx.model.as_bytes() {
        gj::set_str(&mut out, "model", ctx.model);
    }
    Ok(out)
}

/// sdk/translator TranslateTokenCount: the pair's token-count shape, or the upstream body
/// unchanged when the pair registers none.
pub fn translate_token_count(client: Format, upstream: Format, count: i64, body: &[u8]) -> Vec<u8> {
    match token_count(client, upstream) {
        Some(render) => render(count),
        None => body.to_vec(),
    }
}
