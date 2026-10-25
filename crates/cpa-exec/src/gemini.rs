//! Gemini API keys and native Interactions keys (internal/runtime/executor/gemini_executor.go).
//!
//! `gemini` credentials call `generateContent`, `streamGenerateContent` and
//! `countTokens` under `{base}/v1beta/models/{model}`. `gemini-interactions` credentials
//! send Interactions, OpenAI, Responses, Claude and Gemini clients to
//! `{base}/v1beta/interactions`; any other client format takes the generateContent path.
//! Both authenticate with `x-goog-api-key` and add the credential's custom headers
//! (`cpa_common::headers`); config payload rules apply through `cpa_common::payload`.
//!
//! Stream items are the bytes the client's Go route handler writes: Gemini clients get
//! `data: <chunk>\n\n`, or the bare chunk when the request named an `alt`; Interactions
//! clients of the native API get the upstream SSE frame itself.

use std::collections::VecDeque;

use bytes::Bytes;
use cpa_common::json as gj;
use cpa_common::thinking::{self, RequestThinking, parse_suffix};
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::{
    CaptureEvent, CaptureSink, ExecError, ExecRequest, ExecResponse, ExecStream, FailureScope, Operation, ResponseBody,
    UpstreamRequest, UsageSink,
};
use cpa_core::format::Format;
use cpa_translate::{RequestCtx, ResponseCtx, StreamTranslator};
use futures_util::StreamExt;

use crate::gemini_payload as payload;
use crate::gemini_stream::{self as sse, ClaudeInputTokens};
use crate::payload_barrier::Barrier;
use crate::proxy::{self, GoClients, GoHeaders, Hooks, Proxy, Upstream};

/// Providers this executor serves (`NewGeminiExecutor`, `NewGeminiInteractionsExecutor`).
pub const PROVIDERS: [&str; 2] = ["gemini", "gemini-interactions"];

pub fn handles(provider: &str) -> bool {
    PROVIDERS.contains(&provider)
}

pub(crate) const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";
pub(crate) const API_VERSION: &str = "v1beta";
/// `streamScannerBuffer`.
pub(crate) const MAX_LINE: usize = 52_428_800;
/// `geminiInteractionsAPIRevision`.
const INTERACTIONS_REVISION: &str = "2026-05-20";
pub(crate) const COMPACT_ALT: &str = "responses/compact";
/// helps.ApplyPatchUpstreamErrorMessage: Go's answer to an empty or failed translation.
const EMPTY_TRANSLATION: &str = "Invalid apply_patch tool arguments received from upstream.";

pub struct GeminiExecutor {
    clients: GoClients,
}

impl Default for GeminiExecutor {
    fn default() -> Self {
        Self {
            clients: GoClients::new(Hooks::default()),
        }
    }
}

/// Same status-only classification as the other API-key executors.
fn scope_for(status: u16) -> FailureScope {
    match status {
        401 | 402 | 403 | 408 | 429 | 500.. => FailureScope::Credential,
        _ => FailureScope::Request,
    }
}

/// `statusErr{code, msg}`: no retry hint, no headers; an empty message reads `status N`.
pub(crate) fn status_err(status: u16, message: impl Into<Bytes>) -> ExecError {
    let mut body: Bytes = message.into();
    if body.is_empty() {
        body = Bytes::from(format!("status {status}"));
    }
    ExecError {
        status,
        scope: scope_for(status),
        body,
        headers: Box::default(),
        retry_after: None,
        direct: false,
        addon: None,
    }
}

pub(crate) fn bad_gateway() -> ExecError {
    status_err(502, EMPTY_TRANSLATION)
}

/// `geminiAPIKey`: the attribute as stored.
fn api_key(credential: &Credential) -> &str {
    credential
        .attributes
        .get("api_key")
        .map(String::as_str)
        .unwrap_or_default()
}

/// `resolveGeminiBaseURL`.
fn base_url(credential: &Credential) -> String {
    let custom = credential
        .attributes
        .get("base_url")
        .map(|v| v.trim())
        .unwrap_or_default();
    let base = custom.trim_end_matches('/');
    if base.is_empty() {
        DEFAULT_BASE_URL.to_owned()
    } else {
        base.to_owned()
    }
}

/// `shouldExecuteNativeInteractions`.
fn native_interactions(credential: &Credential, source: Format) -> bool {
    credential.provider.trim().eq_ignore_ascii_case("gemini-interactions")
        && matches!(
            source,
            Format::Interactions | Format::OpenAI | Format::OpenAIResponse | Format::Claude | Format::Gemini
        )
}

/// Go `opts.Alt`: absent and empty are the same.
pub(crate) fn alt(req: &ExecRequest) -> Option<&str> {
    req.alt.as_deref().filter(|a| !a.is_empty())
}

/// `helps.ApplyPatchOriginalRequest`.
pub(crate) fn original_request(req: &ExecRequest) -> &Bytes {
    if req.original_body.is_empty() {
        &req.body
    } else {
        &req.original_body
    }
}

