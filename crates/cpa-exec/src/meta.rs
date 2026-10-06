//! Meta Muse executor (internal/runtime/executor/meta_executor*.go).
//!
//! Every client format is translated to Codex Responses and sent, always streaming, to
//! `{base}/responses`. Non-streaming clients get the completed response collected from
//! that SSE stream. A credential holds either a minted API key or only the DCA token
//! (`dca:...`) a key is minted from: by [`MetaExecutor::prepare`] (persisted), or inline
//! for one request as Go's ensureAuth does when preparation did not run.
//!
//! Translation uses cpa-translate (the registered client <-> Codex pair), thinking
//! cpa_common::thinking, JSON edits cpa_common::json, and the Codex and OpenAI-compatible
//! executors' ports of the shared Responses helpers. Stages whose shared module has not
//! landed go through adapters named after their owners (meta_codex, kimi_http, below).
//! Translator apply_patch failures end streams with Go's 502. Go's apply_patch Responses
//! bridge (cpa_translate::apply_patch_responses) runs around the Codex wire: a client's
//! custom `apply_patch` tool goes upstream as a JSON function and its calls come back as
//! the custom tool.

use std::collections::VecDeque;
use std::time::Duration;

use bytes::Bytes;
use cpa_common::json::{self as gj, Kind};
use cpa_common::thinking::{ModelCaps, RequestThinking, apply_request_thinking, parse_suffix};
use cpa_core::config::Config;
use cpa_core::credential::{Credential, MetadataPatch, Source};
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, ExecStream, FailureScope, Operation, ResponseBody};
use cpa_core::format::Format;
use cpa_translate::{Pair, RequestCtx, ResponseCtx, StreamTranslator, apply_patch_responses};
use futures_util::StreamExt;
use serde_json::Value;

use crate::codex_response::OutputItems;
use crate::codex_tokens::count_input_tokens;
use crate::kimi_http::{
    DeferredUsage, UsageRule, credential_headers, defer_usage, payload_rules, read_all_strict, refresh_due,
    report_model, rfc3339_local_now,
};
use crate::meta_auth::{DEFAULT_API_BASE_URL, MetaAuth, MintedKey};
use crate::meta_codex::go_trim_space;
use crate::openai_compat_payload::{ensure_responses_usage_details, sanitize_reasoning_encrypted_content};
use crate::proxy::{GoClients, GoHeaders, MAX_ERROR_BODY, Proxy, default_client, lines, read_all, send};
use crate::tokenizer::Encoding;
use cpa_common::codex_client::normalize_codex_instructions;

/// Provider string served by this executor.
pub const PROVIDER: &str = "meta";
/// `metaUserAgent`.
pub const USER_AGENT: &str =
    "muse-build/1.3.0 (interactive; macos-aarch64; build ac7280f2aca67769d1455a8847bb502b617d50f6)";
/// `metaNotFoundCooldown`: a 404 without `resets_at` cools the model for five minutes.
const NOT_FOUND_COOLDOWN: Duration = Duration::from_secs(300);
/// Go's bufio.Scanner limit for the upstream stream.
const LINE_LIMIT: usize = 52_428_800;
const DISCONNECTED: &str = "meta stream error: stream disconnected before response.completed or response.incomplete";

pub struct MetaExecutor {
    clients: GoClients,
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
            clients: GoClients::with_default(client),
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
    pub fn needs_prepare(&self, credential: &Credential, cfg: &Config) -> bool {
        self.needs_prepare_at(credential, cfg, chrono::Utc::now())
    }

