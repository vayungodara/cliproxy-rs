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
//! - Plugin translator hooks ([`hooks`]) run inside these entry points and inside every
//!   pair's `non_stream` and `stream`, in Go's registry order; with none active, the
//!   bytes are those of the native path.
//! - Response transforms run after the executor has reversed provider rewrites (tool
//!   aliases, cloak) and see the client's original request and the translated request
//!   from before those rewrites.
//!
//! JSON is read and edited with `cpa_common::json`, a byte-exact port of the gjson/sjson
//! behaviour Go's translators depend on, so malformed bytes, coercions and escaping
//! match Go.

/// One Go registration (`translator.Register`). Request and non-stream transforms run
/// deeply nested inputs (the body and, for responses, both requests the translator reads)
/// on a stack sized for them ([`deep_stack`]); streams get the same protection in
/// [`stream::framed`], over everything they have read.
macro_rules! registered {
    ($client:ident -> $upstream:ident, request: $request:expr, non_stream: $non_stream:expr, go_stream: $go_stream:expr, token_count: $token_count:expr $(,)?) => {
        $crate::Registered {
            pair: $crate::Pair {
                request: |ctx, body| {
                    let request: $crate::RequestFn = $request;
                    $crate::deep_stack($crate::levels(&[body]), || request(ctx, body))
                },
                non_stream: |ctx, body| {
                    let non_stream: $crate::NonStreamFn = $non_stream;
                    $crate::hooks::non_stream(
                        cpa_core::format::Format::$upstream.as_str(),
                        cpa_core::format::Format::$client.as_str(),
                        ctx,
                        body,
                        |body| {
                            let levels = $crate::levels(&[body, ctx.original_request, ctx.translated_request]);
                            $crate::deep_stack(levels, || non_stream(ctx, body))
                        },
                    )
                },
                stream: |ctx| {
                    $crate::stream::framed(
                        cpa_core::format::Format::$client,
                        cpa_core::format::Format::$upstream,
                        $crate::hooks::stream(
                            cpa_core::format::Format::$upstream.as_str(),
                            cpa_core::format::Format::$client.as_str(),
                            ctx,
                            ($go_stream)(ctx),
                        ),
                        $crate::levels(&[ctx.original_request, ctx.translated_request]),
                    )
                },
                count_tokens: None,
            },
            token_count: $token_count,
            go_stream: $go_stream,
        }
    };
}

mod antigravity_chat;
mod antigravity_claude;
mod antigravity_claude_response;
mod antigravity_gemini;
mod antigravity_interactions;
mod antigravity_responses;
mod apply_patch;
pub mod apply_patch_responses;
mod claude_chat_request;
mod claude_chat_response;
mod claude_gemini;
mod claude_interactions;
mod claude_responses;
mod claude_responses_response;
mod claude_tool_args;
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
mod gemini_interactions;
mod gemini_interactions_response;
mod gemini_responses;
mod gemini_responses_response;
mod gemini_web_search;
pub mod hooks;
mod interactions_claude;
mod mime;
mod openai;
mod openai_claude;
mod openai_claude_response;
mod openai_gemini;
mod openai_interactions;
mod openai_interactions_response;
mod openai_responses;
mod openai_responses_response;
mod replay_cache;
mod responses_interactions;
mod responses_interactions_response;
mod responses_tools;
pub mod sse;
pub mod stream;
mod thinking;

#[doc(hidden)]
pub use codex_responses::go_stream_with_bridge as codex_responses_go_stream_with_bridge;
pub use codex_responses::{
    non_stream_with_bridge as codex_responses_non_stream_with_bridge,
    stream_with_bridge as codex_responses_stream_with_bridge,
};
/// translatorcommon.UserRun, for executors that refuse an emptied user turn (Devin).
pub use common::UserRun;
pub use replay_cache::set_signature_cache_config as set_antigravity_signature_cache_config;