/// `TranslateRequestWithAPIKeyModelCompatibility` (`TranslateRequestWithCodexMultiAgentV2`
/// when `compat` is false) for one payload: the shared Codex-client rewrites, then the
/// registered pair or, for an is-compat model, its `...WithCompat` translator.
pub(crate) fn translate(
    req: &ExecRequest,
    cfg: &Config,
    target: Format,
    model: &str,
    body: &[u8],
    stream: bool,
    compat: bool,
) -> Result<Vec<u8>, ExecError> {
    let ctx = RequestCtx {
        model,
        stream,
        tool_search: false,
    };
    let client = crate::codex_client::Client::new(&req.headers, cfg, "", compat);
    deep_stack(body, || {
        crate::codex_client::translate_request(req.source_format, target, &ctx, body, &client).map(|t| t.body)
    })
    .map_err(|e| ExecError::local(400, FailureScope::Request, e.to_string()))
}

/// Runs a translator on a thread with Go's maximum goroutine stack when the body nests
/// deeply, as `cpa_translate::translate_request` does for registered pairs: the ported
/// schema walkers recurse per nesting level like Go's, whose stacks grow.
// ponytail: duplicate of cpa_translate's private guard until its exported `...WithCompat`
// entry points guard themselves (owner: translators).
pub(crate) fn deep_stack<T: Send>(body: &[u8], f: impl FnOnce() -> T + Send) -> T {
    const DEEP: usize = 256;
    let (mut depth, mut max, mut in_string, mut escaped) = (0usize, 0usize, false, false);
    for &b in body {
        if in_string {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                max = max.max(depth);
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if max <= DEEP {
        return f();
    }
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(1 << 30)
            .spawn_scoped(scope, f)
            .map(|handle| handle.join())
    })
    .ok()
    .and_then(Result::ok)
    .expect("deep JSON translation thread")
}

/// `TranslateRequestPairWithAPIKeyModelCompatibility`: the payload-config baseline from
/// the original request, and the working body from the current one (translated once
/// when both are the same bytes).
fn translate_pair(
    req: &ExecRequest,
    cfg: &Config,
    target: Format,
    model: &str,
    compat: bool,
) -> Result<(Vec<u8>, Vec<u8>), ExecError> {
    // Go translates the original first; equal payloads once.
    let source = original_request(req);
    let original = translate(req, cfg, target, model, source, req.stream, compat)?;
    if *source == req.body {
        return Ok((original.clone(), original));
    }
    let working = translate(req, cfg, target, model, &req.body, req.stream, compat)?;
    Ok((original, working))
}

/// `helps.ApplyRequestThinking`.
pub(crate) fn apply_thinking(
    req: &ExecRequest,
    body: Vec<u8>,
    from: Format,
    to: Format,
    provider: &str,
    resolved: Option<&Resolved>,
) -> Result<Vec<u8>, ExecError> {
    thinking::apply_request_thinking(&RequestThinking {
        body: &body,
        payload: &req.body,
        original: &req.original_body,
        model: &req.model,
        from: from.as_str(),
        to: to.as_str(),
        provider,
        resolved: resolved.map(|r| Some(&r.caps)),
        has_request_transformer: cpa_translate::pair(from, to).is_some(),
        updates_changed: false,
    })
    .map_err(|e| ExecError::local(e.status(), FailureScope::Request, e.message))
}

/// The token-count baseline for `default` rules: the client's original request
/// translated without stream, else the translated body before thinking. Go counts with
/// a failed translation's partial body; this refuses with its error instead.
pub(crate) fn count_original(
    req: &ExecRequest,
    cfg: &Config,
    target: Format,
    model: &str,
    compat: bool,
    translated: &[u8],
) -> Result<Vec<u8>, ExecError> {
    if req.original_body.is_empty() {
        return Ok(translated.to_vec());
    }
    translate(req, cfg, target, model, &req.original_body, false, compat)
}

/// Content-Type, the API key and the credential's custom headers (`applyGeminiHeaders`).
fn request_headers(credential: &Credential, req: &ExecRequest) -> GoHeaders {
    let mut headers = GoHeaders::new();
    headers.set("Content-Type", "application/json");
    let key = api_key(credential);
    if !key.is_empty() {
        headers.set("x-goog-api-key", key);
    }
    set_custom_headers(&mut headers, credential, req);
    headers
}

/// `applyGeminiHeaders` (util.ApplyCustomHeadersFromAttrs). `$CPA-SESSION-ID` is the
/// attempt's canonical session: Go's conductor binds it to the context
/// (ensureCanonicalSessionMetadata + syncMetadataSessionToContext) before the executor
/// runs, and EnsureSessionContext keeps it. Dispatch puts the same bound identity,
/// derived and message-hash fallbacks included, in `req.session`.
pub(crate) fn set_custom_headers(headers: &mut GoHeaders, credential: &Credential, req: &ExecRequest) {
    let session = cpa_common::session::cpa_session_id(req.session.as_deref());
    for (name, value) in cpa_common::headers::custom_headers(&credential.attributes, &req.headers, session.as_deref()) {
        headers.set(&name, value);
    }
}

/// Go `cliproxyauth.ResolvedModelInfo(req)`: the capabilities dispatch bound to this
/// attempt, and `helps.APIKeyModelIsCompat`.
pub(crate) fn resolved(req: &ExecRequest) -> Option<Resolved> {
    req.resolved_model.as_ref().map(|r| Resolved {
        caps: cpa_common::thinking::ModelCaps::from(&r.info),
        is_compat: r.is_compat(),
    })
}

