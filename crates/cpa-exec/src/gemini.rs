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
use cpa_common::gostr::trim_space;
use cpa_common::json as gj;
use cpa_common::thinking::{self, RequestThinking, parse_suffix};
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, ExecStream, FailureScope, Operation, ResponseBody};
use cpa_core::format::Format;
use cpa_translate::{RequestCtx, ResponseCtx, StreamTranslator};
use futures_util::StreamExt;

use crate::gemini_payload::{self as payload, Resolved};
use crate::gemini_stream::{self as sse, ClaudeInputTokens};
use crate::proxy::{self, GoClients, GoHeaders, Hooks, Proxy, Upstream};

/// Providers this executor serves (`NewGeminiExecutor`, `NewGeminiInteractionsExecutor`).
pub const PROVIDERS: [&str; 2] = ["gemini", "gemini-interactions"];

pub fn handles(provider: &str) -> bool {
    PROVIDERS.contains(&provider)
}

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";
const API_VERSION: &str = "v1beta";
/// `streamScannerBuffer`.
const MAX_LINE: usize = 52_428_800;
/// `geminiInteractionsAPIRevision`.
const INTERACTIONS_REVISION: &str = "2026-05-20";
const COMPACT_ALT: &str = "responses/compact";
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
fn status_err(status: u16, message: impl Into<Bytes>) -> ExecError {
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
    }
}

fn bad_gateway() -> ExecError {
    status_err(502, EMPTY_TRANSLATION)
}

fn not_registered(what: &str, from: Format, to: Format) -> ExecError {
    ExecError::local(
        501,
        FailureScope::Request,
        format!(
            "{what} translation {} -> {} is not registered",
            from.as_str(),
            to.as_str()
        ),
    )
}

