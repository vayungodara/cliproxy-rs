//! Claude executor: Anthropic Messages and count_tokens with OAuth tokens or API keys.
//!
//! The request pipeline follows claude_executor_execute.go / _stream.go / _tokens.go
//! in order: client detection, session identity, translation, cloaking, diagnostics,
//! cache policy, betas, MCP aliases, credential identity, CCH signing, headers, and
//! the native transport. Responses restore aliases and record billing continuity.
//!
//! First-party Anthropic gets the native Claude Code transport (tls.rs); every other
//! origin gets Go's standard transport (proxy.rs), exactly as Go's fallback round
//! tripper does. Delegating executors (Kimi) pass a [`Delegation`].
//!
//! Thinking (`cpa_common::thinking`), signature sanitizing (`cpa_common::signature`)
//! and request translation (`cpa_translate::translate_request`) are the shared modules.
//!
//! Payload rules (`cpa_common::payload`) run after cloaking, as in Go, and
//! reconcile.rs repairs the cloak's model-specific additions afterwards.
//!
//! Usage reports go through `ExecRequest::usage` with Go's reporter calls: the body
//! sent, the upstream body and lines, the TTFT marks, `Publish` on native Execute,
//! required usage on translated Execute and ExecuteStream (no `EnsurePublished`), and
//! tokenless failures where Go's `TrackFailure` or apply_patch hooks publish them. Go 6fecc6e has no Vertex delegation to
//! this executor: `claudeCCHUpstreamVertex` has no caller, so CCH signing stays
//! Anthropic-only.
//!
//! In Home mode, device profiles come from Home KV (`profile::resolve_required`) and
//! so do credential identities (`crate::oauth`); request logs belong to the server.

mod alias;
mod betas;
mod cloak;
mod detect;
mod headers;
mod identity;
#[cfg(test)]
pub(crate) mod kv_test;
mod profile;
mod reconcile;
mod replay;
mod session;
mod settings;
mod signals;
mod signing;
mod stream;

use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use cpa_core::config::Config;
use cpa_core::credential::{Credential, MetadataPatch};
use cpa_core::exec::{
    CaptureEvent, CaptureSink, ExecError, ExecRequest, ExecResponse, FailureScope, Operation, ResponseBody,
    UpstreamRequest,
};
use cpa_core::format::Format;
use futures_util::StreamExt;

use crate::oauth::{self, OAuth};
use crate::proxy::{GoClients, GoHeaders, Proxy, Route};
use crate::rawjson;
use crate::tls::Transport;
use crate::upstream::{Decoded, decode_upstream};
use crate::{quota, tokens, translate};
use settings::Settings;

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Advertised in `CLIProxyAPI/<version>` when a caller-owned request has no User-Agent.
pub(crate) const PROXY_VERSION: &str = env!("CARGO_PKG_VERSION");

pub use crate::proxy::Hooks;

/// The fields of a `"type": "claude"` credential this executor needs.
pub struct ClaudeView<'a> {
    pub access_token: &'a str,
    pub email: &'a str,
}

impl<'a> ClaudeView<'a> {
    pub fn new(credential: &'a Credential) -> Result<Self, ExecError> {
        let access_token = credential
            .attributes
            .get("api_key")
            .map(String::as_str)
            .filter(|t| !t.is_empty())
            .or_else(|| credential.str("access_token"))
            .filter(|t| !t.is_empty())
            .ok_or_else(|| ExecError::local(401, FailureScope::Credential, "claude credential has no access_token"))?;
        Ok(Self {
            access_token,
            email: credential.str("email").unwrap_or_default(),
        })
    }
}

/// The first-party Anthropic transport.
enum Native {
    /// Production or harness: native and OAuth profiles per effective proxy.
    Transport(Arc<Transport>),
    /// Unit tests against plain HTTP mocks.
    Fixed(wreq::Client),
}

/// What a delegating executor changes in the Claude pipeline. Go embeds a
/// `ClaudeExecutor` configured this way (`NewKimiExecutor`).
#[derive(Clone, Copy, Default)]
pub struct Delegation {
    /// `upstreamModelNormalizer`: the model sent upstream, given the base model.
    pub upstream_model: Option<fn(&str) -> String>,
    /// `countTokensUpstream`: count on the upstream for every origin and credential.
    pub count_upstream: bool,
    /// `requestLogProvider`: the provider request logs name (Kimi logs `kimi`); `None`
    /// is the executor's identifier, `claude`.
    pub request_log_provider: Option<&'static str>,
}

pub struct ClaudeExecutor {
    native: Native,
    /// Go standard-transport clients for every other origin.
    go: Arc<GoClients>,
    base_url: String,
    oauth: OAuth,
    replay: Arc<replay::ReplayCache>,
    /// Passive quota snapshots per credential, for the management credential entry.
    quota: Arc<crate::quota::Observations>,
}

impl ClaudeExecutor {
    /// Production executor: native wire profile with the process-global transport.
    pub fn new(base_url: impl Into<String>) -> wreq::Result<Self> {
        Ok(Self::with_transport(
            Arc::new(Transport::new(Hooks::default())),
            Hooks::default(),
            base_url,
        ))
    }

    /// The production profile with test trust roots and dial overrides (harness).
    /// OAuth refresh uses the same transport, so it cannot escape the hooks.
    pub fn with_hooks(hooks: Hooks, base_url: impl Into<String>) -> Self {
        Self::with_transport(Arc::new(Transport::new(hooks.clone())), hooks, base_url)
    }

