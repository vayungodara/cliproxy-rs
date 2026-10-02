//! Meta Muse executor (internal/runtime/executor/meta_executor*.go).
//!
//! Every client format is translated to Codex Responses and sent, always streaming, to
//! `{base}/responses`. Non-streaming clients get the completed response collected from
//! that SSE stream. A credential holds either a minted API key or only the DCA token
//! (`dca:...`) a key is minted from: by [`MetaExecutor::prepare`] (persisted), or inline
//! for one request as Go's ensureAuth does when preparation did not run.
//!
//! Shared Codex/Responses stages go through adapters named after their owners
//! (meta_codex, kimi_thinking, kimi_http). ponytail: the apply_patch Responses bridge
//! (translator common) is not applied, as for Kimi; requests without an apply_patch custom
//! tool are unaffected.

use std::collections::VecDeque;
use std::time::Duration;

use bytes::Bytes;
use cpa_core::config::Config;
use cpa_core::credential::{Credential, MetadataPatch, Source};
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, ExecStream, FailureScope, Operation, ResponseBody};
use cpa_core::format::Format;
use cpa_translate::{Pair, RequestCtx, ResponseCtx, StreamTranslator};
use futures_util::StreamExt;
use gjson::Kind;
use serde_json::Value;

use crate::kimi_http::{
    Clients, GoHeaders, MAX_ERROR_BODY, custom_headers, default_client, lines, proxy_url, read_all, refresh_due,
    rfc3339_local_now, send,
};
use crate::kimi_json::{delete, set_raw, set_str};
use crate::kimi_thinking::{self, parse_suffix};
use crate::meta_auth::{DEFAULT_API_BASE_URL, MetaAuth, MintedKey};
use crate::meta_codex::{
    CodexToResponses, OutputItems, codex_to_responses_non_stream, count_codex_input_tokens,
    ensure_responses_usage_details, go_trim_space, normalize_codex_instructions, normalize_codex_tool_integer_types,
    responses_to_codex, sanitize_reasoning_encrypted_content,
};

/// Provider string served by this executor.
pub const PROVIDER: &str = "meta";
/// `metaUserAgent`.
pub const USER_AGENT: &str =
    "muse-build/1.3.0 (interactive; macos-aarch64; build ac7280f2aca67769d1455a8847bb502b617d50f6)";
/// `metaNotFoundCooldown`: a 404 without `resets_at` cools the model for five minutes.
const NOT_FOUND_COOLDOWN: Duration = Duration::from_secs(300);
/// Go's bufio.Scanner limit for the upstream stream.
const LINE_LIMIT: usize = 52_428_800;
/// `helps.ApplyPatchUpstreamErrorMessage`, also used when translation yields nothing.
const APPLY_PATCH_ERROR: &str = "Invalid apply_patch tool arguments received from upstream.";
const DISCONNECTED: &str = "meta stream error: stream disconnected before response.completed or response.incomplete";

pub struct MetaExecutor {
    clients: Clients,
    mint_url: Option<String>,
}

impl Default for MetaExecutor {
    fn default() -> Self {
        Self::with_client(default_client())
    }
}

impl MetaExecutor {
    /// Uses a caller-built client (tests point it at local mocks).
    pub fn with_client(client: wreq::Client) -> Self {
        Self {
            clients: Clients::new(client),
            mint_url: None,
        }
    }

    /// Mints against a local mock instead of `META_MINT_URL` / api.meta.ai.
    pub fn with_mint_url(mut self, url: &str) -> Self {
        self.mint_url = Some(url.to_owned());
        self
    }

    /// ShouldPrepareRequestAuth (no usable key but a DCA token), or a due
    /// `refresh_interval`: the SDK refresh lead is nil, so nothing else schedules a mint.
    pub fn needs_prepare(&self, credential: &Credential, _cfg: &Config) -> bool {
        self.must_mint(credential)
            || (!is_config_api_key(credential)
                && dca_token(credential).is_some()
                && refresh_due(credential, None, chrono::Utc::now()))
    }