/// Capabilities bound to an attempt, as thinking and translation read them.
pub(crate) struct Resolved {
    pub caps: cpa_common::thinking::ModelCaps,
    pub is_compat: bool,
}

/// Go `helps.RecordAPIRequest` for one physical attempt: `headers` are the request's
/// header map in Go spelling (the observer sorts and masks them). The log may keep the
/// body for the whole exchange, so it gets a handle to the body being sent.
pub(crate) fn capture_request(
    capture: &CaptureSink,
    credential: &Credential,
    provider: &str,
    url: &str,
    headers: &[(String, String)],
    body: &Bytes,
) {
    if !capture.enabled() {
        return;
    }
    let (auth_type, auth_value) = cpa_core::registry::dynamic::account_info(credential);
    capture.record_request(
        UpstreamRequest {
            url,
            method: "POST",
            headers,
            body,
            provider,
            auth_id: &credential.id,
            auth_label: &credential.label,
            auth_type,
            auth_value: &auth_value,
        },
        body,
    );
}

/// Go `helps.RecordAPIResponseMetadata` for an upstream answer, with the headers as
/// Go's `http.Response` holds them.
pub(crate) use crate::kimi_http::capture_metadata;

/// `RecordAPIRequest`, `Client.Do`, then `RecordAPIResponseMetadata`, or
/// `RecordAPIResponseError` when the request fails. `lines`: the caller scans the body
/// line by line, so a body cut short fails with Go's `unexpected EOF`
/// ([`proxy::send_scanned`]).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_captured(
    capture: &CaptureSink,
    credential: &Credential,
    provider: &str,
    client: &wreq::Client,
    url: &str,
    headers: GoHeaders,
    body: Bytes,
    lines: bool,
) -> Result<Upstream, ExecError> {
    capture_request(capture, credential, provider, url, headers.pairs(), &body);
    let sent = if lines {
        proxy::send_scanned(client, url, headers, body, None).await
    } else {
        proxy::send(client, url, headers, body, None).await
    };
    match sent {
        Ok(upstream) => {
            capture_metadata(capture, upstream.status, &upstream.headers);
            Ok(upstream)
        }
        Err(error) => {
            capture.record(CaptureEvent::ResponseError(&String::from_utf8_lossy(&error.body)));
            Err(error)
        }
    }
}

/// `io.ReadAll` of a response body, then `AppendAPIResponseChunk` of it, or
/// `RecordAPIResponseError` when the read fails.
pub(crate) async fn read_captured(capture: &CaptureSink, upstream: Upstream, limit: usize) -> Result<Bytes, ExecError> {
    match proxy::read_all(upstream.body, limit, false).await {
        Ok(data) => {
            capture.record(CaptureEvent::ResponseChunk(&data));
            Ok(data)
        }
        Err(error) => {
            capture.record(CaptureEvent::ResponseError(&String::from_utf8_lossy(&error.body)));
            Err(error)
        }
    }
}

/// Go's scanner loops: `AppendAPIResponseChunk` of every line read and
/// `RecordAPIResponseError` of the scanner's error.
pub(crate) fn captured_lines(lines: ExecStream, capture: CaptureSink) -> ExecStream {
    if !capture.enabled() {
        return lines;
    }
    lines
        .inspect(move |item| match item {
            Ok(line) => capture.record(CaptureEvent::ResponseChunk(line)),
            Err(error) => capture.record(CaptureEvent::ResponseError(&String::from_utf8_lossy(&error.body))),
        })
        .boxed()
}

/// A non-2xx answer: its body (`AppendAPIResponseChunk`, read errors ignored as Go's
/// `b, _ := io.ReadAll` does) becomes the status error.
pub(crate) async fn error_from(capture: &CaptureSink, upstream: Upstream) -> ExecError {
    let status = upstream.status;
    let body = proxy::read_all(upstream.body, proxy::MAX_ERROR_BODY, true)
        .await
        .unwrap_or_default();
    capture.record(CaptureEvent::ResponseChunk(&body));
    status_err(status, body)
}

/// `io.ReadAll` of the whole response, then Go's status check (countTokens and
/// Interactions read before checking).
async fn read_then_check(capture: &CaptureSink, upstream: Upstream) -> Result<(http::HeaderMap, Bytes), ExecError> {
    let status = upstream.status;
    let headers = upstream.headers.clone();
    let ok = (200..300).contains(&status);
    let limit = if ok { usize::MAX } else { proxy::MAX_ERROR_BODY };
    let data = read_captured(capture, upstream, limit).await?;
    if !ok {
        return Err(status_err(status, data));
    }
    Ok((headers, data))
}

/// The executor identity Go logs for a Gemini-family key (`GeminiExecutor.Identifier`).
fn identifier(credential: &Credential) -> &'static str {
    if credential.provider == "gemini-interactions" {
        "gemini-interactions"
    } else {
        "gemini"
    }
}

