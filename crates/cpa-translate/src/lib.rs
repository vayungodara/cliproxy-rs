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

/// One Go registration (`translator.Register`). Request and non-stream transforms run
/// deeply nested bodies on a Go-sized stack ([`deep_stack`]); stream events get the same
/// protection in [`stream::framed`].
macro_rules! registered {
    ($client:ident -> $upstream:ident, request: $request:expr, non_stream: $non_stream:expr, go_stream: $go_stream:expr, token_count: $token_count:expr $(,)?) => {
        $crate::Registered {
            pair: $crate::Pair {
                request: |ctx, body| {
                    let request: $crate::RequestFn = $request;
                    $crate::deep_stack(body, || request(ctx, body))
                },
                non_stream: |ctx, body| {
                    let non_stream: $crate::NonStreamFn = $non_stream;
                    $crate::deep_stack(body, || non_stream(ctx, body))
                },
                stream: |ctx| {
                    $crate::stream::framed(
                        cpa_core::format::Format::$client,
                        cpa_core::format::Format::$upstream,
                        ($go_stream)(ctx),
                    )
                },
                count_tokens: None,
            },
            token_count: $token_count,
            go_stream: $go_stream,
        }
    };
}

mod apply_patch;
mod claude_chat_request;
mod claude_chat_response;
mod claude_responses;
mod claude_responses_response;
mod codex_chat_request;
mod codex_chat_response;
mod codex_claude;
mod codex_claude_response;
mod codex_gemini;
mod codex_interactions;
mod codex_responses;
mod common;
mod gemini;
mod gemini_chat_request;
mod gemini_chat_response;
mod gemini_claude;
mod gemini_claude_response;
mod gemini_responses;
mod gemini_responses_response;
mod gemini_web_search;
mod mime;
mod openai;
mod openai_claude;
mod openai_claude_response;
mod openai_responses;
mod openai_responses_response;
mod replay_cache;
mod responses_tools;
pub mod sse;
pub mod stream;
mod thinking;

pub use claude_chat_request::request_with_compat as openai_to_claude_with_compat;
pub use codex_claude::request_with_compat as claude_to_codex_with_compat;
pub use gemini_claude::request_with_compat as claude_to_gemini_with_compat;
pub use openai_claude::request_with_compat as claude_to_openai_with_compat;

/// ConvertOpenAIResponsesRequestToClaudeWithCompat: like the registered Responses ->
/// Claude request, but unsigned reasoning history is kept for compatibility endpoints.
pub fn responses_to_claude_with_compat(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    Ok(claude_responses::convert(ctx.model, body, ctx.stream, true))
}

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
    /// Before a terminal error is written to a Responses client: the client-side frame
    /// still being joined, if it is complete enough to send (Go's responsesSSEFramer.Flush).
    /// Other clients have nothing pending.
    fn flush_frames(&mut self) -> Vec<Bytes> {
        vec![]
    }
    /// Go's apply_patch tool-input contract (`ToolInputError`): the upstream sent an
    /// invalid or conflicting `apply_patch` call. Check after every `event`: write that
    /// event's frames (they end in `response.failed`), then end the stream with HTTP 502
    /// and [`APPLY_PATCH_UPSTREAM_ERROR`], as helps.StopApplyPatchStream does.
    fn tool_input_failed(&self) -> bool {
        false
    }
    /// Go's `FinalizeToolInput` (helps.EndApplyPatchStream): call when the upstream
    /// transport ends, before any synthetic terminator (`[DONE]`) or `finish`. A stream that
    /// declared `apply_patch` and ended without its terminator yields `response.failed`;
    /// then check [`Self::tool_input_failed`].
    fn finalize_tool_input(&mut self) -> Vec<Bytes> {
        vec![]
    }
}

/// helps.ApplyPatchUpstreamErrorMessage: the 502 message executors return when a
/// translator rejects upstream apply_patch input (a `non_stream` error carries it too).
pub const APPLY_PATCH_UPSTREAM_ERROR: &str = apply_patch::UPSTREAM_ERROR_MESSAGE;

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
        (Format::OpenAIResponse, Format::Claude) => Some(&claude_responses_response::PAIR),
        (Format::Claude, Format::OpenAI) => Some(&openai_claude::PAIR),
        (Format::Gemini, Format::Gemini) => Some(&gemini::PAIR),
        (Format::OpenAI, Format::Gemini) => Some(&gemini_chat_request::PAIR),
        (Format::Claude, Format::Gemini) => Some(&gemini_claude::PAIR),
        (Format::OpenAI, Format::Codex) => Some(&codex_chat_request::PAIR),
        (Format::OpenAIResponse, Format::Gemini) => Some(&gemini_responses_response::PAIR),
        (Format::Claude, Format::Codex) => Some(&codex_claude::PAIR),
        (Format::Gemini, Format::Codex) => Some(&codex_gemini::PAIR),
        (Format::Interactions, Format::Codex) => Some(&codex_interactions::PAIR),
        (Format::OpenAIResponse, Format::OpenAI) => Some(&openai_responses_response::PAIR),
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
        return deep_stack(body, || {
            let summary = thinking::extract_translated_summary(body, client.as_str(), upstream.as_str());
            let out = (pair.request)(ctx, body)?;
            Ok(thinking::apply_summary_for_model(
                out,
                upstream.as_str(),
                ctx.model,
                summary,
            ))
        });
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

/// Nesting depth (arrays and objects) of a JSON body, ignoring brackets inside strings.
fn nesting_depth(body: &[u8]) -> usize {
    let (mut depth, mut max, mut in_string, mut escaped) = (0usize, 0usize, false, false);
    for &c in body {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                max = max.max(depth);
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max
}

/// Runs `f` on a thread with Go's maximum goroutine stack (1 GiB, reserved lazily) when
/// the body nests deeply. Go's JSON walkers recurse per nesting level and rely on
/// growable stacks; the ported walkers do too, so very deep client bodies would otherwise
/// overflow a native thread stack instead of translating as in Go.
pub(crate) fn deep_stack<T: Send>(body: &[u8], f: impl FnOnce() -> T + Send) -> T {
    const DEEP: usize = 256;
    thread_local! {
        static ON_DEEP_STACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if ON_DEEP_STACK.with(std::cell::Cell::get) || nesting_depth(body) <= DEEP {
        return f();
    }
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(1 << 30)
            .spawn_scoped(scope, || {
                ON_DEEP_STACK.with(|flag| flag.set(true));
                f()
            })
            .map(|handle| handle.join())
    })
    .ok()
    .and_then(Result::ok)
    .expect("deep JSON translation thread")
}