    /// `ShouldPrepareRequestAuth`: requests must wait for a mint because there is no
    /// usable key, only a DCA token.
    pub fn must_mint(&self, credential: &Credential) -> bool {
        !is_config_api_key(credential) && creds(credential).1.is_empty() && dca_token(credential).is_some()
    }

    /// Refresh: re-mints the API key from the DCA token. Also what the runtime should call
    /// after an upstream 401 (Go's tryRefreshAfterUnauthorized covers Meta DCA tokens).
    pub async fn prepare(&self, credential: &Credential, cfg: &Config) -> Result<MetadataPatch, ExecError> {
        self.refresh(credential, cfg).await
    }

    async fn refresh(&self, credential: &Credential, cfg: &Config) -> Result<MetadataPatch, ExecError> {
        // ponytail: helps.RefreshAuthViaHome (Home control center) is not ported.
        let Some(dca) = dca_token(credential) else {
            if !creds(credential).1.is_empty() {
                return Ok(MetadataPatch::default());
            }
            return Err(ExecError::local(
                401,
                FailureScope::Credential,
                "meta executor: missing API key or DCA token",
            ));
        };
        let mut auth = MetaAuth::new(self.clients.get(&proxy_url(credential, cfg)));
        if let Some(url) = &self.mint_url {
            auth = auth.with_mint_url(url);
        }
        let minted = auth.mint_api_key(&dca).await.map_err(|e| {
            ExecError::local(
                500,
                FailureScope::Credential,
                format!("meta executor: mint API key failed: {}", e.redacted()),
            )
        })?;
        if minted.api_key.is_empty() {
            return Err(ExecError::local(
                500,
                FailureScope::Credential,
                "meta executor: mint API key returned empty key",
            ));
        }
        Ok(refresh_patch(credential, &dca, &minted, rfc3339_local_now()))
    }

    /// ensureAuth + enrichAuth: a usable key (minting inline from the DCA token when
    /// missing), `base_url`/`api_key` attributes, and Meta's User-Agent as a default
    /// custom header.
    async fn ensure_auth(&self, credential: &Credential, cfg: &Config) -> Result<Credential, ExecError> {
        let mut c = credential.clone();
        if creds(&c).1.is_empty() && dca_token(&c).is_some() {
            let patch = self.refresh(&c, cfg).await?;
            patch.apply(&mut c.metadata);
            // Go's Refresh also writes these attributes on the request's auth.
            for key in ["base_url", "api_key", "access_token"] {
                if let Some(Value::String(v)) = patch.set.get(key) {
                    c.attributes.insert(key.into(), v.clone());
                }
            }
        }
        let (base, token) = creds(&c);
        if token.is_empty() {
            let message = if is_config_api_key(&c) {
                "meta executor: meta-api-key requires a valid API key (DCA tokens require OAuth storage)"
            } else {
                "meta executor: missing API key or access token"
            };
            return Err(ExecError::local(401, FailureScope::Credential, message));
        }
        if c.attributes.get("base_url").is_none_or(|v| v.trim().is_empty()) {
            c.attributes.insert("base_url".into(), base);
        }
        if c.attributes.get("api_key").is_none_or(|v| v.trim().is_empty()) {
            c.attributes.insert("api_key".into(), token);
        }
        c.attributes
            .entry("header:User-Agent".into())
            .or_insert_with(|| USER_AGENT.into());
        Ok(c)
    }