/// `TranslateNonStream`: the registered transform, or the body itself when Go registers
/// none (a Codex client of a Gemini upstream); then Go's empty-output check and Responses
/// usage details.
pub(crate) fn translate_non_stream(
    req: &ExecRequest,
    upstream: Format,
    translated: &[u8],
    data: &[u8],
) -> Result<Bytes, ExecError> {
    let response = req.response_format;
    let ctx = ResponseCtx {
        model: &req.model,
        original_request: original_request(req),
        translated_request: translated,
        tool_search: false,
    };
    let mut out = match cpa_translate::pair(response, upstream) {
        Some(pair) => (pair.non_stream)(&ctx, data).map_err(|_| bad_gateway())?,
        // Go's TranslateNonStream still runs the plugin response hooks.
        None => cpa_translate::hooks::unregistered_non_stream(upstream.as_str(), response.as_str(), &ctx, data)
            .unwrap_or_else(|| data.to_vec()),
    };
    if out.is_empty() {
        return Err(bad_gateway());
    }
    if response == Format::OpenAIResponse {
        out = crate::openai_compat_payload::ensure_responses_usage_details(&out);
    }
    Ok(Bytes::from(out))
}

impl GeminiExecutor {
    /// Uses `client` for credentials without a proxy (tests pass a plain client).
    pub fn with_client(client: wreq::Client) -> Self {
        Self {
            clients: GoClients::with_default(client),
        }
    }

    fn client(&self, credential: &Credential, cfg: &Config) -> wreq::Client {
        self.clients.get(&Proxy::effective(credential, cfg))
    }

    pub async fn execute(
        &self,
        credential: &Credential,
        req: ExecRequest,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        if req.operation == Operation::CountTokens {
            return self.count_tokens(credential, &req, cfg).await;
        }
        if req.alt.as_deref() == Some(COMPACT_ALT) {
            return Err(status_err(501, "/responses/compact not supported"));
        }
        if native_interactions(credential, req.source_format) {
            return self.interactions(credential, &req, cfg).await;
        }
        self.generate(credential, &req, cfg).await
    }

    /// `Execute` / `ExecuteStream` against generateContent / streamGenerateContent.
    async fn generate(
        &self,
        credential: &Credential,
        req: &ExecRequest,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        let base_model = parse_suffix(&req.model).model_name;
        let (from, to) = (req.source_format, Format::Gemini);
        let resolved = resolved(req);
        let compat = resolved.as_ref().is_some_and(|r| r.is_compat);
        let (original_translated, body) = translate_pair(req, cfg, to, &base_model, compat)?;
        let mut body = apply_thinking(req, body, from, to, &credential.provider, resolved.as_ref())?;
        body = payload::fix_image_aspect_ratio(&base_model, body);
        body = payload::set_str_if_different(body, "model", &base_model);
        body = payload::cap_max_output_tokens(body, &base_model);
        body = cpa_common::signature::sanitize_gemini_request_thought_signatures(&body, "contents");
        body = payload::ensure_leading_user_content(body, "contents");
        body = payload::ensure_trailing_user_content(body, "contents");
        let action = if req.stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        let mut url = format!("{}/{API_VERSION}/models/{base_model}:{action}", base_url(credential));
        match alt(req) {
            Some(alt) => url.push_str(&format!("?$alt={alt}")),
            None if req.stream => url.push_str("?alt=sse"),
            None => {}
        }
        body = payload::delete(body, "session_id");
        req.usage.request(to, &body);
        // The payload rules are the last change to the body (868e137).
        body = Barrier {
            executor: "",
            model: &base_model,
            protocol: to.as_str(),
            root: "",
            original: &original_translated,
        }
        .apply(cfg, req, body);
        let headers = request_headers(credential, req);
        let capture = req.capture();
        let client = self.client(credential, cfg);
        // One buffer for the send, the request log and the translators below.
        let body = Bytes::from(body);
        let upstream = send_captured(
            capture,
            credential,
            identifier(credential),
            &client,
            &url,
            headers,
            body.clone(),
            req.stream,
        )
        .await?;
        if !(200..300).contains(&upstream.status) {
            return Err(error_from(capture, upstream).await);
        }
        let response_headers = upstream.headers.clone();
        if !req.stream {
            let data = read_captured(capture, upstream, usize::MAX).await?;
            req.usage.response_body(to, &data);
            let out = translate_non_stream(req, to, &body, &data)?;
            return Ok(ExecResponse {
                status: 200,
                headers: response_headers,
                body: ResponseBody::Buffered(out),
            });
        }
        let output = Output::new(req, to, &body);
        let lines = captured_lines(proxy::lines(upstream.body, MAX_LINE), capture.clone());
        let stream = gemini_lines(lines, output, Format::Gemini);
        Ok(ExecResponse {
            status: 200,
            headers: response_headers,
            body: ResponseBody::Stream(stream),
        })
    }