/// Fails before any upstream call when a translation the attempt needs is not ported.
fn require_pairs(source: Format, upstream: Format, response: Format) -> Result<(), ExecError> {
    if source != upstream && cpa_translate::pair(source, upstream).is_none() {
        return Err(not_registered("request", source, upstream));
    }
    if response != upstream && cpa_translate::pair(response, upstream).is_none() {
        return Err(not_registered("response", response, upstream));
    }
    Ok(())
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
fn alt(req: &ExecRequest) -> Option<&str> {
    req.alt.as_deref().filter(|a| !a.is_empty())
}

/// `helps.ApplyPatchOriginalRequest`.
fn original_request(req: &ExecRequest) -> &Bytes {
    if req.original_body.is_empty() {
        &req.body
    } else {
        &req.original_body
    }
}

/// `TranslateRequestWithAPIKeyModelCompatibility` for one payload: Codex clients' tool
/// integer types first, then the registered pair, or for an is-compat model the
/// `...WithCompat` translator between Go's summary extraction and application.
// ponytail: ConvertClaudeRequestToInteractionsWithCompat is not in cpa_translate yet, so
// is-compat Claude clients of an Interactions key use the regular pair (translator
// thread); the Codex multi-agent v2 input rewrites are cpa_common::codex_client's (Codex
// thread) and not applied.
fn translate(
    req: &ExecRequest,
    target: Format,
    model: &str,
    body: &[u8],
    stream: bool,
    compat: bool,
) -> Result<Vec<u8>, ExecError> {
    let error = |e: cpa_translate::Error| ExecError::local(400, FailureScope::Request, e.to_string());
    let source = req.source_format;
    let normalized;
    let body = if cpa_common::payload::is_codex_user_agent(&req.headers) {
        normalized = cpa_common::payload::normalize_codex_tool_integer_types(body, &req.headers);
        normalized.as_slice()
    } else {
        body
    };
    let ctx = RequestCtx { model, stream };
    let compat_translator: Option<cpa_translate::RequestFn> = match (source, target) {
        (Format::Claude, Format::Gemini) if compat => Some(cpa_translate::claude_to_gemini_with_compat),
        _ => None,
    };
    let Some(convert) = compat_translator else {
        return cpa_translate::translate_request(source, target, &ctx, body).map_err(error);
    };
    use cpa_common::thinking::{apply_summary_config_for_model, extract_translated_summary_config};
    let summary = extract_translated_summary_config(body, source.as_str(), target.as_str());
    let translated = deep_stack(body, || convert(&ctx, body)).map_err(error)?;
    Ok(apply_summary_config_for_model(
        &translated,
        target.as_str(),
        model,
        summary,
    ))
}

/// Runs a translator on a thread with Go's maximum goroutine stack when the body nests
/// deeply, as `cpa_translate::translate_request` does for registered pairs: the ported
/// schema walkers recurse per nesting level like Go's, whose stacks grow.
// ponytail: duplicate of cpa_translate's private guard until its exported `...WithCompat`
// entry points guard themselves (owner: translator thread).
fn deep_stack<T: Send>(body: &[u8], f: impl FnOnce() -> T + Send) -> T {
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
    target: Format,
    model: &str,
    compat: bool,
) -> Result<(Vec<u8>, Vec<u8>), ExecError> {
    let working = translate(req, target, model, &req.body, req.stream, compat)?;
    let source = original_request(req);
    if *source == req.body {
        return Ok((working.clone(), working));
    }
    let original = translate(req, target, model, source, req.stream, compat)?;
    Ok((original, working))
}

/// `helps.ApplyRequestThinking`.
fn apply_thinking(
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

/// `helps.ApplyPayloadConfigWithRequest`: config payload rules on the final body, with
/// the translated original request as the baseline for `default` rules.
fn apply_payload_rules(
    rules: &cpa_common::payload::Rules,
    model: &str,
    protocol: &str,
    body: Vec<u8>,
    original_translated: &[u8],
    req: &ExecRequest,
) -> Vec<u8> {
    let requested = match req.requested_model.trim() {
        "" => req.model.trim(),
        requested => requested,
    };
    cpa_common::payload::apply(
        rules,
        &cpa_common::payload::Request {
            target_executor: "",
            model,
            requested_model: requested,
            protocol,
            from_protocol: req.source_format.as_str(),
            root: "",
            original: original_translated,
            // Gemini routes are never the Images API, the only path payload rules read.
            request_path: "",
            headers: Some(&req.headers),
        },
        body,
    )
}

/// Content-Type, the API key and the credential's custom headers (`applyGeminiHeaders`).
fn request_headers(credential: &Credential, req: &ExecRequest) -> GoHeaders {
    let mut headers = GoHeaders::new();
    headers.set("Content-Type", "application/json");
    let key = api_key(credential);
    if !key.is_empty() {
        headers.set("x-goog-api-key", key);
    }
    // `$CPA-SESSION-ID` is the attempt's canonical session: Go's conductor binds it to the
    // context (ensureCanonicalSessionMetadata + syncMetadataSessionToContext) before the
    // executor runs, and EnsureSessionContext keeps it. Dispatch puts the same bound
    // identity, derived and message-hash fallbacks included, in `req.session`.
    for (name, value) in
        cpa_common::headers::custom_headers(&credential.attributes, &req.headers, req.session.as_deref())
    {
        headers.set(&name, value);
    }
    headers
}

/// The resolved API-key model for this attempt (Go conductor binding).
fn resolved(credential: &Credential, cfg: &Config, req: &ExecRequest) -> Option<Resolved> {
    let (family, model_type) = match credential.provider.as_str() {
        "gemini-interactions" => ("interactions", "interactions"),
        _ => ("gemini", "gemini"),
    };
    payload::resolved_model(credential, cfg, family, model_type, &req.requested_model, &req.model)
}

async fn error_from(upstream: Upstream) -> ExecError {
    let status = upstream.status;
    let body = proxy::read_all(upstream.body, proxy::MAX_ERROR_BODY, true)
        .await
        .unwrap_or_default();
    status_err(status, body)
}

/// `io.ReadAll` of the whole response, then Go's status check (countTokens and
/// Interactions read before checking).
async fn read_then_check(upstream: Upstream) -> Result<(http::HeaderMap, Bytes), ExecError> {
    let status = upstream.status;
    let headers = upstream.headers.clone();
    let ok = (200..300).contains(&status);
    let limit = if ok { usize::MAX } else { proxy::MAX_ERROR_BODY };
    let data = proxy::read_all(upstream.body, limit, false).await?;
    if !ok {
        return Err(status_err(status, data));
    }
    Ok((headers, data))
}

/// `TranslateNonStream`: the registered transform, or the body itself for the
/// passthrough pairs (gemini->gemini, interactions->interactions); then Go's
/// empty-output check and Responses usage details.
fn translate_non_stream(
    req: &ExecRequest,
    upstream: Format,
    translated: &[u8],
    data: &[u8],
) -> Result<Bytes, ExecError> {
    let response = req.response_format;
    let mut out = match cpa_translate::pair(response, upstream) {
        Some(pair) => (pair.non_stream)(
            &ResponseCtx {
                model: &req.model,
                original_request: original_request(req),
                translated_request: translated,
            },
            data,
        )
        .map_err(|_| bad_gateway())?,
        None => data.to_vec(),
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
        require_pairs(from, to, req.response_format)?;
        let resolved = resolved(credential, cfg, req);
        let compat = resolved.as_ref().is_some_and(|r| r.is_compat);
        let (original_translated, body) = translate_pair(req, to, &base_model, compat)?;
        let mut body = apply_thinking(req, body, from, to, &credential.provider, resolved.as_ref())?;
        body = payload::fix_image_aspect_ratio(&base_model, body);
        let rules = cpa_common::payload::Rules::from_config(cfg);
        body = apply_payload_rules(&rules, &base_model, to.as_str(), body, &original_translated, req);
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
        let headers = request_headers(credential, req);
        let upstream = proxy::send(&self.client(credential, cfg), &url, headers, body.clone(), None).await?;
        if !(200..300).contains(&upstream.status) {
            return Err(error_from(upstream).await);
        }
        let response_headers = upstream.headers.clone();
        if !req.stream {
            let data = proxy::read_all(upstream.body, usize::MAX, false).await?;
            let out = translate_non_stream(req, to, &body, &data)?;
            return Ok(ExecResponse {
                status: 200,
                headers: response_headers,
                body: ResponseBody::Buffered(out),
            });
        }
        let output = Output::new(req, to, &body);
        let stream = gemini_lines(proxy::lines(upstream.body, MAX_LINE), output);
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
        require_pairs(from, to, req.response_format)?;
        let resolved = resolved(credential, cfg, req);
        let compat = resolved.as_ref().is_some_and(|r| r.is_compat);
        // Interactions clients are sent as they are, without the registry normalizer.
        let (original_translated, mut body) = if from == Format::Interactions {
            (original_request(req).to_vec(), req.body.to_vec())
        } else {
            translate_pair(req, to, &target, compat)?
        };
        if gj::get(&body, "model").exists() && !target.is_empty() {
            body = payload::set_str_if_different(body, "model", &target);
        }
        // applyGeminiInteractionsThinking: the Gemini applier family for the target.
        body = apply_thinking(req, body, from, to, "gemini", resolved.as_ref())?;
        let rules = cpa_common::payload::Rules::from_config(cfg);
        body = apply_payload_rules(&rules, &target, "interactions", body, &original_translated, req);
        body = payload::sanitize_interactions_input_ids(body);
        if req.stream {
            body = payload::set_bool_if_different(body, "stream", true);
        }
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
        let upstream = proxy::send(&self.client(credential, cfg), &url, headers, body.clone(), None).await?;
        if !req.stream {
            let (response_headers, data) = read_then_check(upstream).await?;
            let out = translate_non_stream(req, to, &body, &data)?;
            return Ok(ExecResponse {
                status: 200,
                headers: response_headers,
                body: ResponseBody::Buffered(out),
            });
        }
        if !(200..300).contains(&upstream.status) {
            return Err(error_from(upstream).await);
        }
        let response_headers = upstream.headers.clone();
        let output = Output::new(req, to, &body);
        let stream = interactions_frames(proxy::lines(upstream.body, MAX_LINE), output);
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
        require_pairs(from, to, req.response_format)?;
        let resolved = resolved(credential, cfg, req);
        let compat = resolved.as_ref().is_some_and(|r| r.is_compat);
        let body = translate(req, to, &base_model, &req.body, false, compat)?;
        let mut body = apply_thinking(req, body, from, to, &credential.provider, resolved.as_ref())?;
        body = payload::fix_image_aspect_ratio(&base_model, body);
        for path in ["tools", "generationConfig", "safetySettings"] {
            body = payload::delete(body, path);
        }
        body = payload::set_str_if_different(body, "model", &base_model);
        body = cpa_common::signature::sanitize_gemini_request_thought_signatures(&body, "contents");
        body = payload::ensure_leading_user_content(body, "contents");
        let url = format!("{}/{API_VERSION}/models/{base_model}:countTokens", base_url(credential));
        let headers = request_headers(credential, req);
        let upstream = proxy::send(&self.client(credential, cfg), &url, headers, body, None).await?;
        let (response_headers, data) = read_then_check(upstream).await?;
        let count = gj::get(&data, "totalTokens").int();
        let out = cpa_translate::translate_token_count(req.response_format, to, count, &data);
        Ok(ExecResponse {
            status: 200,
            headers: response_headers,
            body: ResponseBody::Buffered(Bytes::from(out)),
        })
    }
}

/// Turns Go stream chunks into the bytes the client's route handler writes.
struct Output {
    /// `None` for the passthrough pairs (gemini->gemini, interactions->interactions).
    translator: Option<Box<dyn StreamTranslator>>,
    client: Format,
    /// A Gemini client asked for `alt`: chunks are written bare.
    raw: bool,
    claude: ClaudeInputTokens,
}

impl Output {
    fn new(req: &ExecRequest, upstream: Format, translated: &[u8]) -> Self {
        let client = req.response_format;
        let translator = cpa_translate::pair(client, upstream).map(|pair| {
            (pair.stream)(&ResponseCtx {
                model: &req.model,
                original_request: original_request(req),
                translated_request: translated,
            })
        });
        Self {
            translator,
            client,
            raw: alt(req).is_some(),
            claude: ClaudeInputTokens::new(req.source_format, upstream, client, original_request(req).clone()),
        }
    }

    /// One Go `TranslateStream` call with `payload`, framed for the client.
    fn translate(&mut self, payload: &[u8]) -> Result<Vec<Bytes>, ExecError> {
        let mut out = match &mut self.translator {
            Some(translator) => translator.event(payload).map_err(|_| bad_gateway())?,
            // PassthroughGeminiResponseStream: the payload itself, `[DONE]` dropped.
            None if payload == b"[DONE]" => Vec::new(),
            None => vec![Bytes::copy_from_slice(payload)],
        };
        self.client_bytes(&mut out);
        Ok(out)
    }

    /// End of stream: the translator's closing events.
    fn finish(&mut self) -> Result<Vec<Bytes>, ExecError> {
        let mut out = match &mut self.translator {
            Some(translator) => translator.finish().map_err(|_| bad_gateway())?,
            None => Vec::new(),
        };
        self.client_bytes(&mut out);
        Ok(out)
    }

    /// `responsesSSEFramer.Flush` before a terminal error.
    fn flush_frames(&mut self) -> Vec<Bytes> {
        let mut out = match &mut self.translator {
            Some(translator) => translator.flush_frames(),
            None => Vec::new(),
        };
        self.client_bytes(&mut out);
        out
    }

    fn client_bytes(&mut self, out: &mut Vec<Bytes>) {
        match self.client {
            Format::Gemini => {
                for chunk in out.iter_mut() {
                    // Translators frame Gemini chunks as SSE `data:` events; with `alt` the
                    // Go handler writes the chunk itself.
                    let bare = if self.translator.is_some() {
                        chunk
                            .strip_prefix(b"data: ")
                            .and_then(|c| c.strip_suffix(b"\n\n"))
                            .map_or_else(|| chunk.clone(), Bytes::copy_from_slice)
                    } else {
                        chunk.clone()
                    };
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
            Format::OpenAIResponse => {
                for chunk in out.iter_mut() {
                    *chunk = Bytes::from(crate::openai_compat_payload::ensure_responses_usage_details(chunk));
                }
            }
            _ => {}
        }
        out.retain(|c| !c.is_empty());
        self.claude.apply(out);
    }
}

/// One scanned upstream line in, client items out; `end` runs once at EOF or before a
/// scanner error is reported.
trait LineState: Send + 'static {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Bytes>, ExecError>;
    fn end(&mut self) -> Result<Vec<Bytes>, ExecError>;
    /// Client frames still pending when a terminal error is written.
    fn flush(&mut self) -> Vec<Bytes>;
}

/// Runs `state` over `lines`. A translation error is terminal; a scanner error is
/// reported after the end-of-stream items, like Go's `scanner.Err()` check. Pending
/// client frames are flushed before any terminal error (Go's Responses handler flushes
/// its framer before writing the error).
fn drive<S: LineState>(lines: ExecStream, state: S) -> ExecStream {
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
                let result = match &scanned {
                    Some(Ok(line)) => state.line(line),
                    Some(Err(_)) | None => {
                        ended = true;
                        state.end()
                    }
                };
                match result {
                    Ok(out) => ready.extend(out.into_iter().map(Ok)),
                    Err(error) => {
                        ready.extend(state.flush().into_iter().map(Ok));
                        ready.push_back(Err(error));
                        continue;
                    }
                }
                if let Some(Err(error)) = scanned {
                    ready.extend(state.flush().into_iter().map(Ok));
                    ready.push_back(Err(error));
                }
            }
        },
    )
    .boxed()
}