    pub async fn execute(
        &self,
        credential: &Credential,
        req: ExecRequest,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        if req.operation == Operation::CountTokens {
            return self.count_tokens(credential, req, cfg).await;
        }
        if req.alt.as_deref() == Some("responses/compact") {
            return Err(ExecError::local(
                501,
                FailureScope::Request,
                "/responses/compact not supported",
            ));
        }
        let enriched = self.ensure_auth(credential, cfg).await?;
        let prepared = prepare(&req, cfg, true)?;
        let (base, token) = creds(&enriched);
        if base.trim().is_empty() {
            return Err(ExecError::local(
                401,
                FailureScope::Credential,
                "meta executor: missing provider baseURL",
            ));
        }
        let url = format!("{}/responses", base.strip_suffix('/').unwrap_or(&base));
        let client = self.clients.get(&proxy_url(&enriched, cfg));
        let upstream = send(
            &client,
            &url,
            headers(&enriched, &req, &token),
            prepared.body.clone(),
            None,
        )
        .await?;
        if !(200..300).contains(&upstream.status) {
            let headers = upstream.headers.clone();
            // Go returns a failed error-body read as is, never classified by status.
            let body = read_all(upstream.body, MAX_ERROR_BODY, false).await?;
            let mut error = upstream_error(upstream.status, &body);
            error.headers = Box::new(headers);
            return Err(error);
        }
        let translated = Bytes::from(prepared.body);
        let responses_client = req.response_format == Format::OpenAIResponse;
        let body = if req.stream {
            let translator = prepared.response.stream_translator(&req, &translated);
            ResponseBody::Stream(stream_events(
                lines(upstream.body, LINE_LIMIT),
                translator,
                responses_client,
            ))
        } else {
            let data = read_all(upstream.body, usize::MAX, false).await?;
            let completed = collect_completed(&String::from_utf8_lossy(&data), |event| {
                prepared.response.non_stream(&req, &translated, event)
            })?;
            ResponseBody::Buffered(Bytes::from(if responses_client {
                ensure_responses_usage_details(completed.as_bytes())
            } else {
                completed.into_bytes()
            }))
        };
        Ok(ExecResponse {
            status: upstream.status,
            headers: upstream.headers,
            body,
        })
    }

    /// CountTokens: O200kBase estimate of the translated Codex body, no upstream call.
    async fn count_tokens(
        &self,
        credential: &Credential,
        req: ExecRequest,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        self.ensure_auth(credential, cfg).await?;
        let prepared = prepare(&req, cfg, false)?;
        let count = count_codex_input_tokens(&prepared.body).map_err(|e| {
            ExecError::local(
                500,
                FailureScope::Request,
                format!("meta executor: tokenizer init failed: {e}"),
            )
        })?;
        let usage = format!(
            r#"{{"response":{{"usage":{{"input_tokens":{count},"output_tokens":0,"total_tokens":{count}}}}}}}"#
        );
        let payload = match prepared.response {
            ResponseSide::Pair(Pair {
                count_tokens: Some(translate),
                ..
            }) => {
                let translated = Bytes::from(prepared.body);
                let ctx = response_ctx(&req, &translated);
                translate(&ctx, usage.as_bytes()).map_err(|e| ExecError::local(500, FailureScope::Request, e.0))?
            }
            // TranslateTokenCount without a registered transform returns the usage as is.
            _ => usage.into_bytes(),
        };
        Ok(ExecResponse {
            status: 200,
            headers: http::HeaderMap::new(),
            body: ResponseBody::Buffered(Bytes::from(payload)),
        })
    }
}

/// How upstream Codex events become client events.
enum ResponseSide {
    Pair(&'static Pair),
    /// The openai-response <- codex adapter (meta_codex).
    Responses,
}

impl ResponseSide {
    fn stream_translator(&self, req: &ExecRequest, translated: &Bytes) -> Box<dyn StreamTranslator> {
        match self {
            Self::Pair(pair) => (pair.stream)(&response_ctx(req, translated)),
            Self::Responses => Box::new(CodexToResponses::new(&req.model, original(req), translated)),
        }
    }