    pub fn needs_prepare_at(&self, credential: &Credential, _cfg: &Config, now: chrono::DateTime<chrono::Utc>) -> bool {
        self.must_mint(credential)
            || (!is_config_api_key(credential)
                // Go's refresh loop never schedules API-key-kind credentials.
                && !cpa_core::registry::dynamic::is_api_key(credential)
                && dca_token(credential).is_some()
                && refresh_due(credential, None, now))
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
        // Go `helps.RefreshAuthViaHome`: Home refreshes the credentials it dispatched.
        if credential
            .attributes
            .contains_key(cpa_core::config::credentials::HOME_PROVIDER)
        {
            return refresh_via_home(credential).await;
        }
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
        let mut auth = MetaAuth::new(self.clients.get(&Proxy::effective(credential, cfg)));
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
        req.usage.request(Format::Codex, &prepared.body);
        let (base, token) = creds(&enriched);
        if base.trim().is_empty() {
            return Err(ExecError::local(
                401,
                FailureScope::Credential,
                "meta executor: missing provider baseURL",
            ));
        }
        let url = format!("{}/responses", base.strip_suffix('/').unwrap_or(&base));
        let client = self.clients.get(&Proxy::effective(&enriched, cfg));
        let body = Bytes::from(prepared.body);
        // Go: TrackHTTPClient for Execute; ExecuteStream uses TrackHTTPClientRoundTripOnly,
        // so its first body byte only records the first-packet fallback.
        use crate::kimi_http::{account_info, capture_chunk, capture_error, capture_metadata, capture_request};
        let capture = req.capture();
        let request_headers = headers(&enriched, &req, &token);
        // recordMetaRequest: the enriched credential's account info.
        let (auth_type, auth_value) = account_info(&enriched);
        capture_request(
            capture,
            &enriched,
            &url,
            &request_headers,
            &body,
            PROVIDER,
            (auth_type, &auth_value),
        );
        req.usage.round_trip_started();
        let mut upstream = send(&client, &url, request_headers, body.clone(), None)
            .await
            .inspect_err(|e| capture_error(capture, e))?;
        capture_metadata(capture, upstream.status, &upstream.headers);
        upstream.body = crate::kimi_http::track_first_byte(upstream.body, &req.usage, req.stream);
        if !(200..300).contains(&upstream.status) {
            let headers = upstream.headers.clone();
            // Go returns a failed error-body read as is, never classified by status.
            let error_body = read_all_strict(upstream.body, MAX_ERROR_BODY)
                .await
                .inspect_err(|e| capture_error(capture, e))?;
            // ponytail: the MAX_ERROR_BODY prefix (proxy.rs ceiling); Go logs the whole body.
            capture_chunk(capture, &error_body);
            // Go observes the response model on every non-stream body, errors included.
            if !req.stream {
                report_model(&req.usage, Format::Codex, &error_body);
            }
            let mut error = upstream_error(upstream.status, &error_body);
            error.headers = Box::new(headers);
            return Err(error);
        }
        let ctx = response_ctx(&req, &body);
        let responses_client = req.response_format == Format::OpenAIResponse;
        let out = if req.stream {
            // Go publishes stream usage only through its buffer (no EnsurePublished).
            req.usage.usage_required();
            // Go observes `data:` lines: their response model, and usage once completed.
            let lines = crate::kimi_http::capture_lines(lines(upstream.body, LINE_LIMIT), capture);
            let (tapped, usage) = defer_usage(lines, &req.usage, UsageRule::MetaResponses);
            ResponseBody::Stream(stream_events(
                tapped,
                (prepared.response.stream)(&ctx),
                responses_client,
                prepared.apply_patch,
                usage,
                capture.clone(),
            ))
        } else {
            let data = read_all(upstream.body, usize::MAX, false)
                .await
                .inspect_err(|e| capture_error(capture, e))?;
            capture_chunk(capture, &data);
            report_model(&req.usage, Format::Codex, &data);
            let source = std::cell::RefCell::new(Vec::new());
            let mut apply_patch = prepared.apply_patch;
            let completed = collect_completed(&data, &mut apply_patch, |event| {
                source.borrow_mut().clear();
                source.borrow_mut().extend_from_slice(event);
                // A translator error or empty output is Go's apply_patch 502.
                (prepared.response.non_stream)(&ctx, event)
                    .ok()
                    .filter(|out| !out.is_empty())
                    .ok_or_else(|| {
                        ExecError::local(502, FailureScope::Request, cpa_translate::APPLY_PATCH_UPSTREAM_ERROR)
                    })
            })?;
            // The terminal event Go reads model and Codex usage from, once translated.
            req.usage.response_line(Format::Codex, &source.into_inner());
            ResponseBody::Buffered(Bytes::from(if responses_client {
                ensure_responses_usage_details(&completed)
            } else {
                completed
            }))
        };
        Ok(ExecResponse {
            status: upstream.status,
            headers: upstream.headers,
            body: out,
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
        let count = count_input_tokens(Encoding::O200kBase, &prepared.body).map_err(|e| {
            ExecError::local(
                500,
                FailureScope::Request,
                format!("meta executor: tokenizer init failed: {e}"),
            )
        })?;
        let usage = format!(
            r#"{{"response":{{"usage":{{"input_tokens":{count},"output_tokens":0,"total_tokens":{count}}}}}}}"#
        );
        let payload = cpa_translate::translate_token_count(req.response_format, Format::Codex, count, usage.as_bytes());
        Ok(ExecResponse {
            status: 200,
            headers: http::HeaderMap::new(),
            body: ResponseBody::Buffered(Bytes::from(payload)),
        })
    }
}

fn response_ctx<'a>(req: &'a ExecRequest, translated: &'a [u8]) -> ResponseCtx<'a> {
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
    body: Vec<u8>,
    /// Transforms for the client's response format against the Codex upstream.
    response: &'static Pair,
    /// Go's `ApplyPatchResponsesState` for this request.
    apply_patch: apply_patch_responses::State,
}

fn not_registered(what: &str) -> ExecError {
    ExecError::local(
        501,
        FailureScope::Request,
        format!("{what} translation pair is not registered"),
    )
}

/// prepareResponsesRequest. Every Go client format has a Codex translator; here the
/// unregistered ones answer 501 instead of falling back to a model rewrite.
fn prepare(req: &ExecRequest, cfg: &Config, stream: bool) -> Result<Prepared, ExecError> {
    let Some(response) = cpa_translate::pair(req.response_format, Format::Codex) else {
        return Err(not_registered("Meta response"));
    };
    let has_request_transformer = cpa_translate::pair(req.source_format, Format::Codex).is_some();
    if !has_request_transformer {
        return Err(not_registered("Meta request"));
    }
    let base_model = parse_suffix(&req.model).model_name;
    // Go: helps.TranslateRequestWithAPIKeyModelCompatibility with APIKeyModelIsCompat.
    let is_compat = req.resolved_model.as_ref().is_some_and(|r| r.is_compat());
    let client = crate::codex_client::Client::new(&req.headers, cfg, "", is_compat);
    let translate = |body: &[u8]| {
        crate::codex_client::translate_request(
            req.source_format,
            Format::Codex,
            &RequestCtx {
                model: &base_model,
                stream,
            },
            body,
            &client,
        )
        .map_err(|e| ExecError::local(400, FailureScope::Request, e.0))
    };
    let mut body = translate(&req.body)?;
    // Go `cliproxyauth.ResolvedModelInfo`: capabilities bound to this attempt.
    let caps = req.resolved_model.as_ref().map(|r| ModelCaps::from(&r.info));
    // Go translates the original request too, for payload-rule defaults.
    let original_translated = translate(original(req))?;
    let thinking = apply_request_thinking(&RequestThinking {
        body: &body,
        payload: &req.body,
        original: original(req),
        model: &req.model,
        from: req.source_format.as_str(),
        to: "codex",
        provider: PROVIDER,
        resolved: caps.as_ref().map(Some),
        has_request_transformer,
        updates_changed: false,
    })
    .map_err(|e| ExecError::local(e.status(), FailureScope::Request, e.message))?;
    body = thinking;
    body = payload_rules(cfg, req, &base_model, PROVIDER, body, &original_translated);
    set_string_if_different(&mut body, "model", &base_model);
    set_bool_if_different(&mut body, "stream", stream);
    for key in [
        "generate",
        "prompt_cache_retention",
        "safety_identifier",
        "stream_options",
        "client_metadata",
    ] {
        gj::delete(&mut body, key);
    }
    let apply_patch = apply_patch_responses::State::new(req.source_format, original(req), &original_translated);
    body = apply_patch_responses::normalize_executor_request(&body, Some(original(req)))
        .map_err(|e| ExecError::local(500, FailureScope::Request, e))?;
    normalize_codex_instructions(&mut body);
    body = sanitize_reasoning_encrypted_content(body);
    sanitize_web_search_tools(&mut body);
    body = cpa_common::payload::normalize_codex_tool_integer_types(&body, &req.headers);
    Ok(Prepared {
        body,
        response,
        apply_patch,
    })
}

/// `SetStringIfDifferent`.
fn set_string_if_different(body: &mut Vec<u8>, path: &str, value: &str) {
    let current = gj::get(body, path);
    if current.kind == Kind::String && *current.bytes() == *value.as_bytes() {
        return;
    }
    gj::set_str(body, path, value);
}

/// `SetBoolIfDifferent`.
fn set_bool_if_different(body: &mut Vec<u8>, path: &str, value: bool) {
    let kind = gj::get(body, path).kind;
    if (value && kind == Kind::True) || (!value && kind == Kind::False) {
        return;
    }
    gj::set_bool(body, path, value);
}

/// `SanitizeMetaWebSearchTools`: Meta rejects `search_content_types` on `web_search` tools,
/// top level or inside a namespace.
fn sanitize_web_search_tools(body: &mut Vec<u8>) {
    let tools = gj::get(body, "tools");
    if !tools.is_array() {
        return;
    }
    let strip =
        |tool: &gj::Res<'_>| &*tool.get("type").bytes() == b"web_search" && tool.get("search_content_types").exists();
    let mut paths = Vec::new();
    for (i, tool) in tools.array().iter().enumerate() {
        if strip(tool) {
            paths.push(format!("tools.{i}.search_content_types"));
        }
        let nested = tool.get("tools");
        if &*tool.get("type").bytes() == b"namespace" && nested.is_array() {
            for (j, sub) in nested.array().iter().enumerate() {
                if strip(sub) {
                    paths.push(format!("tools.{i}.tools.{j}.search_content_types"));
                }
            }
        }
    }
    for path in paths {
        gj::delete(body, &path);
    }
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
    for (name, value) in credential_headers(c, req, original(req)) {
        h.set(&name, value);
    }
    h
}

/// `wrapMetaUpstreamError`, with Go's cooldown semantics as scopes: a 429 or 404 cools
/// the model on this credential, a subscription-quota 429 the whole credential.
fn upstream_error(status: u16, body: &[u8]) -> ExecError {
    let mut retry_after = None;
    let scope = match status {
        429 => {
            retry_after = resets_in(body);
            if subscription_quota(body) {
                FailureScope::Credential
            } else {
                FailureScope::Model
            }
        }
        404 => {
            retry_after = Some(resets_in(body).unwrap_or(NOT_FOUND_COOLDOWN));
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
fn resets_in(body: &[u8]) -> Option<Duration> {
    let resets_at = gj::get(body, "error.resets_at").int();
    if resets_at <= 0 {
        return None;
    }
    let at = std::time::UNIX_EPOCH + Duration::from_secs(resets_at as u64);
    at.duration_since(std::time::SystemTime::now())
        .ok()
        .filter(|d| !d.is_zero())
}

/// `isMetaSubscriptionQuota` (status already 429).
fn subscription_quota(body: &[u8]) -> bool {
    if body.is_empty() {
        return false;
    }
    let message = gj::get(body, "error.message").str().to_lowercase();
    let code = gj::get(body, "error.code").str().to_lowercase();
    if message.contains("subscription quota") || message.contains("quota exhausted") {
        return true;
    }
    (code == "rate_limit_exceeded" || code.contains("quota")) && gj::get(body, "error.resets_at").exists()
}

/// `metaStreamEventError`: an `error` or `response.failed` event ends the response with
/// `error.code` as status when it is an HTTP error code, else 502.
fn stream_event_error(event: &[u8]) -> Option<ExecError> {
    let kind = gj::get(event, "type").bytes();
    if &*kind != b"error" && &*kind != b"response.failed" {
        return None;
    }
    let code = gj::get(event, "error.code").int();
    let status = if (400..=599).contains(&code) { code as u16 } else { 502 };
    Some(upstream_error(status, event))
}

/// `metaAsCompletedEvent`: a whole JSON body that is a terminal event or a bare response.
fn as_completed_event(data: &[u8]) -> Option<Vec<u8>> {
    let trimmed = go_trim_space(data);
    if !gj::valid(trimmed) {
        return None;
    }
    let kind = gj::get(trimmed, "type").bytes();
    if &*kind == b"response.completed" || &*kind == b"response.incomplete" {
        return Some(trimmed.to_vec());
    }
    if &*gj::get(trimmed, "object").bytes() == b"response" || gj::get(trimmed, "output").exists() {
        return gj::try_set_raw(br#"{"type":"response.completed"}"#, "response", trimmed).ok();
    }
    None
}

fn data_payload(line: &[u8]) -> Option<&[u8]> {
    line.strip_prefix(b"data:").map(go_trim_space)
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// translateMetaCompleted: the first terminal event of a buffered SSE body, with output
/// items rebuilt from `response.output_item.done`, then a whole-body JSON fallback. Every
/// event goes through the apply_patch bridge first; a bridge failure is the 502.
fn collect_completed(
    data: &[u8],
    apply_patch: &mut apply_patch_responses::State,
    translate: impl Fn(&[u8]) -> Result<Vec<u8>, ExecError>,
) -> Result<Vec<u8>, ExecError> {
    let bridge_error = || ExecError::local(502, FailureScope::Request, cpa_translate::APPLY_PATCH_UPSTREAM_ERROR);
    let mut items = OutputItems::default();
    for line in data.split(|b| *b == b'\n') {
        let Some(event) = data_payload(line) else {
            continue;
        };
        if let Some(error) = stream_event_error(event) {
            return Err(error);
        }
        let (events, error) = apply_patch.transform(event);
        if error.is_some() {
            return Err(bridge_error());
        }
        for event in events {
            let kind = gj::get(&event, "type").bytes();
            match &*kind {
                b"response.output_item.done" => items.collect(&lossy(&event)),
                b"response.completed" | b"response.incomplete" => {
                    return translate(items.patch(lossy(&event)).as_bytes());
                }
                _ => {}
            }
        }
    }
    if let Some(completed) = as_completed_event(data) {
        let patched = items.patch(lossy(&completed));
        let bridged = apply_patch
            .bridge
            .transform_non_stream(patched.as_bytes())
            .map_err(|_| bridge_error())?;
        return translate(&bridged);
    }
    if apply_patch.finish().is_err() {
        return Err(bridge_error());
    }
    Err(ExecError::local(408, FailureScope::Credential, DISCONNECTED))
}

/// The ExecuteStream loop: every scanned line goes through the apply_patch bridge and then
/// the pair's translator (`data:` lines normalized to `data: <trimmed>`), terminal events get
/// their collected output, and an error event, a bridge failure or a scan error ends the
/// stream. At EOF the bridge's `finish_stream` runs before a scan error is reported. The
/// translator emits frames the way the client's Go route writes them; usage is committed
/// only when the stream ends cleanly.
///
/// Responses clients get EnsureResponsesUsageDetails per line before translation; Go
/// applies it to each translated chunk, and the Responses translator only adds
/// `response.model` to creation events, so the two edits commute.
fn stream_events(
    upstream: ExecStream,
    translator: Box<dyn StreamTranslator>,
    responses_client: bool,
    bridge: apply_patch_responses::State,
    usage: DeferredUsage,
    capture: cpa_core::exec::CaptureSink,
) -> ExecStream {
    struct State {
        upstream: ExecStream,
        translator: Box<dyn StreamTranslator>,
        bridge: apply_patch_responses::State,
        usage: DeferredUsage,
        capture: cpa_core::exec::CaptureSink,
        items: OutputItems,
        ready: VecDeque<Result<Bytes, ExecError>>,
        done: bool,
        responses_client: bool,
    }
    fn bridge_error() -> ExecError {
        ExecError::local(502, FailureScope::Request, cpa_translate::APPLY_PATCH_UPSTREAM_ERROR)
    }
    impl State {
        /// Translates bridge output lines; false when a translator error ended the stream.
        fn translate_all(&mut self, lines: Vec<Vec<u8>>) -> bool {
            for line in lines {
                // Bridge frames end in a blank line; Go hands the translator the whole
                // chunk, which it trims, and a Responses route's framer ends the frame at
                // that blank line.
                let complete = line.ends_with(b"\n\n");
                let line = line.strip_suffix(b"\n\n").unwrap_or(&line);
                let line = if self.responses_client {
                    ensure_responses_usage_details(line)
                } else {
                    line.to_vec()
                };
                match self.translator.event(&line) {
                    Ok(mut frames) => {
                        if complete && self.responses_client {
                            frames.extend(self.translator.flush_frames());
                        }
                        self.ready.extend(frames.into_iter().filter(|f| !f.is_empty()).map(Ok));
                    }
                    Err(error) => {
                        self.done = true;
                        self.ready
                            .push_back(Err(ExecError::local(502, FailureScope::Request, error.0)));
                        return false;
                    }
                }
            }
            true
        }

        /// emitTranslatedLine: the bridge, the translator, StopApplyPatchStream, then the
        /// bridge's own failure.
        fn emit(&mut self, line: Vec<u8>) {
            let (lines, bridge_failed) = self.bridge.stream(&line);
            if !self.translate_all(lines) {
                return;
            }
            // StopApplyPatchStream: the rejected call's frames, then a 502. Go's Meta loop
            // does not finalize tool input at EOF.
            if self.translator.tool_input_failed() || bridge_failed.is_some() {
                self.fail(bridge_error());
            }
        }

        /// Ends the stream with `error`. Go stops translating; the Responses route first
        /// flushes the frame it is still joining (other routes hold nothing back).
        fn fail(&mut self, error: ExecError) {
            self.done = true;
            let flushed = self.translator.flush_frames();
            self.ready.extend(flushed.into_iter().filter(|f| !f.is_empty()).map(Ok));
            self.ready.push_back(Err(error));
        }

        fn line(&mut self, line: &[u8]) {
            let Some(event) = data_payload(line) else {
                return self.emit(line.to_vec());
            };
            if let Some(error) = stream_event_error(event) {
                crate::kimi_http::capture_error(&self.capture, &error);
                return self.fail(error);
            }
            let kind = gj::get(event, "type").bytes().into_owned();
            let event = match kind.as_slice() {
                b"response.output_item.done" => {
                    self.items.collect(&lossy(event));
                    event.to_vec()
                }
                b"response.completed" | b"response.incomplete" => self.items.patch(lossy(event)).into_bytes(),
                _ => event.to_vec(),
            };
            let mut out = b"data: ".to_vec();
            out.extend_from_slice(&event);
            self.emit(out);
        }

        /// The end of the upstream lines: `FinishStream`, then the scan error or a clean end.
        fn end(&mut self, scan_error: Option<ExecError>) {
            self.done = true;
            let (events, bridge_failed) = self.bridge.finish_stream();
            if !self.translate_all(events) {
                return;
            }
            if bridge_failed.is_some() {
                return self.fail(bridge_error());
            }
            if let Some(error) = scan_error {
                return self.fail(error);
            }
            match self.translator.finish() {
                Ok(frames) => self.ready.extend(frames.into_iter().filter(|f| !f.is_empty()).map(Ok)),
                Err(error) => {
                    self.ready
                        .push_back(Err(ExecError::local(502, FailureScope::Request, error.0)));
                    return;
                }
            }
            self.usage.commit();
        }
    }
    futures_util::stream::unfold(
        State {
            upstream,
            translator,
            bridge,
            usage,
            capture,
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
                    Some(Err(error)) => st.end(Some(error)),
                    None => st.end(None),
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

/// Go `helps.RefreshAuthViaHome` for a dispatched credential: Home mints (or refreshes)
/// and returns the auth, whose metadata replaces this attempt's. Its `base_url`,
/// `api_key` and `access_token` attributes come along when the metadata lacks them,
/// so [`creds`] and `ensure_auth` see what Home's auth holds.
async fn refresh_via_home(credential: &Credential) -> Result<MetadataPatch, ExecError> {
    let client = cpa_home::client::current();
    let refreshed = cpa_home::refresh::refresh(
        client.as_ref(),
        &cpa_core::config::credentials::auth_index(credential),
        &cpa_core::config::credentials::access_token_sha256(credential),
    )
    .await
    .map_err(|error| {
        tracing::warn!("meta executor: {}", error.log);
        let mut failure = ExecError::local(error.status, FailureScope::Credential, "");
        failure.body = Bytes::from(error.body);
        failure.direct = error.direct;
        failure
    })?;
    let metadata = refreshed
        .auth
        .get("metadata")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut patch = MetadataPatch {
        remove: credential
            .metadata
            .keys()
            .filter(|k| !metadata.contains_key(*k))
            .cloned()
            .collect(),
        set: metadata,
    };
    if let Some(attributes) = refreshed.auth.get("attributes").and_then(Value::as_object) {
        for key in ["base_url", "api_key", "access_token"] {
            if let Some(value) = attributes.get(key).filter(|v| v.is_string())
                && !patch.set.contains_key(key)
            {
                patch.set.insert(key.into(), value.clone());
            }
        }
    }
    Ok(patch)
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