    /// `executeInteractions` / `executeInteractionsStream`.
    async fn interactions(
        &self,
        credential: &Credential,
        req: &ExecRequest,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        let target = parse_suffix(&req.model).model_name;
        let (from, to) = (req.source_format, Format::Interactions);
        let resolved = resolved(req);
        let compat = resolved.as_ref().is_some_and(|r| r.is_compat);
        // Interactions clients are sent as they are, without the registry normalizer.
        let (original_translated, mut body) = if from == Format::Interactions {
            (original_request(req).to_vec(), req.body.to_vec())
        } else {
            translate_pair(req, cfg, to, &target, compat)?
        };
        if gj::get(&body, "model").exists() && !target.is_empty() {
            body = payload::set_str_if_different(body, "model", &target);
        }
        // applyGeminiInteractionsThinking: the Gemini applier family for the target.
        body = apply_thinking(req, body, from, to, "gemini", resolved.as_ref())?;
        body = payload::sanitize_interactions_input_ids(body);
        if req.stream {
            body = payload::set_bool_if_different(body, "stream", true);
        }
        body = Barrier {
            executor: "",
            model: &target,
            protocol: "interactions",
            root: "",
            original: &original_translated,
        }
        .apply(cfg, req, body);
        let url = format!("{}/{API_VERSION}/interactions", base_url(credential));
        let mut headers = request_headers(credential, req);
        // applyGeminiInteractionsRequestHeaders, then applyGeminiInteractionsRevisionHeader.
        if headers.get("Api-Revision").is_none_or(str::is_empty)
            && let Some(revision) = req
                .headers
                .get("Api-Revision")
                .and_then(|v| v.to_str().ok())
                .filter(|v| !v.is_empty())
        {
            headers.set("Api-Revision", revision);
        }
        if headers.get("Api-Revision").is_none_or(str::is_empty) {
            headers.set("Api-Revision", INTERACTIONS_REVISION);
        }
        let capture = req.capture();
        let client = self.client(credential, cfg);
        // One buffer for the send, the request log and the translators below.
        let body = Bytes::from(body);
        let upstream = send_captured(
            capture,
            credential,
            identifier(credential),
            &client,
            &url,
            headers,
            body.clone(),
            req.stream,
        )
        .await?;
        if !req.stream {
            let (response_headers, data) = read_then_check(capture, upstream).await?;
            req.usage.response_body(to, &data);
            let out = translate_non_stream(req, to, &body, &data)?;
            return Ok(ExecResponse {
                status: 200,
                headers: response_headers,
                body: ResponseBody::Buffered(out),
            });
        }
        if !(200..300).contains(&upstream.status) {
            return Err(error_from(capture, upstream).await);
        }
        let response_headers = upstream.headers.clone();
        let output = Output::new(req, to, &body);
        let lines = captured_lines(proxy::lines(upstream.body, MAX_LINE), capture.clone());
        let stream = interactions_lines(lines, output);
        Ok(ExecResponse {
            status: 200,
            headers: response_headers,
            body: ResponseBody::Stream(stream),
        })
    }

    /// `CountTokens`: the translated request without tools, generation and safety
    /// settings, a leading user turn only, and Go's token-count shape for the client.
    async fn count_tokens(
        &self,
        credential: &Credential,
        req: &ExecRequest,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        let base_model = parse_suffix(&req.model).model_name;
        let (from, to) = (req.source_format, Format::Gemini);
        let resolved = resolved(req);
        let compat = resolved.as_ref().is_some_and(|r| r.is_compat);
        let body = translate(req, cfg, to, &base_model, &req.body, false, compat)?;
        let original = count_original(req, cfg, to, &base_model, compat, &body)?;
        let mut body = apply_thinking(req, body, from, to, &credential.provider, resolved.as_ref())?;
        body = payload::fix_image_aspect_ratio(&base_model, body);
        for path in ["tools", "generationConfig", "safetySettings"] {
            body = payload::delete(body, path);
        }
        body = payload::set_str_if_different(body, "model", &base_model);
        body = cpa_common::signature::sanitize_gemini_request_thought_signatures(&body, "contents");
        body = payload::ensure_leading_user_content(body, "contents");
        body = Barrier {
            executor: "gemini",
            model: &base_model,
            protocol: to.as_str(),
            root: "",
            original: &original,
        }
        .apply(cfg, req, body);
        let url = format!("{}/{API_VERSION}/models/{base_model}:countTokens", base_url(credential));
        let headers = request_headers(credential, req);
        let capture = req.capture();
        let client = self.client(credential, cfg);
        // One buffer for the send, the request log and the translators below.
        let body = Bytes::from(body);
        let upstream = send_captured(
            capture,
            credential,
            identifier(credential),
            &client,
            &url,
            headers,
            body.clone(),
            false,
        )
        .await?;
        let (response_headers, data) = read_then_check(capture, upstream).await?;
        let count = gj::get(&data, "totalTokens").int();
        let out = cpa_translate::translate_token_count(req.response_format, to, count, &data);
        Ok(ExecResponse {
            status: 200,
            headers: response_headers,
            body: ResponseBody::Buffered(Bytes::from(out)),
        })
    }
}

/// Client items, and the terminal error that ends the stream after them.
#[derive(Default)]
pub(crate) struct Emit {
    pub(crate) out: Vec<Bytes>,
    pub(crate) stop: Option<ExecError>,
}

impl Emit {
    /// Appends `next`; returns whether the stream stopped.
    pub(crate) fn then(&mut self, next: Emit) -> bool {
        self.out.extend(next.out);
        self.stop = next.stop;
        self.stop.is_some()
    }
}

/// Go's `TranslateStream` without a registered transform: the payload as one chunk.
/// Only Gemini upstreams reach it (a Codex client); every client format that takes the
/// native Interactions path has a registered Interactions pair. Executors that pass
/// scanned lines as [`line_event`]s get the line back without the terminator.
/// With plugin response hooks active, each payload goes through them (`hooks`).
struct Unregistered {
    options: cpa_translate::stream::StreamOptions,
    hooks: Option<Box<dyn cpa_translate::stream::GoStream>>,
}