    fn non_stream(&self, req: &ExecRequest, translated: &Bytes, completed: &str) -> Result<String, ExecError> {
        let out = match self {
            Self::Pair(pair) => (pair.non_stream)(&response_ctx(req, translated), completed.as_bytes())
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .map_err(|e| ExecError::local(502, FailureScope::Request, e.0))?,
            Self::Responses => codex_to_responses_non_stream(completed),
        };
        if out.is_empty() {
            return Err(ExecError::local(502, FailureScope::Request, APPLY_PATCH_ERROR));
        }
        Ok(out)
    }
}

fn response_ctx<'a>(req: &'a ExecRequest, translated: &'a Bytes) -> ResponseCtx<'a> {
    ResponseCtx {
        model: &req.model,
        original_request: original(req),
        translated_request: translated,
    }
}

/// `opts.OriginalRequest`, falling back to the request payload when empty.
fn original(req: &ExecRequest) -> &Bytes {
    if req.original_body.is_empty() {
        &req.body
    } else {
        &req.original_body
    }
}

struct Prepared {
    body: String,
    response: ResponseSide,
}

fn request_error(message: impl Into<String>) -> ExecError {
    ExecError::local(400, FailureScope::Request, message)
}

fn not_registered(what: &str) -> ExecError {
    ExecError::local(
        501,
        FailureScope::Request,
        format!("{what} translation pair is not registered"),
    )
}

fn text(body: &[u8]) -> Result<&str, ExecError> {
    std::str::from_utf8(body).map_err(|_| request_error("request body is not valid UTF-8"))
}

/// ponytail: adapter for `cpa_common::payload` (owner: server thread). Go applies
/// `requests.payload` rules here (ApplyPayloadConfigWithRequest, protocol "meta"); identity
/// until the shared module lands.
fn apply_payload_rules(_cfg: &Config, _model: &str, _protocol: &str, _source: &str, body: String) -> String {
    body
}

/// ponytail: adapter for `cpa_common::codex_client` (owner: Codex thread). Go translates
/// through TranslateRequestWithCodexMultiAgentV2, which rewrites Codex CLI requests;
/// identity until the shared module lands.
fn codex_client_request(_req: &ExecRequest, body: &[u8]) -> Vec<u8> {
    body.to_vec()
}

/// prepareResponsesRequest.
fn prepare(req: &ExecRequest, cfg: &Config, stream: bool) -> Result<Prepared, ExecError> {
    let response = match cpa_translate::pair(req.response_format, Format::Codex) {
        Some(pair) => ResponseSide::Pair(pair),
        None if req.response_format == Format::OpenAIResponse => ResponseSide::Responses,
        None => return Err(not_registered("Meta response")),
    };
    let (base_model, _) = parse_suffix(&req.model);
    let source = text(&req.body)?;
    let client_body = codex_client_request(req, &req.body);
    let mut body = match cpa_translate::pair(req.source_format, Format::Codex) {
        Some(pair) => {
            let out = (pair.request)(
                &RequestCtx {
                    model: base_model,
                    stream,
                },
                &client_body,
            )
            .map_err(|e| request_error(e.0))?;
            String::from_utf8(out).map_err(|_| request_error("translated request is not valid UTF-8"))?
        }
        // ponytail: adapter for the openai-response -> codex pair (owner: translators thread).
        None if req.source_format == Format::OpenAIResponse => responses_to_codex(text(&client_body)?),
        None => return Err(not_registered("Meta request")),
    };
    body = kimi_thinking::apply(
        &body,
        source,
        text(original(req))?,
        &req.model,
        req.source_format.as_str(),
        "codex",
        PROVIDER,
    )
    .map_err(|e| request_error(e.0))?;
    body = apply_payload_rules(cfg, base_model, PROVIDER, req.source_format.as_str(), body);
    body = set_string_if_different(&body, "model", base_model);
    body = set_bool_if_different(&body, "stream", stream);
    for key in [
        "generate",
        "prompt_cache_retention",
        "safety_identifier",
        "stream_options",
        "client_metadata",
    ] {
        body = delete(&body, key);
    }
    body = normalize_codex_instructions(&body);
    body = sanitize_reasoning_encrypted_content(body);
    body = sanitize_web_search_tools(&body);
    body = normalize_codex_tool_integer_types(body, &req.headers);
    Ok(Prepared { body, response })
}

/// `SetStringIfDifferent`.
fn set_string_if_different(body: &str, path: &str, value: &str) -> String {
    let current = gjson::get(body, path);
    if current.kind() == Kind::String && current.str() == value {
        return body.to_owned();
    }
    set_str(body, path, value).unwrap_or_else(|_| body.to_owned())
}

/// `SetBoolIfDifferent`.
fn set_bool_if_different(body: &str, path: &str, value: bool) -> String {
    let kind = gjson::get(body, path).kind();
    if (value && kind == Kind::True) || (!value && kind == Kind::False) {
        return body.to_owned();
    }
    set_raw(body, path, if value { "true" } else { "false" }).unwrap_or_else(|_| body.to_owned())
}

/// `SanitizeMetaWebSearchTools`: Meta rejects `search_content_types` on `web_search` tools,
/// top level or inside a namespace.
fn sanitize_web_search_tools(body: &str) -> String {
    let tools = gjson::get(body, "tools");
    if tools.kind() != Kind::Array {
        return body.to_owned();
    }
    let strip =
        |tool: &gjson::Value<'_>| tool.get("type").str() == "web_search" && tool.get("search_content_types").exists();
    let mut paths = Vec::new();
    for (i, tool) in tools.array().iter().enumerate() {
        if strip(tool) {
            paths.push(format!("tools.{i}.search_content_types"));
        }
        let nested = tool.get("tools");
        if tool.get("type").str() == "namespace" && nested.kind() == Kind::Array {
            for (j, sub) in nested.array().iter().enumerate() {
                if strip(sub) {
                    paths.push(format!("tools.{i}.tools.{j}.search_content_types"));
                }
            }
        }
    }
    paths.iter().fold(body.to_owned(), |body, path| delete(&body, path))
}

fn headers(c: &Credential, req: &ExecRequest, token: &str) -> GoHeaders {
    let mut h = GoHeaders::new();
    h.set("Content-Type", "application/json");
    if !token.trim().is_empty() {
        h.set("Authorization", format!("Bearer {token}"));
    }
    h.set("User-Agent", USER_AGENT);
    h.set("X-Client-Id", "tbh:tui");
    // Meta always streams upstream.
    h.set("Accept", "text/event-stream");
    h.set("Cache-Control", "no-cache");
    for (name, value) in custom_headers(c, &req.headers, req.session.as_deref()) {
        h.set(&name, value);
    }
    h
}

/// `wrapMetaUpstreamError`, with Go's cooldown semantics as scopes: a 429 or 404 cools
/// the model on this credential, a subscription-quota 429 the whole credential.
fn upstream_error(status: u16, body: &[u8]) -> ExecError {
    let text = String::from_utf8_lossy(body);
    let mut retry_after = None;
    let scope = match status {
        429 => {
            retry_after = resets_in(&text);
            if subscription_quota(&text) {
                FailureScope::Credential
            } else {
                FailureScope::Model
            }
        }
        404 => {
            retry_after = Some(resets_in(&text).unwrap_or(NOT_FOUND_COOLDOWN));
            FailureScope::Model
        }
        401 | 402 | 403 | 408 | 500.. => FailureScope::Credential,
        _ => FailureScope::Request,
    };
    ExecError {
        status,
        scope,
        body: Bytes::copy_from_slice(body),
        headers: Box::default(),
        retry_after,
        direct: false,
    }
}

/// `parseMetaRetryAfter` for a 429/404 body: time until a future `error.resets_at`.
fn resets_in(body: &str) -> Option<Duration> {
    let resets_at = gjson::get(body, "error.resets_at").i64();
    if resets_at <= 0 {
        return None;
    }
    let now = std::time::SystemTime::now();
    let at = std::time::UNIX_EPOCH + Duration::from_secs(resets_at as u64);
    at.duration_since(now).ok().filter(|d| !d.is_zero())
}

/// `isMetaSubscriptionQuota` (status already 429).
fn subscription_quota(body: &str) -> bool {
    if body.is_empty() {
        return false;
    }
    let message = gjson::get(body, "error.message").str().to_lowercase();
    let code = crate::kimi_json::gstr(&gjson::get(body, "error.code")).to_lowercase();
    if message.contains("subscription quota") || message.contains("quota exhausted") {
        return true;
    }
    (code == "rate_limit_exceeded" || code.contains("quota")) && gjson::get(body, "error.resets_at").exists()
}

/// `metaStreamEventError`: an `error` or `response.failed` event ends the response with
/// `error.code` as status when it is an HTTP error code, else 502.
fn stream_event_error(event: &str) -> Option<ExecError> {
    let kind = gjson::get(event, "type");
    if kind.str() != "error" && kind.str() != "response.failed" {
        return None;
    }
    let code = gjson::get(event, "error.code").i64();
    let status = if (400..=599).contains(&code) { code as u16 } else { 502 };
    Some(upstream_error(status, event.as_bytes()))
}

/// `metaAsCompletedEvent`: a whole JSON body that is a terminal event or a bare response.
fn as_completed_event(data: &str) -> Option<String> {
    let trimmed = std::str::from_utf8(go_trim_space(data.as_bytes())).unwrap_or_default();
    if !gjson::valid(trimmed) {
        return None;
    }
    let kind = gjson::get(trimmed, "type");
    if matches!(kind.str(), "response.completed" | "response.incomplete") {
        return Some(trimmed.to_owned());
    }
    if gjson::get(trimmed, "object").str() == "response" || gjson::get(trimmed, "output").exists() {
        return set_raw(r#"{"type":"response.completed"}"#, "response", trimmed).ok();
    }
    None
}

fn data_payload(line: &[u8]) -> Option<&[u8]> {
    line.strip_prefix(b"data:").map(go_trim_space)
}

/// translateMetaCompleted: the first terminal event of a buffered SSE body, with output
/// items rebuilt from `response.output_item.done`, then a whole-body JSON fallback.
fn collect_completed(data: &str, translate: impl Fn(&str) -> Result<String, ExecError>) -> Result<String, ExecError> {
    let mut items = OutputItems::default();
    for line in data.split('\n') {
        let Some(event) = data_payload(line.as_bytes()) else {
            continue;
        };
        let event = String::from_utf8_lossy(event);
        if let Some(error) = stream_event_error(&event) {
            return Err(error);
        }
        match gjson::get(&event, "type").str() {
            "response.output_item.done" => items.collect(&event),
            "response.completed" | "response.incomplete" => return translate(&items.patch_completed(&event)),
            _ => {}
        }
    }
    if let Some(completed) = as_completed_event(data) {
        return translate(&items.patch_completed(&completed));
    }
    Err(ExecError::local(408, FailureScope::Credential, DISCONNECTED))
}

/// The ExecuteStream loop: every scanned line goes through the translator (`data:` lines
/// normalized to `data: <trimmed>`), terminal events get their collected output, and an
/// error event or scan error ends the stream. Empty chunks are dropped; Go's Responses
/// writer ignores them.
fn stream_events(upstream: ExecStream, translator: Box<dyn StreamTranslator>, responses_client: bool) -> ExecStream {
    struct State {
        upstream: ExecStream,
        translator: Box<dyn StreamTranslator>,
        items: OutputItems,
        ready: VecDeque<Result<Bytes, ExecError>>,
        done: bool,
        responses_client: bool,
    }
    impl State {
        fn emit(&mut self, translated: Result<Vec<Bytes>, cpa_translate::Error>) {
            match translated {
                Ok(chunks) => {
                    for chunk in chunks {
                        let chunk = if self.responses_client {
                            Bytes::from(ensure_responses_usage_details(&chunk))
                        } else {
                            chunk
                        };
                        if !chunk.is_empty() {
                            self.ready.push_back(Ok(chunk));
                        }
                    }
                }
                Err(error) => {
                    self.done = true;
                    self.ready
                        .push_back(Err(ExecError::local(502, FailureScope::Request, error.0)));
                }
            }
        }

        fn line(&mut self, line: &[u8]) {
            let Some(event) = data_payload(line) else {
                let translated = self.translator.event(line);
                return self.emit(translated);
            };
            let mut event = String::from_utf8_lossy(event).into_owned();
            if let Some(error) = stream_event_error(&event) {
                self.done = true;
                self.ready.push_back(Err(error));
                return;
            }
            match gjson::get(&event, "type").str() {
                "response.output_item.done" => self.items.collect(&event),
                "response.completed" | "response.incomplete" => event = self.items.patch_completed(&event),
                _ => {}
            }
            let translated = self.translator.event(format!("data: {event}").as_bytes());
            self.emit(translated);
        }
    }
    futures_util::stream::unfold(
        State {
            upstream,
            translator,
            items: OutputItems::default(),
            ready: VecDeque::new(),
            done: false,
            responses_client,
        },
        |mut st| async move {
            loop {
                if let Some(item) = st.ready.pop_front() {
                    return Some((item, st));
                }
                if st.done {
                    return None;
                }
                match st.upstream.next().await {
                    Some(Ok(line)) => st.line(&line),
                    Some(Err(error)) => {
                        st.done = true;
                        st.ready.push_back(Err(error));
                    }
                    None => {
                        st.done = true;
                        let finished = st.translator.finish();
                        st.emit(finished);
                    }
                }
            }
        },
    )
    .boxed()
}

/// The Refresh metadata change after a successful mint.
pub(crate) fn refresh_patch(
    credential: &Credential,
    dca: &str,
    minted: &MintedKey,
    now_local: String,
) -> MetadataPatch {
    let mut base = creds(credential).0;
    if !minted.base_url.trim().is_empty() {
        base = minted.base_url.trim().to_owned();
    }
    let mut patch = MetadataPatch::default();
    let mut set = |key: &str, value: Value| {
        patch.set.insert(key.into(), value);
    };
    set("base_url", base.into());
    set("api_key", minted.api_key.clone().into());
    set("access_token", minted.api_key.clone().into());
    set("dca_token", dca.into());
    if !minted.user_email.is_empty() {
        set("email", minted.user_email.clone().into());
    }
    if !minted.user_full_name.is_empty() {
        set("name", minted.user_full_name.clone().into());
    }
    if !minted.subs_tier_name.is_empty() {
        set("subs_tier_name", minted.subs_tier_name.clone().into());
    }
    if !minted.subs_tier_id.is_empty() {
        set("subs_tier_id", minted.subs_tier_id.clone().into());
    }
    set("is_subs_active", minted.is_subs_active.into());
    set("has_payment_method", minted.has_payment_method.into());
    set("type", PROVIDER.into());
    set("last_refresh", now_local.into());
    patch.remove.push("expired".into());
    if minted.subs_tier_name.is_empty() {
        patch.remove.push("subs_tier_name".into());
    }
    if minted.subs_tier_id.is_empty() {
        patch.remove.push("subs_tier_id".into());
    }
    patch
}

fn attribute<'a>(c: &'a Credential, key: &str) -> &'a str {
    c.attributes.get(key).map(|v| v.trim()).unwrap_or_default()
}