/// The streamGenerateContent loop: usage filtering, the JSON payload of each line, and
/// a final `[DONE]` through the translator.
struct GeminiLines(Output);

impl LineState for GeminiLines {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Bytes>, ExecError> {
        let filtered = sse::filter_sse_usage_metadata(line);
        match sse::json_payload(&filtered) {
            Some(payload) => self.0.translate(payload),
            None => Ok(Vec::new()),
        }
    }

    fn end(&mut self) -> Result<Vec<Bytes>, ExecError> {
        let mut out = self.0.translate(b"[DONE]")?;
        out.extend(self.0.finish()?);
        Ok(out)
    }

    fn flush(&mut self) -> Vec<Bytes> {
        self.0.flush_frames()
    }
}

fn gemini_lines(lines: ExecStream, output: Output) -> ExecStream {
    drive(lines, GeminiLines(output))
}

/// The Interactions stream loop: lines grouped into SSE frames at blank lines.
/// Interactions clients get each frame as sent; others get its translated payload.
struct InteractionsFrames {
    output: Output,
    frame: Vec<u8>,
}

impl InteractionsFrames {
    fn emit(&mut self) -> Result<Vec<Bytes>, ExecError> {
        let raw = std::mem::take(&mut self.frame);
        if trim_space(&raw).is_empty() {
            return Ok(Vec::new());
        }
        let mut payload = sse::interactions_sse_payload(&raw);
        if payload.is_none() && sse::interactions_sse_done(&raw) {
            payload = Some(b"[DONE]".to_vec());
        }
        if self.output.client == Format::Interactions {
            let end = raw
                .iter()
                .rposition(|b| !matches!(b, b'\r' | b'\n'))
                .map_or(0, |i| i + 1);
            let mut visible = raw[..end].to_vec();
            visible.extend_from_slice(b"\n\n");
            return Ok(cpa_translate::stream::frame(Format::Interactions, &visible)
                .map(Bytes::from)
                .into_iter()
                .collect());
        }
        match payload {
            Some(payload) => self.output.translate(&payload),
            None => Ok(Vec::new()),
        }
    }
}

impl LineState for InteractionsFrames {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Bytes>, ExecError> {
        if trim_space(line).is_empty() {
            return self.emit();
        }
        if !self.frame.is_empty() {
            self.frame.push(b'\n');
        }
        self.frame.extend_from_slice(line);
        Ok(Vec::new())
    }

    fn end(&mut self) -> Result<Vec<Bytes>, ExecError> {
        let mut out = self.emit()?;
        out.extend(self.output.finish()?);
        Ok(out)
    }

    fn flush(&mut self) -> Vec<Bytes> {
        self.output.flush_frames()
    }
}

fn interactions_frames(lines: ExecStream, output: Output) -> ExecStream {
    drive(
        lines,
        InteractionsFrames {
            output,
            frame: Vec::new(),
        },
    )
}

#[cfg(test)]
#[path = "gemini_tests.rs"]
mod tests;
