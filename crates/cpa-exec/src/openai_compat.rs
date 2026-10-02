//! OpenAI-compatible upstreams (internal/runtime/executor/openai_compat_executor.go):
//! configured providers reached with a bearer API key at `base_url`, speaking Chat
//! Completions, Responses compact and the Images API.

use std::collections::VecDeque;
use std::time::SystemTime;

use bytes::Bytes;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, ExecStream, FailureScope, Operation, ResponseBody};
use cpa_core::format::Format;
use cpa_translate::{RequestCtx, ResponseCtx, StreamTranslator};
use futures_util::StreamExt;

use cpa_common::gostr::GoStr;
use cpa_common::json as gj;
use cpa_common::thinking::{self, ModelCaps, RequestThinking, parse_suffix};

use crate::openai_compat_go as go;
use crate::openai_compat_http::{self as wire, Clients, GoHeaders};
use crate::openai_compat_multipart as multipart;
use crate::openai_compat_payload as payload;

pub const USER_AGENT: &str = "cli-proxy-openai-compat";
const COMPACT_ALT: &str = "responses/compact";
const IMAGES_GENERATIONS: &str = "/images/generations";
const IMAGES_EDITS: &str = "/images/edits";
/// `scanner.Buffer(nil, 52_428_800)`.
const MAX_LINE: usize = 52_428_800;
/// helps.ApplyPatchUpstreamErrorMessage: Go's error for an empty translated response.
const EMPTY_TRANSLATION: &str = "Invalid apply_patch tool arguments received from upstream.";

/// Providers this executor serves: `openai-compatibility` and `openai-compatible-<name>`
/// (util.OpenAICompatibleProviderKey).
pub fn handles(provider: &str) -> bool {
    provider == "openai-compatibility" || provider.starts_with("openai-compatible-")
}

#[derive(Default)]
pub struct OpenAICompatExecutor {
    clients: Clients,
}

/// Same status-only classification as the other API-key executors.
pub(crate) fn scope_for(status: u16) -> FailureScope {
    match status {
        401 | 402 | 403 | 408 | 429 | 500.. => FailureScope::Credential,
        _ => FailureScope::Request,
    }
}

/// `statusErr{code, msg}`: an empty message reads `status N`.
pub(crate) fn status_err(status: u16, message: impl Into<String>) -> ExecError {
    let message = message.into();
    let message = if message.is_empty() {
        format!("status {status}")
    } else {
        message
    };
    ExecError::local(status, scope_for(status), message)
}

/// `newOpenAICompatStatusError`, or the plain `statusErr` the image stream path uses.
async fn upstream_error(upstream: wire::Upstream, with_retry: bool) -> ExecError {
    let (status, headers, body) = wire::read_error_body(upstream).await;
    let mut error = status_err(status, String::from_utf8_lossy(&body));
    if with_retry {
        error.retry_after = payload::retry_after(status, &headers, &body, SystemTime::now());
    }
    error.headers = Box::new(headers);
    error
}

fn credentials(credential: &Credential) -> (String, String) {
    let attr = |k: &str| {
        credential
            .attributes
            .get(k)
            .map(|v| v.trim().to_owned())
            .unwrap_or_default()
    };
    (attr("base_url"), attr("api_key"))
}

fn missing_base_url() -> ExecError {
    ExecError::local(401, FailureScope::Credential, "missing provider baseURL")
}