fn metadata_str<'a>(c: &'a Credential, key: &str) -> Option<&'a str> {
    c.str(key).map(str::trim).filter(|s| !s.is_empty())
}

/// `metaCreds`: base URL and usable (non-DCA) key, attributes before metadata.
pub(crate) fn creds(c: &Credential) -> (String, String) {
    let mut base = DEFAULT_API_BASE_URL.to_owned();
    let mut token = String::new();
    let usable = |t: &str| !t.is_empty() && !t.starts_with("dca:");
    if !attribute(c, "base_url").is_empty() {
        base = attribute(c, "base_url").to_owned();
    }
    if usable(attribute(c, "api_key")) {
        token = attribute(c, "api_key").to_owned();
    } else if usable(attribute(c, "access_token")) {
        token = attribute(c, "access_token").to_owned();
    }
    if base == DEFAULT_API_BASE_URL
        && let Some(b) = metadata_str(c, "base_url").or_else(|| metadata_str(c, "api_base_url"))
    {
        base = b.to_owned();
    }
    if token.is_empty() {
        if let Some(k) = metadata_str(c, "api_key").filter(|k| usable(k)) {
            token = k.to_owned();
        } else if let Some(t) = metadata_str(c, "access_token").filter(|t| usable(t)) {
            token = t.to_owned();
        }
    }
    (base, token)
}