/// util.GeminiClaudeToolUseID: the Claude-facing provenance ID of a native Gemini call,
/// which the Antigravity executor's replay ledger resolves back.
pub fn gemini_claude_tool_use_id(call_id: &[u8], name: &[u8], args: &[u8]) -> Vec<u8> {
    antigravity_claude_response::gemini_claude_tool_use_id(call_id, name, args)
}

/// util.SanitizedFunctionNameMap, for executors that key tool schemas like Go.
pub fn sanitized_function_name_map(raw: &[u8]) -> std::collections::HashMap<Vec<u8>, Vec<u8>> {
    responses_tools::sanitized_function_name_map(raw)
}

/// util.MapSanitizedFunctionName.
pub fn map_sanitized_function_name(map: &std::collections::HashMap<Vec<u8>, Vec<u8>>, name: &[u8]) -> Vec<u8> {
    responses_tools::map_sanitized_function_name(map, name)
}

/// StripInvalidGeminiSignatureThinkingBlocks (internal/translator/antigravity/claude):
/// the Antigravity executor's cleanup of Claude requests for Gemini-family models.
pub fn strip_invalid_gemini_signature_thinking_blocks(payload: &[u8]) -> Vec<u8> {
    antigravity_claude::strip_invalid_gemini_signature_thinking_blocks(payload)
}

// Go's `...WithCompat` request converters, exported beside the registered pairs for
// compatibility endpoints; deeply nested bodies run on a sized stack as registered
// requests do.

/// ConvertOpenAIResponsesRequestToOpenAIChatCompletions as Go's Chat Completions handler
/// calls it: the converted body, even when the conversion would refuse an emptied turn.
/// The only error left is a failed stack reservation for a deeply nested body.
pub fn responses_to_chat_completions_body(model: &str, body: &[u8], stream: bool) -> Result<Vec<u8>, Error> {
    deep_stack(levels(&[body]), || {
        Ok(openai_responses::convert_with_refusal(model, body, stream).0)
    })
}

/// ConvertOpenAIRequestToClaudeWithCompat.
pub fn openai_to_claude_with_compat(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    deep_stack(levels(&[body]), || claude_chat_request::request_with_compat(ctx, body))
}

/// ConvertClaudeRequestToCodexWithCompat.
pub fn claude_to_codex_with_compat(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    deep_stack(levels(&[body]), || codex_claude::request_with_compat(ctx, body))
}

/// ConvertClaudeRequestToGeminiWithCompat.
pub fn claude_to_gemini_with_compat(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    deep_stack(levels(&[body]), || gemini_claude::request_with_compat(ctx, body))
}

/// ConvertClaudeRequestToInteractionsWithCompat.
pub fn claude_to_interactions_with_compat(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    deep_stack(levels(&[body]), || interactions_claude::request_with_compat(ctx, body))
}

/// ConvertClaudeRequestToOpenAIWithCompat.
pub fn claude_to_openai_with_compat(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    deep_stack(levels(&[body]), || openai_claude::request_with_compat(ctx, body))
}

/// ConvertOpenAIResponsesRequestToClaudeWithCompat: like the registered Responses ->
/// Claude request, but unsigned reasoning history is kept for compatibility endpoints.
pub fn responses_to_claude_with_compat(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    deep_stack(levels(&[body]), || {
        claude_responses::convert(ctx.model, body, ctx.stream, true, ctx.tool_search)
    })
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
    /// `upstream.claude.tool-search` as pinned for this request (Go
    /// `WithClientToolSearch`): Codex client tool_search on the Responses -> Claude pair.
    pub tool_search: bool,
}

/// Request-scoped inputs available to response transforms.
pub struct ResponseCtx<'a> {
    pub model: &'a str,
    pub original_request: &'a [u8],
    pub translated_request: &'a [u8],
    /// The request's pinned `upstream.claude.tool-search` ([`RequestCtx::tool_search`]).
    pub tool_search: bool,
}