impl StreamTranslator for Unregistered {
    fn event(&mut self, event: &[u8]) -> Result<Vec<Bytes>, cpa_translate::Error> {
        let line = if self.options.whole_events {
            event
        } else {
            event.strip_suffix(LINE_END).unwrap_or(event)
        };
        let emit = |chunk: &[u8]| match self.options.chunk {
            Some(hook) => Bytes::from(hook(chunk)),
            None => Bytes::copy_from_slice(chunk),
        };
        match &mut self.hooks {
            Some(hooks) => Ok(hooks.line(line)?.iter().map(|c| emit(c)).collect()),
            None => Ok(vec![emit(line)]),
        }
    }

    fn finish(&mut self) -> Result<Vec<Bytes>, cpa_translate::Error> {
        Ok(Vec::new())
    }
}

/// Turns Go stream chunks into the bytes the client's route handler writes.
pub(crate) struct Output {
    translator: Box<dyn StreamTranslator>,
    client: Format,
    /// A Gemini client asked for `alt`: chunks are written bare.
    raw: bool,
    executor_chunks: bool,
    claude: ClaudeInputTokens,
    /// The attempt's usage reporter; stream loops feed it what Go's reporter sees.
    pub(crate) usage: UsageSink,
    /// The attempt's request capture, for errors a stream loop finds in a payload.
    capture: CaptureSink,
}

impl Output {
    pub(crate) fn new(req: &ExecRequest, upstream: Format, translated: &[u8]) -> Self {
        Self::with_options(
            req,
            upstream,
            translated,
            cpa_translate::stream::StreamOptions::default(),
        )
    }

    /// [`Output::new`] for an executor that drives the translator with `options`.
    pub(crate) fn with_options(
        req: &ExecRequest,
        upstream: Format,
        translated: &[u8],
        options: cpa_translate::stream::StreamOptions,
    ) -> Self {
        let client = req.response_format;
        let ctx = ResponseCtx {
            model: &req.model,
            original_request: original_request(req),
            translated_request: translated,
            tool_search: false,
        };
        let mut translator = cpa_translate::stream_with(client, upstream, &ctx, options).unwrap_or_else(|| {
            Box::new(Unregistered {
                options,
                hooks: cpa_translate::hooks::wrap_stream(upstream.as_str(), client.as_str(), &ctx, None),
            })
        });
        if req.stream_payloads {
            translator.executor_chunks();
        }
        Self {
            translator,
            client,
            raw: alt(req).is_some(),
            executor_chunks: req.stream_payloads,
            claude: ClaudeInputTokens::new(req.source_format, upstream, client, original_request(req).clone()),
            usage: req.usage.clone(),
            capture: req.capture().clone(),
        }
    }

    /// The Claude `message_start` estimate over `original` instead of the client's
    /// original request (Antigravity estimates its cleaned request).
    pub(crate) fn with_claude_original(mut self, original: Bytes) -> Self {
        self.claude = self.claude.with_original(original);
        self
    }

    /// helps.EndApplyPatchStream, then the client side's pending frames: the end of a
    /// stream that feeds no `[DONE]`.
    pub(crate) fn end(&mut self) -> Emit {
        let mut emit = self.finalize();
        if emit.stop.is_none() {
            emit.then(self.finish());
        }
        emit
    }

    /// End of a Gemini-upstream stream: the tool-input finalization, then `[DONE]`
    /// through the translator, then the client side's pending frames.
    pub(crate) fn end_with_done(&mut self) -> Emit {
        let mut emit = self.finalize();
        if emit.stop.is_none() && !emit.then(self.translate(b"[DONE]")) {
            emit.then(self.finish());
        }
        emit
    }

    /// One Go `TranslateStreamWithClaudeInputTokens` call, framed for the client, then
    /// helps.StopApplyPatchStream: an apply_patch failure ends the stream after the
    /// event's frames.
    pub(crate) fn translate(&mut self, payload: &[u8]) -> Emit {
        let mut out = match self.translator.event(payload) {
            Ok(out) => out,
            Err(_) => {
                return Emit {
                    out: Vec::new(),
                    stop: Some(bad_gateway()),
                };
            }
        };
        self.frame(&mut out);
        self.post_process(&mut out);
        self.checked(out)
    }

    /// helps.EndApplyPatchStream: the translator's tool-input finalization when the
    /// upstream transport ends, before any synthetic `[DONE]`. Go writes these chunks
    /// without the usage and token post-processing of translated chunks.
    fn finalize(&mut self) -> Emit {
        let mut out = self.translator.finalize_tool_input();
        self.frame(&mut out);
        self.checked(out)
    }

    /// End of stream: frames the client side still holds (the handler's final flush).
    fn finish(&mut self) -> Emit {
        let mut out = match self.translator.finish() {
            Ok(out) => out,
            Err(_) => {
                return Emit {
                    out: Vec::new(),
                    stop: Some(bad_gateway()),
                };
            }
        };
        self.frame(&mut out);
        self.post_process(&mut out);
        self.checked(out)
    }