/// `extractDCAToken`: never for config API keys.
pub(crate) fn dca_token(c: &Credential) -> Option<String> {
    if is_config_api_key(c) {
        return None;
    }
    if !attribute(c, "dca_token").is_empty() {
        return Some(attribute(c, "dca_token").to_owned());
    }
    if attribute(c, "access_token").starts_with("dca:") {
        return Some(attribute(c, "access_token").to_owned());
    }
    if let Some(d) = metadata_str(c, "dca_token") {
        return Some(d.to_owned());
    }
    metadata_str(c, "access_token")
        .filter(|t| t.starts_with("dca:"))
        .map(str::to_owned)
}

/// `IsConfigAPIKeyAuth`: an API-key credential whose source is config.yaml.
pub(crate) fn is_config_api_key(c: &Credential) -> bool {
    let kind = |raw: &str| match raw.trim().to_ascii_lowercase().as_str() {
        "apikey" | "api_key" | "api-key" => Some("apikey"),
        "oauth" | "oauth2" => Some("oauth"),
        _ => None,
    };
    let oauth_metadata = [
        "access_token",
        "refresh_token",
        "id_token",
        "email",
        "token_type",
        "expires_at",
        "expired",
    ]
    .iter()
    .any(|k| metadata_str(c, k).is_some())
        || c.metadata
            .get("token")
            .and_then(Value::as_object)
            .is_some_and(|t| !t.is_empty());
    let auth_kind = kind(attribute(c, "auth_kind"))
        .or_else(|| c.str("auth_kind").and_then(kind))
        .or_else(|| (!attribute(c, "api_key").is_empty()).then_some("apikey"))
        .or_else(|| oauth_metadata.then_some("oauth"));
    if auth_kind != Some("apikey") {
        return false;
    }
    if attribute(c, "runtime_only").eq_ignore_ascii_case("true") {
        return false;
    }
    match attribute(c, "source_backend").to_ascii_lowercase().as_str() {
        "config" => return true,
        "file" | "filesystem" | "git" | "memory" | "runtime" | "runtime_only" | "objectstore" | "object-store"
        | "postgres" | "postgresql" | "database" | "db" => return false,
        _ => {}
    }
    let source = attribute(c, "source");
    if !source.is_empty() {
        return source.to_ascii_lowercase().starts_with("config:") || source.eq_ignore_ascii_case("config");
    }
    if !attribute(c, "path").is_empty() {
        return false;
    }
    matches!(c.source, Source::Config { .. })
}

#[cfg(test)]
#[path = "meta_tests.rs"]
mod tests;