    fn with_transport(transport: Arc<Transport>, hooks: Hooks, base_url: impl Into<String>) -> Self {
        Self {
            oauth: OAuth::with_transport(transport.clone()),
            replay: replay::shared(),
            quota: Arc::default(),
            native: Native::Transport(transport),
            go: Arc::new(GoClients::new(hooks)),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }

    /// A caller-built client for every unproxied request (plain mocks). First-party
    /// requests keep the native header order on it, but not the native TLS profile.
    pub fn with_client(client: wreq::Client, base_url: impl Into<String>) -> Self {
        Self {
            oauth: OAuth::new(client.clone()),
            replay: replay::shared(),
            quota: Arc::default(),
            go: Arc::new(GoClients::with_default(client.clone())),
            native: Native::Fixed(client),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }

    /// The latest quota signals each Claude credential's responses carried (Go
    /// `QuotaState.ObserveResponseHeadersForProvider`).
    pub fn quota(&self) -> &crate::quota::Observations {
        &self.quota
    }

    /// Overrides the OAuth service (local token/profile mocks).
    pub fn with_oauth(mut self, oauth: OAuth) -> Self {
        self.oauth = oauth;
        self
    }

    pub fn needs_prepare(&self, credential: &Credential, cfg: &Config) -> bool {
        self.needs_prepare_at(credential, cfg, chrono::Utc::now())
    }

    pub fn needs_prepare_at(&self, credential: &Credential, _cfg: &Config, now: chrono::DateTime<chrono::Utc>) -> bool {
        oauth::needs_prepare(credential, now)
    }

    pub async fn prepare(&self, credential: &Credential, cfg: &Config) -> Result<MetadataPatch, ExecError> {
        self.oauth.prepare(credential, &Proxy::effective(credential, cfg)).await
    }

    fn native_client(&self, proxy: &Proxy) -> Result<wreq::Client, ExecError> {
        match &self.native {
            Native::Fixed(client) => Ok(client.clone()),
            Native::Transport(t) => t
                .clients(proxy)
                .map(|clients| clients.native.clone())
                .map_err(|_| ExecError::local(502, FailureScope::Transport, "upstream request failed")),
        }
    }

    pub async fn execute(
        &self,
        credential: &Credential,
        req: ExecRequest,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        self.execute_delegated(credential, req, cfg, Delegation::default())
            .await
    }

    /// [`Self::execute`] on behalf of a delegating executor.
    pub async fn execute_delegated(
        &self,
        credential: &Credential,
        req: ExecRequest,
        cfg: &Config,
        delegation: Delegation,
    ) -> Result<ExecResponse, ExecError> {
        tracing::trace!(target: "cpa_latency", stage = "executor");
        // Go executorForAuth: an API-key credential runs with cfg.ForAPIKey(), which
        // zeroes the v8 OAuth-only provider settings.
        let scoped = if cpa_core::registry::dynamic::auth_kind(credential) == Some("apikey") {
            cfg.for_api_key()
        } else {
            std::borrow::Cow::Borrowed(cfg)
        };
        let cfg = &*scoped;
        // Go never checks for a token here: an empty key counts locally, or reaches the
        // upstream unauthenticated and gets its answer.
        if req.alt.as_deref() == Some("responses/compact") {
            // Go returns before it creates the usage reporter: no record at all.
            req.usage.discard();
            return Err(ExecError::local(
                501,
                FailureScope::Request,
                "/responses/compact not supported",
            ));
        }
        if req.response_format != Format::Claude && cpa_translate::pair(req.response_format, Format::Claude).is_none() {
            return Err(ExecError::local(
                501,
                FailureScope::Request,
                "Claude response translation pair is not registered",
            ));
        }
        let ctx = Ctx::new(self, credential, &req, cfg, delegation);
        match req.operation {
            Operation::Generate => self.generate(ctx, req).await,
            Operation::CountTokens => self.count_tokens(ctx, req, delegation.count_upstream).await,
        }
    }

    /// Execute / ExecuteStream around the compat thinking replay: restore before
    /// translation; `generate_with` clears applied replay the upstream rejected.
    async fn generate(&self, ctx: Ctx<'_>, mut req: ExecRequest) -> Result<ExecResponse, ExecError> {
        let attr = |k: &str| ctx.credential.attributes.get(k).map(String::as_str).unwrap_or_default();
        let gate = replay::Gate {
            credential: ctx.credential,
            api_key: &ctx.api_key,
            base_url: attr("base_url"),
            base_model: &ctx.base_model,
            is_compat: ctx.is_compat,
            oauth_token: ctx.oauth_token,
        };
        let scope = replay::prepare(&self.replay, &gate, &mut req).await;
        let result = self.generate_with(ctx, req, scope.as_ref()).await;
        match (result, scope) {
            // wrapClaudeThinkingReplayStream wraps the stream ExecuteStream returns,
            // after translation; the response waits for the cache writes it caused.
            (Ok(mut response), Some(scope)) => {
                let writes = scope.writes.clone();
                match response.body {
                    ResponseBody::Stream(events) => {
                        response.body = ResponseBody::Stream(writes.gate(scope.wrap(events)));
                    }
                    body => {
                        response.body = body;
                        writes.settle().await;
                    }
                }
                Ok(response)
            }
            (Err(error), Some(scope)) => {
                scope.writes.settle().await;
                Err(error)
            }
            (result, None) => result,
        }
    }

    async fn generate_with(
        &self,
        ctx: Ctx<'_>,
        req: ExecRequest,
        replay: Option<&replay::Scope>,
    ) -> Result<ExecResponse, ExecError> {
        let upstream_stream = req.stream || req.response_format != Format::Claude;
        // reporter.SetUpstreamModel when the delegation renames the model.
        if ctx.upstream_model != ctx.base_model {
            req.usage.upstream_model(&ctx.upstream_model);
        }
        let translated = translate::request(&req, ctx.codex, &ctx.base_model, ctx.is_compat)?;
        let original_translated = translate::original(&req, &translated, ctx.codex, &ctx.base_model, ctx.is_compat)?;
        tracing::trace!(target: "cpa_latency", stage = "translated");
        let mut prepared = ctx
            .prepare_messages(&req, &translated, &original_translated, upstream_stream)
            .await?;
        // reporter.SetTranslatedReasoningEffort on the body sent upstream.
        if req.usage.enabled() {
            req.usage.request(Format::Claude, prepared.body.as_bytes());
        }
        let response = self
            .send(&ctx, &mut prepared, "/v1/messages", Some(&req.usage), req.capture())
            .await;
        let response = response.inspect_err(|error| {
            // shouldClearKimiThinkingReplayAfterError: an upstream rejection of applied replay.
            if let Some(scope) = replay.filter(|s| s.applied)
                && replay::upstream_rejects(error)
            {
                scope.clear();
            }
        })?;
        let reverse = prepared.reverse.clone();
        let continuity = prepared.continuity.clone();
        let request_id = header_value(&response.headers, "request-id");
        let fast = ctx.first_party && prepared.fast;
        let wrap = |e| fast_request_error(fast, e);
        let body = match response.body {
            ResponseBody::Stream(raw) if upstream_stream && req.stream && req.operation == Operation::Generate => {
                let done: stream::OnComplete = Box::new(move |message_id| {
                    session::commit(
                        &continuity.key,
                        continuity.sequence,
                        &message_id,
                        &request_id,
                        &continuity.prompt_id,
                    );
                });
                // ExecuteStream publishes through its stream buffer only (no
                // EnsurePublished): a stream that never carried usage records nothing.
                req.usage.usage_required();
                let (usage, capture) = (req.usage.clone(), req.capture().clone());
                let relayed = if req.response_format == Format::Claude {
                    stream::relay(raw, reverse, done, usage, capture)
                } else {
                    stream::relay_translated(raw, reverse, done, usage, capture)
                };
                ResponseBody::Stream(relayed.map(move |r| r.map_err(|e| fast_request_error(fast, e))).boxed())
            }
            ResponseBody::Stream(raw) => {
                let capture = req.capture();
                let data = collect(raw).await.map_err(|e| {
                    capture_error(capture, &e);
                    wrap(e)
                })?;
                capture.record(CaptureEvent::ResponseChunk(&data));
                if upstream_stream {
                    // Translated Execute publishes with streamUsage.Publish only: no usage
                    // seen, no record. A restore failure below keeps the usage seen so far
                    // (streamUsage.PublishFailure), which the server does for Claude.
                    req.usage.usage_required();
                    stream::validate_buffered(&data).map_err(|e| {
                        capture_error(capture, &e);
                        wrap(e)
                    })?;
                    let id = stream::buffered_message_id(&data);
                    session::commit(
                        &continuity.key,
                        continuity.sequence,
                        &id,
                        &request_id,
                        &continuity.prompt_id,
                    );
                    let mut out = Vec::with_capacity(data.len());
                    for (i, line) in data.split(|b| *b == b'\n').enumerate() {
                        if i > 0 {
                            out.push(b'\n');
                        }
                        // ObserveResponseModel and ObserveClaudeStream, before restore.
                        req.usage.response_line(Format::Claude, line);
                        out.extend(stream::restore_line(line, &reverse).map_err(|m| {
                            let error =
                                plain_error(format!("restore Claude OAuth tool name from streaming response: {m}"));
                            capture_error(capture, &error);
                            wrap(error)
                        })?);
                    }
                    if let Some(scope) = replay {
                        scope.store_response(&out);
                    }
                    ResponseBody::Buffered(Bytes::from(out))
                } else {
                    // ObserveResponseModel and ParseClaudeUsage of the upstream body.
                    req.usage.response_body(Format::Claude, &data);
                    let text = String::from_utf8_lossy(&data).into_owned();
                    let id = rawjson::string(&text, "id").trim().to_owned();
                    session::commit(
                        &continuity.key,
                        continuity.sequence,
                        &id,
                        &request_id,
                        &continuity.prompt_id,
                    );
                    // A failed restore returns before ParseClaudeUsage: Go's deferred
                    // TrackFailure publishes no tokens. The error is plain (wrapped for a
                    // fast request with the 2xx status), so Go records no status.
                    let restored = alias::restore_response(&text, &reverse).map_err(|m| {
                        let message = format!("restore Claude OAuth tool name from response: {m}");
                        req.usage.publish_failure(0, &message);
                        capture.record(CaptureEvent::ResponseError(&message));
                        wrap(plain_error(message))
                    })?;
                    if let Some(scope) = replay {
                        scope.store_response(restored.as_bytes());
                    }
                    // reporter.Publish(ParseClaudeUsage(data)): native Execute always
                    // publishes, with whatever usage the body carried.
                    req.usage.publish();
                    ResponseBody::Buffered(Bytes::from(restored))
                }
            }
            ResponseBody::Buffered(_) => unreachable!("send returns raw streams"),
        };
        let response = ExecResponse {
            status: response.status,
            headers: response.headers,
            body,
        };
        if req.response_format == Format::Claude {
            return Ok(response);
        }
        // Translated clients: framed events through the registered stream translator.
        let response = match response.body {
            ResponseBody::Buffered(_) => response,
            ResponseBody::Stream(events) => ExecResponse {
                body: ResponseBody::Stream(events),
                ..response
            },
        };
        translate::response(req, translated, response).await
    }

    async fn count_tokens(&self, ctx: Ctx<'_>, req: ExecRequest, upstream: bool) -> Result<ExecResponse, ExecError> {
        let translated = translate::request(&req, ctx.codex, &ctx.base_model, ctx.is_compat)?;
        // sdktranslator.TranslateTokenCount(to=claude, responseFormat, count, raw).
        let render = |raw: &[u8]| {
            let count = gjson::get(&String::from_utf8_lossy(raw), "input_tokens").i64();
            Bytes::from(cpa_translate::translate_token_count(
                req.response_format,
                Format::Claude,
                count,
                raw,
            ))
        };
        if !upstream && (ctx.api_key.trim().is_empty() || !ctx.first_party) {
            let mut body = ctx.apply_thinking(&req, String::from_utf8_lossy(&translated).into_owned())?;
            if ctx.rebuild_mid_system() {
                body = rebuild_mid_system(&body);
            }
            let body = sanitize_for_upstream(&body, &ctx.base_model, ctx.is_compat);
            return Ok(ExecResponse {
                status: 200,
                headers: Default::default(),
                body: ResponseBody::Buffered(render(&tokens::count(body.as_bytes())?)),
            });
        }
        let mut prepared = ctx.prepare_count(&req, &translated).await?;
        let capture = req.capture();
        let response = self
            .send(&ctx, &mut prepared, "/v1/messages/count_tokens", None, capture)
            .await?;
        let body = match response.body {
            ResponseBody::Stream(raw) => collect(raw).await.inspect_err(|e| capture_error(capture, e))?,
            ResponseBody::Buffered(b) => b,
        };
        capture.record(CaptureEvent::ResponseChunk(&body));
        Ok(ExecResponse {
            status: response.status,
            headers: response.headers,
            body: ResponseBody::Buffered(render(&body)),
        })
    }

    /// Sends one Messages or count_tokens request. `usage` receives Go's TTFT marks
    /// (`TrackHTTPClient`): the round trip starts, then the first raw body byte arrives,
    /// before decoding and for any status.
    // ponytail: Go's http.Client drains up to 2 KiB of a redirect response's body
    // through the tracked transport, so a redirect with a body marks the first byte at
    // that hop; this marks it on the final response only.
    async fn send(
        &self,
        ctx: &Ctx<'_>,
        prepared: &mut Prepared,
        path: &str,
        usage: Option<&cpa_core::exec::UsageSink>,
        capture: &CaptureSink,
    ) -> Result<RawResponse, ExecError> {
        let usage = usage.filter(|u| u.enabled());
        let url = format!("{}{path}?beta=true", ctx.base_url);
        let fast = ctx.first_party && prepared.fast;
        // The body moves into the request; nothing reads it after the send.
        let body = Bytes::from(std::mem::take(&mut prepared.body));
        let prepared = &*prepared;
        let mut headers = GoHeaders::new();
        for (name, value) in &prepared.headers {
            headers.add_raw(name, value.as_str());
        }
        // Go's fallbackRoundTripper, per hop: the native Claude Code transport and
        // ordered writer for Anthropic, http.DefaultTransport (or the proxy transport)
        // for every other origin, behind one redirect-following http.Client.
        let route = |hop: &url::Url| {
            Ok(if tokens::first_party(hop.as_str()) {
                Route {
                    client: self.native_client(&ctx.proxy)?,
                    order: Some(prepared.order.clone()),
                }
            } else {
                Route {
                    client: self.go.get(&ctx.proxy),
                    order: None,
                }
            })
        };
        // helps.RecordAPIRequest: the header map the executor built (Go spelling, the
        // transport's own headers excluded), unmasked; every physical send.
        if capture.enabled() {
            let (auth_type, auth_value) = cpa_core::registry::dynamic::account_info(ctx.credential);
            capture.record(CaptureEvent::Request(UpstreamRequest {
                url: &url,
                method: "POST",
                headers: &prepared.headers,
                body: &body,
                provider: ctx.log_provider,
                auth_id: &ctx.credential.id,
                auth_label: &ctx.credential.label,
                auth_type,
                auth_value: &auth_value,
            }));
        }
        if let Some(usage) = usage {
            usage.round_trip_started();
        }
        tracing::trace!(target: "cpa_latency", stage = "prepared");
        let mut upstream = crate::proxy::send_routed(&route, &url, headers, body, None)
            .await
            .map_err(|e| {
                capture_error(capture, &e);
                fast_request_error(fast, e)
            })?;
        if capture.enabled() {
            capture.record(CaptureEvent::ResponseMetadata(
                upstream.status,
                &go_headers(&upstream.headers),
            ));
        }
        if let Some(usage) = usage.cloned() {
            let mut marked = false;
            upstream.body = upstream
                .body
                .inspect(move |chunk| {
                    if !marked && chunk.as_ref().is_ok_and(|c| !c.is_empty()) {
                        marked = true;
                        usage.first_byte();
                    }
                })
                .boxed();
        }
        // MarkResult observes the headers of every upstream answer to a Messages request
        // from a Claude credential, whatever its status or body (Go records them before
        // reading the body); count_tokens results skip observation in Go's conductor.
        // ponytail: Go keys the model state by the conductor's state model; the executor
        // sees the upstream base model, as Codex's observation does.
        if path == "/v1/messages" && ctx.credential.provider.trim().eq_ignore_ascii_case("claude") {
            self.quota
                .observe_model(&ctx.credential.id, &ctx.base_model, &upstream.headers);
        }
        finish(
            decode_upstream(upstream).await,
            fast,
            ctx.settings.model_level_cooling,
            capture,
        )
        .await
    }
}

/// `httpResp.Header.Clone()` for request logs: Go's canonical names, every value.
fn go_headers(headers: &http::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                crate::proxy::canonical_header(name.as_str()),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

/// `helps.RecordAPIResponseError` with the error's text.
fn capture_error(capture: &CaptureSink, error: &ExecError) {
    if capture.enabled() {
        capture.record(CaptureEvent::ResponseError(&String::from_utf8_lossy(&error.body)));
    }
}

/// The response half of Go's Execute, ExecuteStream and countTokensUpstream: the
/// decoded 2xx body, or the error Go builds from a non-2xx answer. An undecodable or
/// unreadable error body keeps the upstream status and says why.
/// Request logs record a decode failure, an unreadable error body, and the error body
/// itself, at Go's sites.
async fn finish(
    decoded: Decoded,
    fast: bool,
    model_level_cooling: bool,
    capture: &CaptureSink,
) -> Result<RawResponse, ExecError> {
    let Decoded { status, headers, body } = decoded;
    if (200..300).contains(&status) {
        let body = body.map_err(|m| {
            capture.record(CaptureEvent::ResponseError(&m));
            fast_request_error(fast, plain_error(m))
        })?;
        return Ok(RawResponse {
            status,
            headers,
            body: ResponseBody::Stream(body),
        });
    }
    let data = match body {
        Err(m) => {
            capture.record(CaptureEvent::ResponseError(&m));
            let message = format!("failed to decode error response body: {m}");
            let error = upstream_error(status, headers, Bytes::from(message), false, model_level_cooling);
            return Err(fast_request_error(fast, error));
        }
        Ok(mut body) => {
            // ponytail: Go reads error bodies without a bound; 16 MiB covers any real
            // provider error without letting a hostile upstream exhaust a small VPS.
            let mut data = BytesMut::new();
            loop {
                match body.next().await {
                    Some(Ok(chunk)) if data.len() < crate::proxy::MAX_ERROR_BODY => data.extend_from_slice(&chunk),
                    Some(Ok(_)) | None => break data.freeze(),
                    Some(Err(e)) => {
                        let reason = if e.scope == FailureScope::Transport {
                            "unexpected EOF".into()
                        } else {
                            String::from_utf8_lossy(&e.body).into_owned()
                        };
                        capture.record(CaptureEvent::ResponseError(&reason));
                        break Bytes::from(format!("failed to read error response body: {reason}"));
                    }
                }
            }
        }
    };
    capture.record(CaptureEvent::ResponseChunk(&data));
    Err(upstream_error(status, headers, data, fast, model_level_cooling))
}

struct RawResponse {
    status: u16,
    headers: http::HeaderMap,
    body: ResponseBody,
}

fn header_value(headers: &http::HeaderMap, name: &str) -> String {
    detect::header(headers, name).to_owned()
}

async fn collect(mut raw: cpa_core::exec::ExecStream) -> Result<Bytes, ExecError> {
    let mut out = BytesMut::new();
    while let Some(chunk) = raw.next().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out.freeze())
}

/// A non-2xx upstream answer. `headers` are already decoded (no Content-Encoding or
/// Content-Length). Fast requests to Anthropic answer the client as sent
/// (`newClaudeFastDirectResponseError`); everything else is classified
/// (`classifyClaudeUpstreamErrorWithCooling`).
fn upstream_error(
    status: u16,
    headers: http::HeaderMap,
    body: Bytes,
    fast: bool,
    model_level_cooling: bool,
) -> ExecError {
    if fast {
        // Only a genuine shared-window 429 leaves the request's scope.
        let rate_limited = status == 429;
        return ExecError {
            status,
            scope: if rate_limited && quota::shared_rejection(&headers) {
                FailureScope::Credential
            } else {
                FailureScope::Request
            },
            retry_after: rate_limited.then(|| quota::rate_limit_reset(&headers)).flatten(),
            headers: Box::new(headers),
            body,
            direct: true,
        };
    }
    let error = ExecError {
        status,
        scope: crate::upstream::scope_for(status),
        retry_after: None,
        headers: Box::new(headers),
        body: if body.is_empty() {
            Bytes::from(format!("status {status}"))
        } else {
            body
        },
        direct: false,
    };
    quota::classify(error, model_level_cooling)
}

/// A Go error with no status or scope (`fmt.Errorf`): the handler answers 500, and the
/// conductor neither cools the credential nor treats the request as unservable.
pub(crate) fn plain_error(message: impl Into<String>) -> ExecError {
    ExecError::local(500, FailureScope::Transport, message)
}

/// `wrapClaudeFastRequestError`: any other failure of a fast request stops at the
/// caller instead of failing over. Only a cause that is explicitly credential-scoped
/// (a classified shared-window 429) keeps that scope; status-derived scope does not.
fn fast_request_error(fast: bool, mut error: ExecError) -> ExecError {
    if fast {
        let shared_window =
            error.scope == FailureScope::Credential && error.status == 429 && quota::shared_rejection(&error.headers);
        if !shared_window {
            error.scope = FailureScope::Request;
        }
    }
    error
}

/// Request facts shared by every stage.
struct Ctx<'a> {
    credential: &'a Credential,
    settings: Settings,
    api_key: String,
    base_url: String,
    first_party: bool,
    proxy: Proxy,
    base_model: String,
    /// The model sent upstream (`upstreamModel`): the base model unless a delegating
    /// executor normalizes it.
    upstream_model: String,
    /// `isKimiMessagesUpstream`.
    kimi: bool,
    /// `upstreamRequestLogProvider`: the provider request logs name.
    log_provider: &'static str,
    /// Normalized execution-session ID (websocket executions), or empty.
    execution: String,
    /// `cliproxyauth.ResolvedModelInfo`: the capabilities dispatch bound to this attempt.
    resolved: Option<cpa_common::thinking::ModelCaps>,
    /// `helps.APIKeyModelIsCompat`.
    is_compat: bool,
    /// The Codex client rewrite settings of this config snapshot (Go `e.cfg`).
    codex: cpa_common::codex_client::Settings,
    /// `requests.payload` rules of this config snapshot (parsed once per snapshot).
    payload_rules: std::sync::Arc<cpa_common::payload::Rules>,
    /// Real Claude OAuth token (`sk-ant-oat`).
    oauth_token: bool,
    /// `fp.ProfileClaudeCodeCLI`: OAuth token or `fingerprint-profile: claude-code-cli`.
    cli_profile: bool,
    /// Bearer vs x-api-key on first-party (`claudeCredentialUsesOAuth`).
    bearer: bool,
    /// Local date for the currentDate reminder.
    // ponytail: only UTC and the process zone are resolvable without a tz database;
    // other IANA names fall back to local time, as Go does for unknown zones.
    today: String,
}

struct Prepared {
    body: String,
    headers: Vec<(String, String)>,
    order: Vec<String>,
    reverse: alias::Reverse,
    continuity: session::Continuity,
    fast: bool,
}

/// `config.NormalizeClaudeFingerprintProfile`: unknown values are the default ("").
fn normalize_fingerprint_profile(raw: &str) -> &'static str {
    match raw.trim().to_lowercase().as_str() {
        "claude-code-cli" | "oauth-cli" => "claude-code-cli",
        _ => "",
    }
}