pub trait StreamTranslator: Send {
    /// Select executor chunks before the HTTP handler's framing. Native translators
    /// already emit executor chunks; the shared framing adapter overrides this.
    fn executor_chunks(&mut self) {}
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
    /// Go's `CanFinalizeResponseStream` (0ea4e1dc): the OpenAI Chat Completions to
    /// Responses stream saw a finish reason that left every item done, so a clean EOF
    /// without `[DONE]` may translate a synthetic `data: [DONE]` instead of failing.
    fn can_finalize_response_stream(&self) -> bool {
        false
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
    // ponytail: static match; plugins translate through [`hooks`] for pairs without one.
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
        (Format::Gemini, Format::OpenAI) => Some(&openai_gemini::PAIR),
        (Format::Interactions, Format::Interactions) => Some(&gemini_interactions::PASSTHROUGH),
        (Format::Interactions, Format::Gemini) => Some(&gemini_interactions::INTERACTIONS_TO_GEMINI),
        (Format::Gemini, Format::Interactions) => Some(&gemini_interactions::GEMINI_TO_INTERACTIONS),
        (Format::OpenAI, Format::Interactions) => Some(&openai_interactions::OPENAI_TO_INTERACTIONS),
        (Format::Interactions, Format::OpenAI) => Some(&openai_interactions::INTERACTIONS_TO_OPENAI),
        (Format::OpenAIResponse, Format::Interactions) => Some(&responses_interactions::RESPONSES_TO_INTERACTIONS),
        (Format::Interactions, Format::OpenAIResponse) => Some(&responses_interactions::INTERACTIONS_TO_RESPONSES),
        (Format::Interactions, Format::Claude) => Some(&claude_interactions::PAIR),
        (Format::Claude, Format::Interactions) => Some(&interactions_claude::PAIR),
        (Format::Gemini, Format::Claude) => Some(&claude_gemini::PAIR),
        (Format::Gemini, Format::Antigravity) => Some(&antigravity_gemini::PAIR),
        (Format::OpenAI, Format::Antigravity) => Some(&antigravity_chat::PAIR),
        (Format::OpenAIResponse, Format::Antigravity) => Some(&antigravity_responses::PAIR),
        (Format::Interactions, Format::Antigravity) => Some(&antigravity_interactions::PAIR),
        (Format::Claude, Format::Antigravity) => Some(&antigravity_claude::PAIR),
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

/// A pair's `stream` driven the way a Go executor with [`stream::StreamOptions`] drives
/// it; `None` when Go registers no pair.
pub fn stream_with(
    client: Format,
    upstream: Format,
    ctx: &ResponseCtx<'_>,
    options: stream::StreamOptions,
) -> Option<Box<dyn StreamTranslator>> {
    registered(client, upstream).map(|r| {
        stream::framed_with(
            client,
            upstream,
            hooks::stream(upstream.as_str(), client.as_str(), ctx, (r.go_stream)(ctx)),
            levels(&[ctx.original_request, ctx.translated_request]),
            options,
        )
    })
}

/// The Go-shaped line translator behind a pair's `stream`, for golden tests.
#[doc(hidden)]
pub fn go_stream(client: Format, upstream: Format) -> Option<GoStreamFn> {
    registered(client, upstream).map(|r| r.go_stream)
}

/// sdk/translator TranslateRequest. A registered pair runs between summary extraction
/// (from the client body) and summary application (to the upstream body), then the
/// plugin request normalizers, which own the final payload. Without a pair, a differing
/// top-level `model` is rewritten; with plugin hooks, the normalizers run on that body
/// and a plugin request translator may take it, wrapped in the summary pipeline.
pub fn translate_request(
    client: Format,
    upstream: Format,
    ctx: &RequestCtx<'_>,
    body: &[u8],
) -> Result<Vec<u8>, Error> {
    translate_request_tracked(client, upstream, ctx, body).map(|t| t.body)
}

/// A translated request and whether a plugin request normalizer changed its Responses
/// `configuration_update` input items (Go `RequestEnvelope.ConfigurationUpdatesChanged`,
/// which the Codex executor passes on to thinking).
pub struct Translated {
    pub body: Vec<u8>,
    pub configuration_updates_changed: bool,
}

/// [`translate_request`] with [`Translated::configuration_updates_changed`].
pub fn translate_request_tracked(
    client: Format,
    upstream: Format,
    ctx: &RequestCtx<'_>,
    body: &[u8],
) -> Result<Translated, Error> {
    match pair(client, upstream) {
        Some(pair) => translate_registered(client, upstream, ctx, body, |body| (pair.request)(ctx, body)),
        None => Ok(translate_fallback(client.as_str(), upstream.as_str(), ctx, body)),
    }
}

/// [`translate_request`] between format names, which may name a format only a plugin
/// knows (Go's registry is keyed by name): such a pair has no native translator.
pub fn translate_request_named(
    client: &str,
    upstream: &str,
    ctx: &RequestCtx<'_>,
    body: &[u8],
) -> Result<Vec<u8>, Error> {
    match Format::parse(client).zip(Format::parse(upstream)) {
        Some((client, upstream)) => translate_request(client, upstream, ctx, body),
        None => Ok(translate_fallback(client, upstream, ctx, body).body),
    }
}

/// The registered branch of Go `TranslateRequestEnvelope`.
fn translate_registered(
    client: Format,
    upstream: Format,
    ctx: &RequestCtx<'_>,
    body: &[u8],
    native: impl FnOnce(&[u8]) -> Result<Vec<u8>, Error> + Send,
) -> Result<Translated, Error> {
    let out = deep_stack(levels(&[body]), || {
        let summary = thinking::extract_translated_summary(body, client.as_str(), upstream.as_str());
        let out = native(body)?;
        Ok(thinking::apply_summary_for_model(
            out,
            upstream.as_str(),
            ctx.model,
            summary,
        ))
    })?;
    let Some(h) = hooks::current() else {
        return Ok(Translated {
            body: out,
            configuration_updates_changed: false,
        });
    };
    let (body, changed) = hooks::normalize_tracked(&*h, client.as_str(), upstream.as_str(), ctx.model, out, ctx.stream);
    Ok(Translated {
        body,
        configuration_updates_changed: changed,
    })
}

/// The unregistered branch of Go `TranslateRequestEnvelope`.
fn translate_fallback(from: &str, to: &str, ctx: &RequestCtx<'_>, body: &[u8]) -> Translated {
    let mut out = body.to_vec();
    if !ctx.model.is_empty() && *gj::get(body, "model").bytes() != *ctx.model.as_bytes() {
        gj::set_str(&mut out, "model", ctx.model);
    }
    let Some(h) = hooks::current() else {
        return Translated {
            body: out,
            configuration_updates_changed: false,
        };
    };
    // The normalizers canonicalize the source before a plugin translator handles the
    // missing route; summary intent is read from the normalized source.
    let (mut out, changed) = hooks::normalize_tracked(&*h, from, to, ctx.model, out, ctx.stream);
    let summary = thinking::extract_translated_summary(&out, from, to);
    if let Some(translated) = h.translate_request(from, to, ctx.model, &out, ctx.stream) {
        out = thinking::apply_summary_for_model(translated, to, ctx.model, summary);
    }
    Translated {
        body: out,
        configuration_updates_changed: changed,
    }
}

/// sdk/translator TranslateRequestEnvelope: [`translate_request`] with the request-scoped
/// model info Go's executors put in the envelope (`ResolvedModelInfo` of the selected
/// credential). Only pairs Go registers with RegisterRequestEnvelope read it: OpenAI
/// Responses -> Antigravity, whose native web-search capability decides between a
/// dedicated web-search request and a normal one.
pub fn translate_request_envelope(
    client: Format,
    upstream: Format,
    ctx: &RequestCtx<'_>,
    body: &[u8],
    model_info: Option<&cpa_core::registry::ModelInfo>,
) -> Result<Vec<u8>, Error> {
    translate_request_envelope_tracked(client, upstream, ctx, body, model_info).map(|t| t.body)
}

/// [`translate_request_envelope`] with [`Translated::configuration_updates_changed`].
pub fn translate_request_envelope_tracked(
    client: Format,
    upstream: Format,
    ctx: &RequestCtx<'_>,
    body: &[u8],
    model_info: Option<&cpa_core::registry::ModelInfo>,
) -> Result<Translated, Error> {
    if (client, upstream) != (Format::OpenAIResponse, Format::Antigravity) {
        return translate_request_tracked(client, upstream, ctx, body);
    }
    translate_registered(client, upstream, ctx, body, |body| {
        antigravity_responses::request_envelope(ctx, body, model_info)
    })
}

/// The pair's Go-shaped stream translator with the plugin response hooks around it, for
/// executors that drive Go lines themselves. `None` when no pair is registered and no
/// hooks are active (Go's `TranslateStream` then passes each chunk through).
pub fn hooked_go_stream(client: &str, upstream: &str, ctx: &ResponseCtx<'_>) -> Option<Box<dyn stream::GoStream>> {
    let native = Format::parse(client)
        .zip(Format::parse(upstream))
        .and_then(|(c, u)| registered(c, u))
        .map(|r| (r.go_stream)(ctx));
    hooks::wrap_stream(upstream, client, ctx, native)
}

/// sdk/translator TranslateTokenCount: the pair's token-count shape, or the upstream body
/// unchanged when the pair registers none.
pub fn translate_token_count(client: Format, upstream: Format, count: i64, body: &[u8]) -> Vec<u8> {
    match token_count(client, upstream) {
        Some(render) => render(count),
        None => body.to_vec(),
    }
}

/// Bracket nesting Go's recursive JSON walkers may go through after reading some bytes:
/// the deepest nesting outside strings, and every bracket opened inside a string (JSON
/// text, such as tool arguments, that a translator may parse, or accumulate from stream
/// deltas and parse later). Counting string brackets without closing them keeps the bound
/// sound for any split, and string brackets written as `\u005b`/`\u007b` count too (text
/// escaped twice, JSON inside a string inside a string, is not decoded); a body with
/// much code in its strings over-counts and just runs on a larger stack. Fed in pieces,
/// it covers what a stream retains.
#[derive(Default)]
pub(crate) struct Depth {
    depth: usize,
    max: usize,
    string_opens: usize,
    in_string: bool,
    escaped: bool,
    /// Inside a `\u` escape: (hex digits read, value so far).
    unicode: Option<(u8, u32)>,
}

impl Depth {
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        for &c in bytes {
            if self.in_string {
                if let Some((digits, value)) = self.unicode {
                    if let Some(d) = (c as char).to_digit(16) {
                        let value = value * 16 + d;
                        self.unicode = (digits < 3).then_some((digits + 1, value));
                        if digits == 3 && (value == u32::from(b'[') || value == u32::from(b'{')) {
                            self.string_opens += 1;
                        }
                        continue;
                    }
                    self.unicode = None;
                }
                if self.escaped {
                    self.escaped = false;
                    if c == b'u' {
                        self.unicode = Some((0, 0));
                    }
                } else if c == b'\\' {
                    self.escaped = true;
                } else if c == b'"' {
                    self.in_string = false;
                } else if c == b'{' || c == b'[' {
                    self.string_opens += 1;
                }
                continue;
            }
            match c {
                b'"' => self.in_string = true,
                b'{' | b'[' => {
                    self.depth += 1;
                    self.max = self.max.max(self.depth);
                }
                b'}' | b']' => self.depth = self.depth.saturating_sub(1),
                _ => {}
            }
        }
    }

    pub(crate) fn levels(&self) -> usize {
        self.max.max(self.string_opens)
    }
}

/// The largest [`Depth::levels`] of separately scanned bodies.
pub(crate) fn levels(bodies: &[&[u8]]) -> usize {
    bodies
        .iter()
        .map(|body| {
            let mut depth = Depth::default();
            depth.feed(body);
            depth.levels()
        })
        .max()
        .unwrap_or(0)
}

/// Runs `f` on a thread with a stack sized for `levels` of nesting when that exceeds what
/// a caller's stack (a 2 MiB async worker) safely holds. Go's JSON walkers recurse per
/// nesting level on growable stacks (up to 1 GiB); the ported walkers recurse too, so
/// deep data would otherwise overflow a native stack and abort the process. A stack that
/// cannot be reserved (memory limits on a small host) is an error, not a panic.
// ponytail: the stack is sized from a measured per-level cost of the deepest walkers
// (under 2 KiB in debug builds, which use more stack than release; 4x margin), clamped
// to Go's 1 GiB maximum. Once a stream's strings have held more than 256 brackets (code
// in text deltas), each later event runs on a new thread (about 32 us measured); a
// per-stream worker thread would remove that cost if it ever matters.
pub(crate) fn deep_stack<T: Send>(levels: usize, f: impl FnOnce() -> Result<T, Error> + Send) -> Result<T, Error> {
    const DEEP: usize = 256;
    const PER_LEVEL: usize = 8 << 10;
    const MIN_STACK: usize = 8 << 20;
    thread_local! {
        static ON_DEEP_STACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if ON_DEEP_STACK.with(std::cell::Cell::get) || levels <= DEEP {
        return f();
    }
    #[cfg(test)]
    if FAIL_SPAWN.with(std::cell::Cell::get) {
        return Err(Error("translator: test stack reservation failure".into()));
    }
    let size = levels.saturating_mul(PER_LEVEL).clamp(MIN_STACK, 1 << 30);
    std::thread::scope(|scope| {
        let worker = std::thread::Builder::new().stack_size(size).spawn_scoped(scope, || {
            ON_DEEP_STACK.with(|flag| flag.set(true));
            f()
        });
        match worker {
            Ok(handle) => handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
            Err(e) => Err(Error(format!(
                "translator: cannot reserve a {size}-byte stack for JSON nested {levels} levels deep: {e}"
            ))),
        }
    })
}

#[cfg(test)]
thread_local! {
    /// Makes [`deep_stack`] fail as if its stack could not be reserved.
    pub(crate) static FAIL_SPAWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
mod go_helper_tests;

#[cfg(test)]
mod depth_tests {
    use super::{Depth, levels};

    #[test]
    fn depth_counts_string_brackets_written_as_escapes() {
        // Bare JSON strings: no structural nesting, so the count is the string brackets.
        assert_eq!(levels(&[br#""\u005b\u005B\u007b x[""#]), 4);
        assert_eq!(
            levels(&[br#""\\u005b \u005c \u005bz""#]),
            1,
            "escaped backslash, other escapes"
        );
        assert_eq!(levels(&[br#""\u05b \u00""#]), 0, "malformed escapes");
        assert_eq!(levels(&[br#""\"[""#]), 1, "escaped quote stays in the string");
        // A stream's escapes split anywhere still count.
        let body = format!(r#""{}""#, r"\u005b".repeat(300));
        let mut depth = Depth::default();
        for piece in body.as_bytes().chunks(3) {
            depth.feed(piece);
        }
        assert_eq!(depth.levels(), 300);
        assert_eq!(levels(&[b"[[[[]]]", br#"{"a":"]]]"}"#]), 4, "the deepest body");
    }
}