    /// `responsesSSEFramer.Flush` before a terminal error.
    pub(crate) fn flush_frames(&mut self) -> Vec<Bytes> {
        let mut out = self.translator.flush_frames();
        self.frame(&mut out);
        self.post_process(&mut out);
        out
    }

    fn checked(&self, out: Vec<Bytes>) -> Emit {
        Emit {
            out,
            stop: self.translator.tool_input_failed().then(bad_gateway),
        }
    }

    /// Gemini clients: translators frame chunks as SSE `data:` events; with `alt` the Go
    /// handler writes the chunk itself.
    fn frame(&self, out: &mut Vec<Bytes>) {
        if self.client == Format::Gemini && !self.executor_chunks {
            for chunk in out.iter_mut() {
                let bare = chunk
                    .strip_prefix(b"data: ")
                    .and_then(|c| c.strip_suffix(b"\n\n"))
                    .map_or_else(|| chunk.clone(), Bytes::copy_from_slice);
                *chunk = if self.raw {
                    bare
                } else {
                    let mut framed = Vec::with_capacity(bare.len() + 8);
                    framed.extend_from_slice(b"data: ");
                    framed.extend_from_slice(&bare);
                    framed.extend_from_slice(b"\n\n");
                    Bytes::from(framed)
                };
            }
        }
        out.retain(|c| !c.is_empty());
    }

    /// Responses usage details and the Claude input-token estimate on translated chunks.
    fn post_process(&mut self, out: &mut [Bytes]) {
        if self.client == Format::OpenAIResponse {
            for chunk in out.iter_mut() {
                *chunk = Bytes::from(crate::openai_compat_payload::ensure_responses_usage_details(chunk));
            }
        }
        self.claude.apply(out);
    }
}

/// One scanned upstream line in, client items out; `end` runs once at EOF or before a
/// scanner error is reported.
pub(crate) trait LineState: Send + 'static {
    fn line(&mut self, line: &[u8]) -> Emit;
    fn end(&mut self) -> Emit;
    /// `end` before a scanner error is reported; executors that skip the synthetic
    /// terminal after a read error override it.
    fn end_after_error(&mut self) -> Emit {
        self.end()
    }
    /// Client frames still pending when a terminal error is written.
    fn flush(&mut self) -> Vec<Bytes>;
}

/// Runs `state` over `lines`. A stop is terminal; a scanner error is reported after the
/// end-of-stream items, like Go's `scanner.Err()` check. Pending client frames are
/// flushed before any terminal error (Go's Responses handler flushes its framer before
/// writing the error).
pub(crate) fn drive<S: LineState>(lines: ExecStream, state: S) -> ExecStream {
    futures_util::stream::unfold(
        (lines, state, VecDeque::<Result<Bytes, ExecError>>::new(), false),
        |(mut lines, mut state, mut ready, mut ended)| async move {
            loop {
                if let Some(item) = ready.pop_front() {
                    if item.is_err() {
                        ready.clear();
                        ended = true;
                    }
                    return Some((item, (lines, state, ready, ended)));
                }
                if ended {
                    return None;
                }
                let scanned = lines.next().await;
                let emit = match &scanned {
                    Some(Ok(line)) => state.line(line),
                    Some(Err(_)) => {
                        ended = true;
                        state.end_after_error()
                    }
                    None => {
                        ended = true;
                        state.end()
                    }
                };
                ready.extend(emit.out.into_iter().map(Ok));
                let error = match (emit.stop, scanned) {
                    (Some(error), _) | (None, Some(Err(error))) => error,
                    _ => continue,
                };
                ready.extend(state.flush().into_iter().map(Ok));
                ready.push_back(Err(error));
            }
        },
    )
    .boxed()
}

/// The streamGenerateContent loop of Gemini and Antigravity upstreams: usage filtering
/// and the JSON payload of each line through the translator; at EOF the tool-input
/// finalization, then `[DONE]`. Antigravity lines come through [`antigravity_payloads`],
/// already filtered.
struct GeminiLines {
    output: Output,
    upstream: Format,
}

impl LineState for GeminiLines {
    fn line(&mut self, line: &[u8]) -> Emit {
        if self.upstream != Format::Antigravity {
            let filtered = sse::filter_sse_usage_metadata(line);
            // Go observes the response model on the raw line and usage on the filtered
            // payload; the filter only drops non-terminal usageMetadata, which the model
            // reader ignores.
            self.output.usage.response_line(self.upstream, &filtered);
            return match sse::json_payload(&filtered) {
                Some(payload) => self.output.translate(payload),
                None => Emit::default(),
            };
        }
        // A payload joined from several lines; its usage is not observed (Go reads usage
        // from each scanned line, and no part of a split payload parses alone).
        let assembled = line.contains(&b'\n');
        if !assembled {
            self.output.usage.response_line(self.upstream, line);
        }
        let Some(payload) = sse::json_payload(line) else {
            return Emit::default();
        };
        // Go v8.0.27 (4e4dede4): an `error` object ends the stream with its code (502
        // outside 400 to 599) and the payload as the message.
        let error = gj::get(payload, "error");
        if error.exists() {
            let code = error.get("code").int();
            let status = u16::try_from(code)
                .ok()
                .filter(|c| (400..=599).contains(c))
                .unwrap_or(502);
            let error = crate::antigravity::upstream_error(status, Bytes::copy_from_slice(payload));
            self.output
                .capture
                .record(CaptureEvent::ResponseError(&String::from_utf8_lossy(&error.body)));
            return Emit {
                out: Vec::new(),
                stop: Some(error),
            };
        }
        if assembled {
            self.output.usage.response_model_line(payload);
        }
        self.output.translate(payload)
    }