/// The executor keeps bodies as text; shared byte APIs never split UTF-8 they were given.
/// `String::from_utf8_lossy` with the fast validator first: request bodies are
/// almost always valid UTF-8, and the lossy decoder's own check is several times slower.
fn utf8(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    std::str::from_utf8(bytes).map_or_else(|_| String::from_utf8_lossy(bytes), std::borrow::Cow::Borrowed)
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// `helps.SetStringIfDifferent`.
fn set_string_if_different(body: &str, path: &str, value: &str) -> String {
    let current = rawjson::get(body, path);
    if current.kind() == gjson::Kind::String && current.str() == value {
        return body.to_owned();
    }
    rawjson::set_str(body, path, value)
}

/// `isKimiMessagesUpstream`: a Kimi credential, or Kimi's API host.
fn kimi_upstream(provider: &str, base_url: &str) -> bool {
    let provider = provider.trim().to_lowercase();
    if matches!(provider.as_str(), "kimi" | "kimi-ai" | "kimi.ai" | "kimi.com") {
        return true;
    }
    url::Url::parse(base_url.trim()).is_ok_and(|u| {
        u.host_str()
            .is_some_and(|h| h.eq_ignore_ascii_case("api.kimi.com") || h.eq_ignore_ascii_case("api.kimi.ai"))
    })
}

/// `helps.ClaudeCLIAuthIdentitySeed`.
// ponytail: Go falls back to the auth index and file name when the ID is empty; every
// loaded or synthesized credential here has an ID.
fn identity_seed(credential: &Credential) -> String {
    let id = credential.id.trim();
    if id.is_empty() {
        String::new()
    } else {
        format!("auth-id|{id}")
    }
}

/// `thinking.ParseSuffix`: `model(level)` → `model`.
fn base_model(model: &str) -> String {
    cpa_common::thinking::parse_suffix(model).model_name
}

/// `sanitizeClaudeMessagesForClaudeUpstreamWithDebug`: Claude-family targets (and
/// compat models, which keep empty thinking blocks) go through the shared Messages
/// signature sanitizer; then empty web_search domain lists are removed.
fn sanitize_for_upstream(body: &str, base_model: &str, preserve_empty_thinking: bool) -> String {
    use cpa_common::signature::{Provider, provider_from_model_name, sanitize_claude_messages_for_claude_upstream};
    let mut body = body.to_owned();
    if provider_from_model_name(base_model) == Provider::Claude || preserve_empty_thinking {
        let sanitized =
            sanitize_claude_messages_for_claude_upstream(body.as_bytes(), base_model, preserve_empty_thinking).0;
        body = text(sanitized);
    }
    // sanitizeClaudeWebSearchDomains: one pass over the tools, then the deletes.
    let mut empty = Vec::new();
    for (t, tool) in rawjson::get(&body, "tools").array().iter().enumerate() {
        if !tool.get("type").str().starts_with("web_search_") {
            continue;
        }
        for field in ["allowed_domains", "blocked_domains"] {
            let v = tool.get(field);
            if v.kind() == gjson::Kind::Array && v.array().is_empty() {
                empty.push(format!("tools.{t}.{field}"));
            }
        }
    }
    for path in empty {
        body = rawjson::delete(&body, &path);
    }
    body
}

/// `extractAndRemoveBetas`.
fn extract_betas(body: &str) -> (Vec<String>, String) {
    let v = rawjson::get(body, "betas");
    if !v.exists() {
        return (Vec::new(), body.to_owned());
    }
    let betas = if v.kind() == gjson::Kind::Array {
        v.array()
            .iter()
            .map(|b| b.str().trim().to_owned())
            .filter(|b| !b.is_empty())
            .collect()
    } else {
        Some(v.str().trim().to_owned())
            .filter(|b| !b.is_empty())
            .into_iter()
            .collect()
    };
    (betas, rawjson::delete(body, "betas"))
}

impl<'a> Ctx<'a> {
    fn new(
        exec: &ClaudeExecutor,
        credential: &'a Credential,
        req: &ExecRequest,
        cfg: &Config,
        delegation: Delegation,
    ) -> Self {
        let settings = Settings::from_config(cfg);
        let attr = |k: &str| credential.attributes.get(k).map(String::as_str).unwrap_or_default();
        let api_key = if attr("api_key").is_empty() {
            credential.str("access_token").unwrap_or_default().to_owned()
        } else {
            attr("api_key").to_owned()
        };
        let base_url = if attr("base_url").is_empty() {
            exec.base_url.clone()
        } else {
            attr("base_url").trim_end_matches('/').to_owned()
        };
        let oauth_token = api_key.contains("sk-ant-oat");
        // claudeFingerprintProfileFromConfig: the credential's own value, else the key's.
        let own = Some(attr("fingerprint_profile"))
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                ["fingerprint_profile", "fingerprint-profile"]
                    .iter()
                    .filter_map(|k| credential.str(k))
                    .find(|s| !s.trim().is_empty())
            })
            .map(normalize_fingerprint_profile)
            .unwrap_or_default();
        let profile = if own.is_empty() {
            settings
                .key_for(&api_key, attr("base_url"))
                .map(|k| normalize_fingerprint_profile(&k.fingerprint_profile))
                .unwrap_or_default()
        } else {
            own
        };
        let api_key_kind = attr("auth_kind") == "apikey";
        let bearer = oauth_token || (!api_key_kind && attr("api_key").trim().is_empty());
        let base = base_model(&req.model);
        // Go `cliproxyauth.ResolvedModelInfo(req)`, bound by dispatch per attempt.
        let resolved = req
            .resolved_model
            .as_ref()
            .map(|r| cpa_common::thinking::ModelCaps::from(&r.info));
        Self {
            credential,
            first_party: tokens::first_party(&base_url),
            proxy: Proxy::effective(credential, cfg),
            upstream_model: delegation.upstream_model.map_or_else(|| base.clone(), |f| f(&base)),
            kimi: kimi_upstream(&credential.provider, &base_url),
            log_provider: delegation.request_log_provider.unwrap_or("claude"),
            execution: session::normalize(req.execution_session.as_deref().unwrap_or_default()),
            is_compat: req.resolved_model.as_ref().is_some_and(|r| r.is_compat()),
            codex: cpa_common::codex_client::Settings::from_config(cfg),
            payload_rules: cpa_common::payload::Rules::of(cfg),
            resolved,
            base_model: base,
            cli_profile: oauth_token || profile == "claude-code-cli",
            oauth_token,
            bearer,
            settings,
            api_key,
            base_url,
            today: String::new(),
        }
        .with_today(credential)
    }

    /// `claudeCodeTimezone`: credential `timezone`, then header defaults, else local.
    fn with_today(mut self, credential: &Credential) -> Self {
        let own = credential
            .attributes
            .get("timezone")
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        let zone = own
            .or_else(|| {
                credential
                    .str("timezone")
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| self.settings.header_defaults.timezone.trim().to_owned());
        self.today = if matches!(zone.as_str(), "UTC" | "Etc/UTC") {
            chrono::Utc::now().format("%Y-%m-%d").to_string()
        } else {
            chrono::Local::now().format("%Y-%m-%d").to_string()
        };
        self
    }

    /// `helps.ApplyRequestThinking` with provider `claude`.
    fn apply_thinking(&self, req: &ExecRequest, body: String) -> Result<String, ExecError> {
        use cpa_common::thinking::{RequestThinking, apply_request_thinking};
        apply_request_thinking(&RequestThinking {
            body: body.as_bytes(),
            payload: &req.body,
            original: &req.original_body,
            model: &req.model,
            from: req.source_format.as_str(),
            to: Format::Claude.as_str(),
            provider: "claude",
            resolved: self.resolved.as_ref().map(Some),
            has_request_transformer: cpa_translate::pair(req.source_format, Format::Claude).is_some(),
            updates_changed: false,
        })
        .map(text)
        .map_err(|e| {
            if e.code.is_some() {
                ExecError::local(e.status(), FailureScope::Request, e.message)
            } else {
                plain_error(e.message)
            }
        })
    }

    /// `resolveClaudeWirePolicy`: (cloak, strict, sensitive words, cache user id).
    fn wire_policy(&self, confirmed: bool) -> (bool, bool, Vec<String>, bool) {
        let attr = |k: &str| {
            let a = self.credential.attributes.get(k).map(|s| s.trim()).unwrap_or_default();
            if a.is_empty() {
                self.credential.str(k).unwrap_or_default().trim().to_owned()
            } else {
                a.to_owned()
            }
        };
        let attr_mode = attr("cloak_mode");
        let attr_strict = attr("cloak_strict_mode").eq_ignore_ascii_case("true");
        let attr_words: Vec<String> = Some(attr("cloak_sensitive_words"))
            .filter(|s| !s.is_empty())
            .map(|s| s.split(',').map(|w| w.trim().to_owned()).collect())
            .unwrap_or_default();
        let attr_cache = attr("cloak_cache_user_id").eq_ignore_ascii_case("true");
        let mut mode = if self.settings.disable_cloak_mode {
            "never".to_owned()
        } else {
            "auto".to_owned()
        };
        let (mut strict, mut words, mut cache) = (attr_strict, attr_words.clone(), attr_cache);
        if !attr_mode.is_empty() {
            mode = attr_mode.clone();
        }
        let base = self
            .credential
            .attributes
            .get("base_url")
            .map(String::as_str)
            .unwrap_or_default();
        let key_cloak = self.settings.key_for(&self.api_key, base).and_then(|k| k.cloak.clone());
        if let Some(c) = &key_cloak {
            if !c.mode.trim().is_empty() {
                mode = c.mode.trim().to_owned();
            }
            strict |= c.strict_mode;
            if !c.sensitive_words.is_empty() {
                words = c.sensitive_words.clone();
            }
            if let Some(v) = c.cache_user_id {
                cache = v;
            }
        }
        let configured =
            key_cloak.is_some() || !attr_mode.is_empty() || attr_strict || !attr_words.is_empty() || attr_cache;
        let mut cloak = (self.cli_profile || configured) && !confirmed;
        if !confirmed {
            match mode.trim().to_lowercase().as_str() {
                "always" => cloak = true,
                "never" => cloak = false,
                _ => {}
            }
        }
        (cloak, strict, words, cache)
    }

    /// Go `derived_session_id` metadata, as the server's `session.Enrich` set it.
    fn derived_session(&self, req: &ExecRequest) -> String {
        req.derived_session.clone().unwrap_or_default()
    }

    async fn prepare_messages(
        &self,
        req: &ExecRequest,
        translated: &[u8],
        original_translated: &[u8],
        upstream_stream: bool,
    ) -> Result<Prepared, ExecError> {
        let original = utf8(&req.original_body);
        let detection = detect::detect(&req.headers, &original, false, &self.settings);
        let confirmed = detection.confirmed;
        let derived = self.derived_session(req);
        let translated = utf8(translated).into_owned();
        let session_id = if self.cli_profile {
            session::agent_session_uuid(
                &session::Inputs {
                    headers: &req.headers,
                    original: &original,
                    translated: &utf8(&req.body),
                    derived: &derived,
                    execution: &self.execution,
                },
                confirmed,
            )
        } else {
            String::new()
        };
        let mut body = translated;
        body = set_string_if_different(&body, "model", &self.upstream_model);
        body = self.apply_thinking(req, body)?;
        if self.rebuild_mid_system() {
            body = rebuild_mid_system(&body);
        }
        let (cloak, strict, words, cache_user_id) = self.wire_policy(confirmed);
        let cch = signing::enabled(&self.api_key, self.cli_profile, self.first_party);
        let probe_before = signals::probe_or_helper(&body);
        let mut continuity = session::Continuity::default();
        let mut cloaked = false;
        let before_cloak = body.clone();
        if cloak {
            if !strict && let Some(message) = cloak::invalid_system_block(&body) {
                return Err(ExecError::local(400, FailureScope::Request, message));
            }
            let mut subagent = false;
            let (mut prev, mut prompt) = (String::new(), String::new());
            if !probe_before {
                subagent = signals::subagent(&req.headers, &body);
                let (existing_prev, existing_prompt) = signals::billing_tags(&body);
                (prev, prompt, continuity) =
                    self.continuity_tags(&req.headers, &body, &session_id, &existing_prev, &existing_prompt);
            }
            let version = profile::default_version(&self.settings);
            let workload = detect::header(&req.headers, "x-cpa-claude-workload").to_owned();
            body = cloak::install_system(
                &body,
                &cloak::SystemPlan {
                    strict,
                    billing: cloak::Billing {
                        signed: cch,
                        version: &version,
                        message: "",
                        entrypoint: "cli",
                        workload: &workload,
                        subagent,
                        prev_req: &prev,
                        prompt_id: &prompt,
                        human_turn: !probe_before && !subagent,
                    },
                    date: &self.today,
                },
            );
            let model = rawjson::string(&body, "model").trim().to_lowercase();
            if cloak::is_opus55(&model) && !probe_before && !rawjson::get(&body, "fallbacks").exists() {
                body = rawjson::set_raw(&body, "fallbacks", r#"[{"model":"claude-opus-4-8"}]"#);
            }
            if cloak::is_fable51(&model) && !probe_before && !rawjson::get(&body, "fallbacks").exists() {
                body = rawjson::set_raw(&body, "fallbacks", r#"[{"model":"claude-opus-5"}]"#);
            }
            if !probe_before {
                body = cloak_thinking_display(&body);
            }
            if probe_before || (subagent && !signals::subagent_requests_1h(&req.headers, &body)) {
                body = cloak::strip_ttl(&body);
            }
            if !self.cli_profile && needs_fake_user_id(&body) {
                // Go `injectFakeUserID`: Home KV in Home mode, and its errors fail the
                // request.
                let user_id = if cache_user_id {
                    session::cached_user_id_required(&self.api_key).await
                } else {
                    session::cached_session_id_required(&self.api_key)
                        .await
                        .map(|session| cloak::fake_user_id(&session))
                };
                let user_id = user_id.map_err(|e| ExecError::local(500, FailureScope::Transport, e))?;
                body = rawjson::set_str(&body, "metadata.user_id", &user_id);
            }
            cloaked = true;
        }
        let probe = probe_before || signals::probe_or_helper(&body);
        let eligible = cloaked && self.first_party;
        let caller_owned_cm = rawjson::get(&body, "context_management").exists();
        let mut injected_cm = false;
        let mut diagnostics = session::Continuity::default();
        let mut injected_diagnostics = false;
        if continuity.initialized {
            diagnostics = continuity.clone();
        }
        if eligible {
            if let Some(updated) = cloak::inject_context_management(&body) {
                body = updated;
                injected_cm = true;
            }
            if self.cli_profile && !probe {
                if !continuity.initialized {
                    let begun = session::begin(&self.continuity_identity(), &session_id, false, "");
                    diagnostics = begun;
                    diagnostics.initialized = !diagnostics.key.is_empty();
                }
                if diagnostics.initialized || !diagnostics.key.is_empty() {
                    body = inject_diagnostics(&body, &diagnostics.previous_message_id);
                    injected_diagnostics = true;
                }
            }
        }
        let placement = reconcile::SystemPlacement::capture(&before_cloak, &body, cloaked);
        let fable = reconcile::FableState::capture(&before_cloak, &body, cloaked);
        let (edited, touched) = cpa_common::payload::apply_tracked(
            &self.payload_rules,
            &cpa_common::payload::Request {
                target_executor: "claude",
                model: &self.base_model,
                requested_model: &req.requested_model,
                protocol: Format::Claude.as_str(),
                from_protocol: req.source_format.as_str(),
                root: "",
                original: original_translated,
                request_path: &req.request_path,
                headers: Some(&req.headers),
            },
            body.into_bytes(),
            &["context_management", "fallbacks", "thinking.display", "diagnostics"],
        );
        body = text(edited);
        let touched = |path: &str| touched.contains(path);
        body = placement.reconcile(&body);
        let was_probe = probe;
        let probe = signals::probe_or_helper(&body);
        if probe {
            diagnostics = session::Continuity::default();
            if injected_diagnostics && !touched("diagnostics") {
                body = rawjson::delete(&body, "diagnostics");
            }
            if cloaked {
                body = signals::strip_billing_tags(&body);
            }
            continuity = session::Continuity::default();
        } else if was_probe && cloaked {
            // A payload rule declassified the probe: start continuity and diagnostics
            // now, as cloaking would have.
            let (existing_prev, existing_prompt) = signals::billing_tags(&body);
            let (prev, prompt, begun) =
                self.continuity_tags(&req.headers, &body, &session_id, &existing_prev, &existing_prompt);
            if begun.initialized {
                continuity = begun.clone();
                body = signals::inject_billing_tags(&body, &prev, &prompt);
                if self.cli_profile && self.first_party {
                    body = inject_diagnostics(&body, &begun.previous_message_id);
                    diagnostics = begun;
                }
            }
        }
        body = reconcile::fable(
            &body,
            &fable,
            touched("fallbacks"),
            touched("thinking.display"),
            cloaked,
            probe,
        );
        body = self.ensure_max_tokens(&body);
        body = disable_thinking_if_tool_choice_forced(&body);
        body = cloak::reconcile_context_management(
            &body,
            eligible,
            caller_owned_cm,
            injected_cm,
            touched("context_management"),
        );
        body = normalize_sampling(&body, confirmed);
        let cpa_owns_cache = !confirmed && (cloaked || cloak::count_cache_controls(&body) == 0);
        if cpa_owns_cache {
            body = cloak::ensure_cache_control(&body);
        }
        body = cloak::enforce_cache_limit(&body, 4);
        let subagent = signals::subagent(&req.headers, &body);
        let subagent_1h = subagent && signals::subagent_requests_1h(&req.headers, &body);
        if cpa_owns_cache && self.cli_profile && (!subagent || subagent_1h) && !probe {
            body = cloak::upgrade_ttl(&body, "1h");
        } else if probe || (subagent && !subagent_1h) {
            body = cloak::strip_ttl(&body);
        }
        body = cloak::normalize_ttl(&body);
        // Execute only: ExecuteStream forwards the body's own `stream` field.
        let stream_field = rawjson::get(&body, "stream");
        if !req.stream && (!detection.helper_profile || stream_field.exists() || upstream_stream) {
            let want = if upstream_stream { "true" } else { "false" };
            if stream_field.json() != want {
                body = rawjson::set_raw(&body, "stream", want);
            }
        }
        let (extra_betas, stripped) = extract_betas(&body);
        body = stripped;
        let mut reverse = alias::Reverse::new();
        if self.cli_profile && cloaked {
            let secret = Some(req.caller.principal.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or(alias::DEFAULT_SECRET);
            (body, reverse) = alias::remap(&body, secret);
        }
        body = sanitize_for_upstream(&body, &self.base_model, self.is_compat);
        if self.cli_profile {
            body = self.apply_identity(&body, &session_id)?;
        }
        if cloaked
            && !words.is_empty()
            && let Some(m) = cloak::SensitiveWords::new(&words)
        {
            body = m.obfuscate(&body);
        }
        if cch {
            let fallback = if !detection.helper_profile || rawjson::get(&body, "system").exists() {
                self.fallback_billing(&req.headers, &body, &detection.entrypoint, &continuity, confirmed)
            } else {
                String::new()
            };
            body = signing::finalize(&body, &fallback)
                .map_err(|e| ExecError::local(500, FailureScope::Request, format!("finalize Claude CCH: {e}")))?;
        }
        if self.kimi && !self.cli_profile {
            // stripDefaultKimiClaudeCodeAttribution: Kimi reads the block as prompt text.
            body = strip_attribution_system(&body);
        }
        validate_mid_system(&body, confirmed, self.first_party)?;
        let fast = betas::uses_fast_mode(&body, &betas::requested("", &[]));
        let (headers, order) = self
            .headers(
                req,
                &body,
                &extra_betas,
                upstream_stream,
                false,
                confirmed && !cloaked,
                detection.helper_profile,
                &session_id,
                cloaked,
            )
            .await?;
        let fast = fast
            || headers.iter().any(|(k, v)| {
                k.eq_ignore_ascii_case("anthropic-beta") && v.split(',').any(|b| b.trim() == betas::FAST_MODE)
            });
        Ok(Prepared {
            body,
            headers,
            order,
            reverse,
            continuity: diagnostics,
            fast,
        })
    }

    async fn prepare_count(&self, req: &ExecRequest, translated: &[u8]) -> Result<Prepared, ExecError> {
        let original = utf8(&req.original_body);
        let detection = detect::detect(&req.headers, &original, true, &self.settings);
        let confirmed = detection.confirmed;
        let session_id = if self.cli_profile {
            let derived = self.derived_session(req);
            session::agent_session_uuid(
                &session::Inputs {
                    headers: &req.headers,
                    original: &original,
                    translated: &utf8(&req.body),
                    derived: &derived,
                    execution: &self.execution,
                },
                confirmed,
            )
        } else {
            String::new()
        };
        let mut body = utf8(translated).into_owned();
        body = set_string_if_different(&body, "model", &self.upstream_model);
        body = self.apply_thinking(req, body)?;
        if self.rebuild_mid_system() {
            body = rebuild_mid_system(&body);
        }
        let (cloak, strict, words, _) = self.wire_policy(confirmed);
        if cloak {
            if !strict && let Some(message) = cloak::invalid_system_block(&body) {
                return Err(ExecError::local(400, FailureScope::Request, message));
            }
            body = cloak::relocate_system_for_count(&body, strict);
            if !words.is_empty()
                && let Some(m) = cloak::SensitiveWords::new(&words)
            {
                body = m.obfuscate(&body);
            }
        }
        body = cloak::enforce_cache_limit(&body, 4);
        body = cloak::normalize_ttl(&body);
        let (mut extra_betas, stripped) = extract_betas(&body);
        body = stripped;
        extra_betas.push(betas::TOKEN_COUNTING.into());
        if self.cli_profile && cloak {
            let secret = Some(req.caller.principal.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or(alias::DEFAULT_SECRET);
            body = alias::remap(&body, secret).0;
        }
        body = sanitize_for_upstream(&body, &self.base_model, self.is_compat);
        if self.first_party || self.cli_profile {
            for field in ["metadata", "context_management", "diagnostics"] {
                body = rawjson::delete(&body, field);
            }
        }
        if self.cli_profile {
            body = strip_attribution_system(&body);
        }
        validate_mid_system(&body, confirmed, self.first_party)?;
        let (headers, order) = self
            .headers(
                req,
                &body,
                &extra_betas,
                false,
                true,
                confirmed && !cloak,
                false,
                &session_id,
                cloak,
            )
            .await?;
        Ok(Prepared {
            body,
            headers,
            order,
            reverse: Default::default(),
            continuity: Default::default(),
            fast: false,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn headers(
        &self,
        req: &ExecRequest,
        body: &str,
        extra_betas: &[String],
        stream: bool,
        count_tokens: bool,
        confirmed: bool,
        helper: bool,
        session_id: &str,
        cloak: bool,
    ) -> Result<(Vec<(String, String)>, Vec<String>), ExecError> {
        let cpa_session = cpa_common::session::cpa_session_id(req.session.as_deref()).unwrap_or_default();
        // Only a fingerprinted request replaces the caller's identity; an API key without
        // cloaking forwards the caller's own User-Agent, refused or not.
        if self.cli_profile || cloak {
            profile::note_refused(self.credential, &req.headers, &self.settings);
        }
        // Go: stabilizeDeviceProfile && confirmedClaudeCode, and the error (Home KV
        // unreachable in Home mode) fails the request before it is sent.
        let device_profile = if self.settings.header_defaults.stabilize_device_profile && confirmed {
            let profile = profile::resolve_required(&self.credential.id, &self.api_key, &req.headers, &self.settings)
                .await
                .map_err(|e| ExecError::local(500, FailureScope::Transport, e))?;
            Some(profile)
        } else {
            None
        };
        // Go `applyClaudeHeaders`, after the device profile and the passthrough
        // return: without a session, the key's cached session ID.
        let cached_session_id = if headers::needs_cached_session(self.cli_profile || cloak, confirmed, session_id) {
            session::cached_session_id_required(&self.api_key)
                .await
                .map_err(|e| ExecError::local(500, FailureScope::Transport, e))?
        } else {
            String::new()
        };
        let h = headers::build(&headers::Plan {
            api_key: &self.api_key,
            bearer: self.bearer,
            first_party: self.first_party,
            count_tokens,
            stream,
            extra_betas,
            body,
            incoming: &req.headers,
            confirmed,
            helper,
            cli_fingerprint: self.cli_profile || cloak,
            use_oauth_betas: self.cli_profile,
            session_id,
            settings: &self.settings,
            attributes: &self.credential.attributes,
            cpa_session: &cpa_session,
            device_profile,
            cached_session_id: &cached_session_id,
        });
        Ok(headers::wire(h, self.first_party, count_tokens))
    }

    fn rebuild_mid_system(&self) -> bool {
        self.credential
            .attributes
            .get("rebuild_mid_system_message")
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
            || self
                .settings
                .key_for(
                    &self.api_key,
                    self.credential
                        .attributes
                        .get("base_url")
                        .map(String::as_str)
                        .unwrap_or_default(),
                )
                .is_some_and(|k| k.rebuild_mid_system_message)
    }

    /// `claudeDiagnosticsCredentialIdentity`.
    fn continuity_identity(&self) -> String {
        if !self.credential.id.trim().is_empty() {
            return format!("id:{}", self.credential.id.trim());
        }
        let account = identity::account_uuid(self.credential);
        if !account.is_empty() {
            return format!("account:{account}");
        }
        String::new()
    }

    /// `resolveClaudeContinuityTags`.
    fn continuity_tags(
        &self,
        headers: &http::HeaderMap,
        body: &str,
        session_id: &str,
        existing_prev: &str,
        existing_prompt: &str,
    ) -> (String, String, session::Continuity) {
        let session = if session_id.is_empty() {
            session::agent_session_uuid(
                &session::Inputs {
                    headers,
                    original: body,
                    translated: body,
                    derived: "",
                    execution: &self.execution,
                },
                false,
            )
        } else {
            session_id.to_owned()
        };
        // ClaudeRequestHasExecutionMetadata.
        let has_execution_metadata = !self.execution.is_empty();
        let new_turn = signals::new_prompt_turn(body);
        let begun = session::begin(&self.continuity_identity(), &session, new_turn, existing_prompt);
        if begun.key.is_empty() {
            return Default::default();
        }
        let prompt = if !existing_prompt.is_empty() {
            existing_prompt.to_owned()
        } else if !begun.previous_message_id.is_empty()
            && !begun.prompt_id.is_empty()
            && (has_execution_metadata || !new_turn)
        {
            begun.prompt_id.clone()
        } else if !has_execution_metadata {
            session::deterministic_prompt_id(&format!("cpa:prompt:{}", cloak::fingerprint_message(body)))
        } else {
            begun.prompt_id.clone()
        };
        let use_stored = has_execution_metadata || !existing_prev.is_empty();
        let prev = if use_stored && !begun.previous_request_id.is_empty() {
            begun.previous_request_id.clone()
        } else {
            existing_prev.to_owned()
        };
        let continuity = session::Continuity {
            key: begun.key,
            sequence: begun.sequence,
            previous_message_id: if use_stored {
                begun.previous_message_id
            } else {
                String::new()
            },
            previous_request_id: if use_stored {
                begun.previous_request_id
            } else {
                String::new()
            },
            prompt_id: prompt.clone(),
            initialized: true,
        };
        (prev, prompt, continuity)
    }

    /// `claudeCCHFallbackBillingHeader`.
    fn fallback_billing(
        &self,
        headers: &http::HeaderMap,
        body: &str,
        entrypoint: &str,
        continuity: &session::Continuity,
        confirmed: bool,
    ) -> String {
        let probe = signals::probe_or_helper(body);
        let (mut prev, mut prompt) = signals::billing_tags(body);
        if !probe {
            if prev.is_empty() {
                prev = continuity.previous_request_id.clone();
            }
            if prompt.is_empty() {
                prompt = continuity.prompt_id.clone();
            }
        }
        let message = cloak::fingerprint_message(body);
        cloak::billing_header(&cloak::Billing {
            signed: true,
            // A forwarded newer release signs with its own version, as its User-Agent
            // says (docs/DIFFERENCES-FROM-GO.md).
            version: &profile::billing_version(detect::header(headers, "user-agent"), confirmed, &self.settings),
            message: &message,
            entrypoint,
            workload: detect::header(headers, "x-cpa-claude-workload"),
            subagent: signals::subagent(headers, body),
            prev_req: &prev,
            prompt_id: &prompt,
            human_turn: false,
        })
    }

    fn apply_identity(&self, body: &str, session_id: &str) -> Result<String, ExecError> {
        let synthesize = self.cli_profile && !self.oauth_token;
        // Delegated providers seed from the stable auth identity, so a token rotation
        // does not rotate the device fingerprint.
        let seed = if self.kimi {
            identity_seed(self.credential)
        } else {
            self.api_key.clone()
        };
        let (device, account) = identity::wire_identity(self.credential, &seed, synthesize);
        // applyClaudeCLIIdentity wraps every ApplyClaudeCredentialMetadata error once more.
        identity::apply(body, &device, &account, session_id).map_err(|e| match e {
            identity::ApplyError::Plain(e) => plain_error(format!("apply Claude credential metadata: {e}")),
            identity::ApplyError::Request(e) => ExecError::local(
                400,
                FailureScope::Request,
                format!("apply Claude credential metadata: {e}"),
            ),
        })
    }

    /// `ensureModelMaxTokens`: only a model some Claude credential registered gets a
    /// default `max_tokens`, its registered completion limit or 1024.
    fn ensure_max_tokens(&self, body: &str) -> String {
        let model = self.base_model.trim();
        if !gjson::valid(body)
            || rawjson::get(body, "max_tokens").exists()
            || !cpa_core::registry::model_providers(model)
                .iter()
                .any(|p| p.eq_ignore_ascii_case("claude"))
        {
            return body.to_owned();
        }
        let max = cpa_core::registry::registered_model(model, Some("claude"))
            .and_then(|info| {
                info.raw
                    .get("max_completion_tokens")
                    .and_then(serde_json::Value::as_i64)
            })
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_TOKENS);
        rawjson::set_raw(body, "max_tokens", &max.to_string())
    }
}

/// `defaultModelMaxTokens`.
const DEFAULT_MAX_TOKENS: i64 = 1024;

/// Fill `thinking.display: updates` for progress-display models with active thinking.
fn cloak_thinking_display(body: &str) -> String {
    if rawjson::get(body, "thinking.display").exists() || !betas::progress_display(&rawjson::string(body, "model")) {
        return body.to_owned();
    }
    match rawjson::string(body, "thinking.type").trim().to_lowercase().as_str() {
        "adaptive" | "enabled" => rawjson::set_str(body, "thinking.display", "updates"),
        _ => body.to_owned(),
    }
}

/// `injectFakeUserID`: a caller's valid Claude Code user ID is kept.
/// `injectFakeUserID`: a missing or invalid `metadata.user_id` is replaced.
fn needs_fake_user_id(body: &str) -> bool {
    let existing = rawjson::string(body, "metadata.user_id");
    !rawjson::get(body, "metadata").exists() || existing.is_empty() || !detect::valid_user_id(&existing)
}

/// `injectClaudeDiagnosticsWithState`: after context_management when present.
fn inject_diagnostics(body: &str, previous_message_id: &str) -> String {
    let value = if previous_message_id.is_empty() {
        r#"{"previous_message_id":null}"#.to_owned()
    } else {
        format!(
            r#"{{"previous_message_id":{}}}"#,
            rawjson::js_string(previous_message_id)
        )
    };
    if rawjson::get(body, "diagnostics").exists() {
        return rawjson::set_raw(body, "diagnostics", &value);
    }
    let cm = rawjson::get(body, "context_management");
    if let Some(start) = rawjson::offset(body, &cm) {
        let end = start + cm.json().len();
        return format!("{},\"diagnostics\":{value}{}", &body[..end], &body[end..]);
    }
    rawjson::set_raw(body, "diagnostics", &value)
}

/// `disableThinkingIfToolChoiceForced`.
fn disable_thinking_if_tool_choice_forced(body: &str) -> String {
    if !matches!(rawjson::get(body, "tool_choice.type").str(), "any" | "tool") {
        return body.to_owned();
    }
    let mut body = rawjson::delete(body, "thinking");
    body = rawjson::delete(&body, "output_config.effort");
    let oc = rawjson::get(&body, "output_config");
    let mut members = 0;
    oc.each(|_, _| {
        members += 1;
        true
    });
    if oc.kind() == gjson::Kind::Object && members == 0 {
        body = rawjson::delete(&body, "output_config");
    }
    body
}

/// `normalizeClaudeSamplingForUpstream`.
fn normalize_sampling(body: &str, native: bool) -> String {
    let thinking = matches!(
        rawjson::string(body, "thinking.type").trim().to_lowercase().as_str(),
        "enabled" | "adaptive" | "auto"
    );
    let mut body = body.to_owned();
    if !native {
        body = rawjson::delete(&body, "temperature");
        body = rawjson::delete(&body, "top_p");
        if thinking {
            body = rawjson::delete(&body, "top_k");
        }
        return body;
    }
    if thinking {
        let t = rawjson::get(&body, "temperature");
        if t.exists() && t.f64() != 1.0 {
            body = rawjson::delete(&body, "temperature");
        }
        let p = rawjson::get(&body, "top_p");
        if p.exists() && p.f64() < 0.95 {
            body = rawjson::delete(&body, "top_p");
        }
        return rawjson::delete(&body, "top_k");
    }
    if rawjson::get(&body, "temperature").exists() && rawjson::get(&body, "top_p").exists() {
        body = rawjson::delete(&body, "top_p");
    }
    body
}

fn has_mid_system(body: &str) -> bool {
    rawjson::get(body, "messages")
        .array()
        .iter()
        .any(|m| m.get("role").str() == "system")
}

/// `validateClaudeMidSystemMessageModel`.
fn validate_mid_system(body: &str, confirmed: bool, first_party: bool) -> Result<(), ExecError> {
    if confirmed || !first_party || !cloak::legacy_system_reminder(body) || !has_mid_system(body) {
        return Ok(());
    }
    let model = rawjson::string(body, "model");
    let model = if model.is_empty() { "unknown".to_owned() } else { model };
    Err(ExecError::local(
        400,
        FailureScope::Request,
        format!(
            "invalid_request_error: role 'system' is not supported on this model. Model {model:?} predates mid-conversation system turns, so system instructions must stay in the top-level system field for it."
        ),
    ))
}

/// `rebuildMidSystemMessagesToTopLevel`: fold role=system turns into `system`.
fn rebuild_mid_system(body: &str) -> String {
    let messages = rawjson::get(body, "messages");
    if messages.kind() != gjson::Kind::Array {
        return body.to_owned();
    }
    let mut moved = Vec::new();
    let mut kept = Vec::new();
    for m in messages.array() {
        if m.get("role").str().trim().eq_ignore_ascii_case("system") {
            moved.extend(system_text_parts(&m.get("content")));
        } else {
            kept.push(m.json().to_owned());
        }
    }
    if moved.is_empty() {
        return body.to_owned();
    }
    let mut parts = system_text_parts(&rawjson::get(body, "system"));
    parts.extend(moved);
    let body = rawjson::set_raw(body, "system", &format!("[{}]", parts.join(",")));
    rawjson::set_raw(&body, "messages", &format!("[{}]", kept.join(",")))
}

/// `claudeSystemTextParts`: strings become text blocks, text objects stay raw.
fn system_text_parts(content: &gjson::Value<'_>) -> Vec<String> {
    let block = |text: &str| rawjson::set_str(r#"{"type":"text","text":""}"#, "text", text);
    match content.kind() {
        gjson::Kind::String if !content.str().trim().is_empty() => vec![block(content.str())],
        gjson::Kind::Array => content
            .array()
            .iter()
            .filter_map(|item| match item.kind() {
                gjson::Kind::String if !item.str().trim().is_empty() => Some(block(item.str())),
                gjson::Kind::Object
                    if item.get("type").str() == "text" && !item.get("text").str().trim().is_empty() =>
                {
                    Some(item.json().to_owned())
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// `util.StripClaudeCodeAttributionSystem`.
fn strip_attribution_system(body: &str) -> String {
    let system = rawjson::get(body, "system");
    let attribution = |t: &str| t.trim_start().starts_with(cloak::BILLING_PREFIX);
    match system.kind() {
        gjson::Kind::String if attribution(system.str()) => rawjson::delete(body, "system"),
        gjson::Kind::Array => {
            let blocks = system.array();
            let kept: Vec<String> = blocks
                .iter()
                .filter(|b| !(b.get("type").str() == "text" && attribution(b.get("text").str())))
                .map(|b| b.json().to_owned())
                .collect();
            if kept.len() == blocks.len() {
                body.to_owned()
            } else if kept.is_empty() {
                rawjson::delete(body, "system")
            } else {
                rawjson::set_raw(body, "system", &format!("[{}]", kept.join(",")))
            }
        }
        _ => body.to_owned(),
    }
}

#[cfg(test)]
mod go_exec;
#[cfg(test)]
mod go_unit;
#[cfg(test)]
mod tests;