/// `strings.TrimSuffix(baseURL, "/") + path`: one slash only, like Go.
fn endpoint(base_url: &str, path: &str) -> String {
    format!("{}{path}", base_url.strip_suffix('/').unwrap_or(base_url))
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

/// `helps.TranslateRequestWithAPIKeyModelCompatibilityAndUpdateIntent` for this executor.
// ponytail: Go switches to the compat translators (ConvertClaudeRequestToOpenAIWithCompat)
// when the resolved model sets `is-compat`; cpa-translate has no compat variant for
// OpenAI targets yet (translator thread). The Codex multi-agent v2 rewrites and their
// configuration-update intent (owner: Codex thread) are not applied, so the intent is
// always false.
fn translate_request(req: &ExecRequest, target: Format, model: &str, stream: bool) -> Result<Vec<u8>, ExecError> {
    cpa_translate::translate_request(req.source_format, target, &RequestCtx { model, stream }, &req.body)
        .map_err(|e| ExecError::local(400, FailureScope::Request, e.to_string()))
}

/// `helps.ApplyRequestThinking` with the capabilities bound to this attempt.
fn apply_thinking(
    body: Vec<u8>,
    req: &ExecRequest,
    target: Format,
    provider: &str,
    resolved: Option<&ModelCaps>,
) -> Result<Vec<u8>, ExecError> {
    thinking::apply_request_thinking(&RequestThinking {
        body: &body,
        payload: &req.body,
        original: &req.original_body,
        model: &req.model,
        from: req.source_format.as_str(),
        to: target.as_str(),
        provider,
        resolved: resolved.map(Some),
        has_request_transformer: cpa_translate::pair(req.source_format, target).is_some(),
        updates_changed: false,
    })
    .map_err(|e| ExecError::local(400, FailureScope::Request, e.message))
}

fn base_headers(api_key: &str, content_type: &str) -> GoHeaders {
    let mut headers = GoHeaders::new();
    headers.set("Content-Type", content_type);
    if !api_key.is_empty() {
        headers.set("Authorization", format!("Bearer {api_key}"));
    }
    headers.set("User-Agent", USER_AGENT);
    headers
}

fn apply_custom(headers: &mut GoHeaders, credential: &Credential, req: &ExecRequest) {
    for (name, value) in wire::custom_headers(credential, &req.headers, req.session.as_deref()) {
        headers.set(&name, value);
    }
}

impl OpenAICompatExecutor {
    /// Uses `client` for credentials without a proxy (tests pass a plain client).
    pub fn with_client(client: wreq::Client) -> Self {
        Self {
            clients: Clients::new(client),
        }
    }

    pub async fn execute(
        &self,
        credential: &Credential,
        req: ExecRequest,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        match req.operation {
            Operation::CountTokens => count_tokens(credential, &req, cfg),
            Operation::Generate => self.chat(credential, req, cfg).await,
        }
    }

    /// Execute (`stream` false) and ExecuteStream (`stream` true) for chat and compact.
    async fn chat(&self, credential: &Credential, req: ExecRequest, cfg: &Config) -> Result<ExecResponse, ExecError> {
        let base_model = parse_suffix(&req.model).model_name;
        let (base_url, api_key) = credentials(credential);
        if base_url.is_empty() {
            return Err(missing_base_url());
        }
        let compact = req.alt.as_deref() == Some(COMPACT_ALT);
        // ExecuteStream always targets chat completions, even with the compact alt.
        let (target, path) = if compact && !req.stream {
            (Format::OpenAIResponse, "/responses/compact")
        } else {
            (Format::OpenAI, "/chat/completions")
        };
        let response_pair = cpa_translate::pair(req.response_format, target);
        if response_pair.is_none() && req.response_format != target {
            return Err(not_registered("response", req.response_format, target));
        }
        let compat = payload::resolve_compat(credential, cfg);
        let resolved = payload::resolved_model(compat.as_ref(), credential, route_model(&req), &req.model);
        let mut body = translate_request(&req, target, &base_model, req.stream)?;
        body = apply_thinking(body, &req, target, &credential.provider, resolved.as_ref())?;
        body = wire::apply_payload_rules(body, cfg);
        let requested = route_model(&req);
        if payload::excludes_images(compat.as_ref(), &base_model, requested) {
            body = payload::normalize_tool_results_text_only(body);
        }
        if !compact {
            let mct = payload::uses_max_completion_tokens(compat.as_ref(), &base_model, requested);
            body = payload::normalize_max_tokens(body, mct);
            body = prompt_cache_key(compat.as_ref(), &credential.provider, &req, &base_model, body);
        }
        if compact && !req.stream {
            gj::delete(&mut body, "stream");
            body = payload::sanitize_reasoning_encrypted_content(body);
        }
        if req.stream {
            payload::set_bool_if_different(&mut body, "stream_options.include_usage", true);
        }
        let mut headers = base_headers(&api_key, "application/json");
        apply_custom(&mut headers, credential, &req);
        if req.stream {
            headers.set("Accept", "text/event-stream");
            headers.set("Cache-Control", "no-cache");
        }
        let client = self.clients.for_credential(credential, cfg);
        let upstream = wire::send(&client, &endpoint(&base_url, path), headers, Bytes::from(body.clone())).await?;
        if !(200..300).contains(&upstream.status) {
            return Err(upstream_error(upstream, true).await);
        }
        let response_headers = upstream.headers.clone();
        let original = if req.original_body.is_empty() {
            req.body.clone()
        } else {
            req.original_body.clone()
        };
        if req.stream {
            let translator: Box<dyn StreamTranslator> = match response_pair {
                Some(pair) => (pair.stream)(&ResponseCtx {
                    model: &req.model,
                    original_request: &original,
                    translated_request: &body,
                }),
                None => Box::new(Identity),
            };
            let stream = frames(
                wire::lines(upstream, MAX_LINE),
                translator,
                req.response_format == Format::OpenAIResponse,
            );
            return Ok(ExecResponse {
                status: 200,
                headers: response_headers,
                body: ResponseBody::Stream(stream),
            });
        }
        let raw = wire::read_all(upstream).await?;
        let mut out = match response_pair {
            Some(pair) => (pair.non_stream)(
                &ResponseCtx {
                    model: &req.model,
                    original_request: &original,
                    translated_request: &body,
                },
                &raw,
            )
            .map_err(|_| ExecError::local(502, FailureScope::Request, EMPTY_TRANSLATION))?,
            None => raw.to_vec(),
        };
        if out.is_empty() {
            return Err(ExecError::local(502, FailureScope::Request, EMPTY_TRANSLATION));
        }
        if req.response_format == Format::OpenAIResponse {
            out = payload::ensure_responses_usage_details(&out);
        }
        Ok(ExecResponse {
            status: 200,
            headers: response_headers,
            body: ResponseBody::Buffered(Bytes::from(out)),
        })
    }

    /// `executeImages` / `executeImagesStream`: the body (JSON or multipart) goes to
    /// `/images/edits` or `/images/generations` with the model replaced. `request_path` is
    /// the inbound route; `req.stream` selects raw SSE passthrough.
    pub async fn images(
        &self,
        credential: &Credential,
        req: ExecRequest,
        request_path: &str,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        let base_model = parse_suffix(&req.model).model_name.trim().to_owned();
        let (base_url, api_key) = credentials(credential);
        if base_url.is_empty() {
            return Err(missing_base_url());
        }
        let content_type = req
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .trim()
            .to_owned();
        let (body, content_type) = prepare_images_payload(&req.body, &base_model, &content_type, req.stream)?;
        let content_type = if content_type.is_empty() {
            "application/json".to_owned()
        } else {
            content_type
        };
        let path = if request_path.trim().ends_with(IMAGES_EDITS) {
            IMAGES_EDITS
        } else {
            IMAGES_GENERATIONS
        };
        let mut headers = base_headers(&api_key, &content_type);
        if req.stream {
            headers.set("Accept", "text/event-stream");
            headers.set("Cache-Control", "no-cache");
        }
        apply_custom(&mut headers, credential, &req);
        let client = self.clients.for_credential(credential, cfg);
        let upstream = wire::send(&client, &endpoint(&base_url, path), headers, body).await?;
        if !(200..300).contains(&upstream.status) {
            return Err(upstream_error(upstream, !req.stream).await);
        }
        let headers = upstream.headers.clone();
        if req.stream {
            return Ok(ExecResponse {
                status: 200,
                headers,
                body: ResponseBody::Stream(upstream.body),
            });
        }
        Ok(ExecResponse {
            status: 200,
            headers,
            body: ResponseBody::Buffered(wire::read_all(upstream).await?),
        })
    }
}

/// `prepareOpenAICompatImagesPayload`.
fn prepare_images_payload(
    body: &[u8],
    model: &str,
    content_type: &str,
    stream: bool,
) -> Result<(Bytes, String), ExecError> {
    if go::json_valid(body) {
        let mut body = body.to_vec();
        if !model.is_empty() {
            payload::set_str_if_different(&mut body, "model", model);
        }
        if stream {
            payload::set_bool_if_different(&mut body, "stream", true);
        } else {
            gj::delete(&mut body, "stream");
        }
        return Ok((Bytes::from(body), "application/json".into()));
    }
    let Some(boundary) = multipart::boundary(content_type) else {
        return Ok((Bytes::copy_from_slice(body), content_type.to_owned()));
    };
    // Go returns a plain error here; the handler answers 500.
    let boundary = boundary.map_err(|e| ExecError::local(500, FailureScope::Request, e))?;
    let form = multipart::read_form(body, &boundary)
        .map_err(|e| ExecError::local(500, FailureScope::Request, format!("read multipart form failed: {e}")))?;
    let (out, content_type) = multipart::rewrite_images_form(&form, model, stream, false);
    Ok((Bytes::from(out), content_type))
}

/// `helps.PayloadRequestedModel`: the client's model, else the execution model.
fn route_model(req: &ExecRequest) -> &str {
    if req.requested_model.trim().is_empty() {
        req.model.trim()
    } else {
        req.requested_model.trim()
    }
}

/// `applyPromptCacheKey`.
fn prompt_cache_key(
    compat: Option<&payload::Compat>,
    provider: &str,
    req: &ExecRequest,
    base_model: &str,
    mut body: Vec<u8>,
) -> Vec<u8> {
    if !compat.is_some_and(|c| c.support_prompt_cache_key) {
        return body;
    }
    let explicit = [&req.body[..], &req.original_body[..], &body[..]]
        .into_iter()
        .map(|source| gj::get(source, "prompt_cache_key").str().trim().to_owned())
        .find(|key| !key.is_empty());
    if let Some(key) = explicit {
        payload::set_str_if_different(&mut body, "prompt_cache_key", &key);
        return body;
    }
    let model = gj::get(&body, "model").str().trim().to_owned();
    let model = if model.is_empty() { base_model.to_owned() } else { model };
    if req.source_format == Format::Claude
        && let Some(key) = payload::claude_code_prompt_cache(&model, &req.body, &req.headers)
    {
        payload::set_str_if_different(&mut body, "prompt_cache_key", &key);
        return body;
    }
    let Some(session) = provider_session_uuid(provider, req) else {
        return body;
    };
    let provider = provider.trim().go_lower();
    let identity = format!(
        "cli-proxy-api:openai-compat:prompt-cache\0{provider}\0{}\0{}\0{session}",
        model.go_lower(),
        req.source_format.as_str().trim().go_lower()
    );
    let key = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, identity.as_bytes()).to_string();
    payload::set_str_if_different(&mut body, "prompt_cache_key", &key);
    body
}