    fn end(&mut self) -> Emit {
        self.output.end_with_done()
    }

    /// Gemini translates `[DONE]` before checking the scanner; Antigravity finalizes
    /// tool input only, so a truncated stream never reads as a completed one.
    fn end_after_error(&mut self) -> Emit {
        if self.upstream == Format::Antigravity {
            self.output.finalize()
        } else {
            self.output.end_with_done()
        }
    }

    fn flush(&mut self) -> Vec<Bytes> {
        self.output.flush_frames()
    }
}

/// [`GeminiLines`] over `lines` for an `upstream` (Gemini or Antigravity) stream.
pub(crate) fn gemini_lines(lines: ExecStream, output: Output, upstream: Format) -> ExecStream {
    drive(lines, GeminiLines { output, upstream })
}

/// The Antigravity stream loop's line handling before a payload is used (Go v8.0.27,
/// 4e4dede4): each scanned line with its non-terminal usage filtered, and a JSON payload
/// split over several lines joined (each later line trimmed and without `data:`,
/// newline-separated) until the joined text parses. A joined payload is one item with
/// newlines in it, which a scanned line never has. Lines that end no pending payload
/// are not validated again, so joining stays linear in the payload's size.
pub(crate) fn antigravity_payloads(lines: ExecStream) -> ExecStream {
    lines
        .scan(Vec::<u8>::new(), |pending, item| {
            let line = match item {
                Ok(line) => line,
                Err(error) => return std::future::ready(Some(Some(Err(error)))),
            };
            let filtered = sse::filter_sse_usage_metadata(&line);
            let out = if pending.is_empty() {
                match sse::json_payload(&filtered) {
                    Some(payload) if !gj::valid(payload) => {
                        *pending = payload.to_vec();
                        None
                    }
                    _ => Some(Bytes::from(filtered)),
                }
            } else {
                let mut piece = cpa_common::gostr::trim_space(&filtered);
                if let Some(rest) = piece.strip_prefix(b"data:") {
                    piece = cpa_common::gostr::trim_space(rest);
                }
                if !piece.is_empty() {
                    pending.push(b'\n');
                    pending.extend_from_slice(piece);
                }
                // The pending text starts with `{` and its pieces are trimmed, so it can
                // only become valid with a piece that ends in `}`.
                (piece.ends_with(b"}") && gj::valid(pending)).then(|| Bytes::from(std::mem::take(pending)))
            };
            std::future::ready(Some(out.map(Ok)))
        })
        .filter_map(std::future::ready)
        .boxed()
}

/// The terminator executors append to a scanned line to pass it as one translator
/// event. The translators split events like bufio.ScanLines, which drops one `\r`
/// before each `\n`, so `\r\n` hands them the line exactly as Go's scanner returned it
/// (including a trailing `\r` of its own).
pub(crate) const LINE_END: &[u8] = b"\r\n";

pub(crate) fn line_event(line: &[u8]) -> Vec<u8> {
    [line, LINE_END].concat()
}

/// The Interactions stream loop. The translator groups the lines into SSE frames at
/// blank lines (`stream::Framed`): Interactions clients get each frame as sent, others
/// its translated payload. At EOF the pending frame is emitted (Go's final emitFrame),
/// then the tool-input finalization. With usage reporting on, the loop also keeps Go's
/// frame so each frame's payload reaches the reporter (Go's emitFrame observation).
struct InteractionsLines {
    output: Output,
    frame: Option<Vec<u8>>,
}

impl InteractionsLines {
    /// emitFrame's `ObserveResponseModel(payload)` and `ParseInteractionsStreamUsage`.
    fn report_frame(&mut self) {
        if let Some(frame) = &mut self.frame {
            let payload = cpa_translate::stream::interactions_frame_payload(frame);
            frame.clear();
            if !payload.is_empty() {
                self.output.usage.response_line(Format::Interactions, &payload);
            }
        }
    }
}

impl LineState for InteractionsLines {
    fn line(&mut self, line: &[u8]) -> Emit {
        if self.frame.is_some() {
            if line.trim_ascii().is_empty() {
                self.report_frame();
            } else if let Some(frame) = &mut self.frame {
                if !frame.is_empty() {
                    frame.push(b'\n');
                }
                frame.extend_from_slice(line);
            }
        }
        self.output.translate(&line_event(line))
    }

    fn end(&mut self) -> Emit {
        self.report_frame();
        let mut emit = self.output.translate(b"\n");
        if emit.stop.is_none() && !emit.then(self.output.finalize()) {
            emit.then(self.output.finish());
        }
        emit
    }

    fn flush(&mut self) -> Vec<Bytes> {
        self.output.flush_frames()
    }
}

fn interactions_lines(lines: ExecStream, output: Output) -> ExecStream {
    let frame = output.usage.enabled().then(Vec::new);
    drive(lines, InteractionsLines { output, frame })
}

#[cfg(test)]
#[path = "gemini_tests.rs"]
pub(crate) mod tests;