/// `helps.ProviderSessionUUID`: the execution session, else the derived session identity,
/// as a provider-scoped stable UUID.
fn provider_session_uuid(provider: &str, req: &ExecRequest) -> Option<String> {
    let nonempty = |s: &Option<String>| s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let (kind, value) = match nonempty(&req.execution_session) {
        Some(execution) => ("execution-session", execution),
        None => ("derived-session", nonempty(&req.derived_session)?),
    };
    let provider = provider.trim().go_lower();
    if provider.is_empty() {
        return None;
    }
    let identity = format!("cli-proxy-api\0{provider}\0{kind}\0{value}");
    Some(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, identity.as_bytes()).to_string())
}

/// `CountTokens`: a local tiktoken estimate of the translated chat request.
fn count_tokens(credential: &Credential, req: &ExecRequest, cfg: &Config) -> Result<ExecResponse, ExecError> {
    let base_model = parse_suffix(&req.model).model_name;
    let compat = payload::resolve_compat(credential, cfg);
    let resolved = payload::resolved_model(compat.as_ref(), credential, route_model(req), &req.model);
    let body = translate_request(req, Format::OpenAI, &base_model, false)?;
    let body = apply_thinking(body, req, Format::OpenAI, &credential.provider, resolved.as_ref())?;
    let count = payload::count_chat_tokens(&base_model, &body).map_err(|e| {
        ExecError::local(
            500,
            FailureScope::Request,
            format!("openai compat executor: token counting failed: {e}"),
        )
    })?;
    let usage = format!(r#"{{"usage":{{"prompt_tokens":{count},"completion_tokens":0,"total_tokens":{count}}}}}"#);
    let out = cpa_translate::translate_token_count(req.response_format, Format::OpenAI, count, usage.as_bytes());
    Ok(ExecResponse {
        status: 200,
        headers: Default::default(),
        body: ResponseBody::Buffered(Bytes::from(out)),
    })
}

/// Same-format passthrough when no pair is registered: one `data:` event per frame.
struct Identity;

impl StreamTranslator for Identity {
    fn event(&mut self, event: &[u8]) -> Result<Vec<Bytes>, cpa_translate::Error> {
        Ok(if event.starts_with(b"data: [DONE]") {
            Vec::new()
        } else {
            vec![Bytes::copy_from_slice(event)]
        })
    }

    fn finish(&mut self) -> Result<Vec<Bytes>, cpa_translate::Error> {
        Ok(Vec::new())
    }
}

fn trim(bytes: &[u8]) -> &[u8] {
    go::trim_space(bytes)
}

/// The ExecuteStream scan loop: SSE lines grouped into frames, Go's error detection on
/// every frame, and the terminal rules for `[DONE]` and EOF.
struct Frames {
    translator: Box<dyn StreamTranslator>,
    responses: bool,
    event: String,
    data: Vec<Vec<u8>>,
    ready: VecDeque<Result<Bytes, ExecError>>,
    seen_done: bool,
    failed: bool,
}

impl Frames {
    fn fail(&mut self, status: u16, message: impl Into<String>) -> bool {
        self.ready.push_back(Err(status_err(status, message)));
        self.failed = true;
        true
    }

    fn translate(&mut self, line: &[u8]) -> bool {
        let mut event = line.to_vec();
        event.extend_from_slice(b"\n\n");
        match self.translator.event(&event) {
            Ok(out) => {
                self.ready.extend(out.into_iter().map(Ok));
                false
            }
            Err(e) => self.fail(502, e.to_string()),
        }
    }

    /// `processFrame`; true ends the scan.
    fn frame(&mut self) -> bool {
        let event = std::mem::take(&mut self.event);
        let data = std::mem::take(&mut self.data);
        if data.is_empty() {
            return payload::error_event(&event) && self.fail(502, "upstream error event ended without data");
        }
        if data.len() > 1 && data.iter().any(|d| trim(d) == b"[DONE]") {
            return self.fail(502, "upstream stream ended with incomplete data before [DONE]");
        }
        let joined = data.join(&b"\n"[..]);
        let payload_bytes = trim(&joined);
        let done = payload_bytes == b"[DONE]";
        if done && payload::error_event(&event) {
            return self.fail(502, "upstream error event ended before [DONE]");
        }
        if !done && !go::json_valid(payload_bytes) {
            return self.fail(502, "upstream stream ended with incomplete SSE data frame");
        }
        if !done && let Some(status) = payload::stream_data_error(payload_bytes, &event) {
            return self.fail(status, String::from_utf8_lossy(payload_bytes));
        }
        let mut line = b"data: ".to_vec();
        line.extend_from_slice(payload_bytes);
        if self.translate(&line) {
            return true;
        }
        if done {
            self.seen_done = true;
        }
        done
    }

    /// One scanned line; true ends the scan.
    fn line(&mut self, line: &[u8]) -> bool {
        let line = trim(line);
        if line.is_empty() {
            return self.frame();
        }
        if let Some(rest) = line.strip_prefix(b"data:") {
            self.data.push(trim(rest).to_vec());
        } else if let Some(rest) = line.strip_prefix(b"event:") {
            self.event = String::from_utf8_lossy(trim(rest)).into_owned();
        } else if line.starts_with(b":") || line.starts_with(b"id:") || line.starts_with(b"retry:") {
        } else if line.starts_with(b"{") || line.starts_with(b"[") {
            return self.fail(502, String::from_utf8_lossy(line));
        }
        false
    }

    /// Clean EOF: flush a pending frame, then Go's terminal rule per client format.
    fn eof(&mut self) {
        if !self.seen_done && !self.failed && !self.data.is_empty() {
            self.frame();
        }
        if self.failed {
            return;
        }
        if !self.seen_done {
            if self.responses {
                self.fail(502, "upstream stream closed before [DONE]");
                return;
            }
            if self.translate(b"data: [DONE]") {
                return;
            }
        }
        self.finish();
    }

    fn finish(&mut self) {
        match self.translator.finish() {
            Ok(out) => self.ready.extend(out.into_iter().map(Ok)),
            Err(e) => {
                self.fail(502, e.to_string());
            }
        }
    }
}

fn frames(lines: ExecStream, translator: Box<dyn StreamTranslator>, responses: bool) -> ExecStream {
    let state = Frames {
        translator,
        responses,
        event: String::new(),
        data: Vec::new(),
        ready: VecDeque::new(),
        seen_done: false,
        failed: false,
    };
    futures_util::stream::unfold((lines, state, false), |(mut lines, mut state, mut ended)| async move {
        loop {
            if let Some(item) = state.ready.pop_front() {
                if item.is_err() {
                    state.ready.clear();
                    ended = true;
                }
                return Some((item, (lines, state, ended)));
            }
            if ended {
                return None;
            }
            match lines.next().await {
                Some(Ok(line)) => {
                    if state.line(&line) {
                        if !state.failed {
                            state.finish();
                        }
                        ended = true;
                    }
                }
                Some(Err(error)) => {
                    state.ready.push_back(Err(error));
                    ended = true;
                }
                None => {
                    state.eof();
                    ended = true;
                }
            }
        }
    })
    .boxed()
}

#[cfg(test)]
#[path = "openai_compat_tests.rs"]
mod tests;
