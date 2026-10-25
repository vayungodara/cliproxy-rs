//! Codex executor: OpenAI Responses on the ChatGPT Codex backend, with an OAuth token
//! (bearer + `Chatgpt-Account-Id`) or a configured API key (`codex-api-key`).
//!
//! HTTP paths port codex_executor_execute.go and codex_executor_stream.go. The WebSocket
//! upstream lives in `codex_ws`.

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use chrono::Utc;
use cpa_core::config::Config;
use cpa_core::credential::{Credential, MetadataPatch};
use cpa_core::exec::{
    ExecError, ExecRequest, ExecResponse, ExecSession, ExecStream, FailureScope, Operation, ResponseBody,
};
use cpa_core::format::Format;
use futures_util::StreamExt;
use http::HeaderMap;

use crate::codex_capture::Wire;
use crate::codex_oauth::{self, CodexOAuth};
use crate::codex_quota::QuotaSignals;
use crate::codex_request::{self as request, Call, Settings, View};
use crate::codex_response::{self as response, Bootstrap, Processor};

pub use crate::codex_request::DEFAULT_BASE_URL;
pub use crate::codex_ws::{ClientFrames, InterruptError, NO_ACTIVE_UPSTREAM, SteeringInput};

pub struct CodexExecutor {
    /// Chrome profile for chatgpt.com, Go's standard transport elsewhere, per proxy.
    pub(crate) transport: crate::codex_tls::Transport,
    oauth: CodexOAuth,
    quota: Arc<QuotaSignals>,
    /// Upstream Responses WebSocket sockets per downstream session.
    pub(crate) ws: Arc<crate::codex_ws::Pool>,
    /// Claude clients' reasoning replay (process-wide in Go).
    pub(crate) replay: Arc<crate::codex_replay::Cache>,
    /// Base for OAuth Alpha Search, which Go never derives from credential attributes.
    alpha_base_url: String,
    /// Live call, sideband and hangup URLs (`codex_live`).
    pub(crate) live: crate::codex_live::Endpoints,
}

/// The production executor. Construction only fails if the TLS backend cannot initialise.
impl Default for CodexExecutor {
    fn default() -> Self {
        Self::new().expect("codex HTTP client")
    }
}

impl CodexExecutor {
    /// Production transports: uTLS Chrome for chatgpt.com, Go's standard transport
    /// elsewhere, both per effective proxy (crate::proxy). Token refreshes use the
    /// standard transport for the credential's effective proxy.
    pub fn new() -> wreq::Result<Self> {
        Ok(Self::with_transport(
            crate::codex_tls::Transport::new(crate::proxy::Hooks::default()),
            CodexOAuth::new(crate::proxy::default_client()),
        ))
    }

    /// Caller-built transports. Tests pass a plain client and an OAuth service pointed at
    /// a local mock so nothing reaches OpenAI. Unproxied non-chatgpt.com requests use
    /// `client`.
    pub fn with_client(client: wreq::Client, oauth: CodexOAuth) -> Self {
        Self::with_transport(crate::codex_tls::Transport::with_default(client), oauth)
    }

    fn with_transport(transport: crate::codex_tls::Transport, oauth: CodexOAuth) -> Self {
        Self {
            transport,
            oauth,
            quota: Arc::default(),
            ws: Default::default(),
            replay: Arc::default(),
            alpha_base_url: DEFAULT_BASE_URL.into(),
            live: Default::default(),
        }
    }

    /// Test hook: where OAuth Alpha Search requests go.
    pub fn with_alpha_base_url(mut self, base: impl Into<String>) -> Self {
        self.alpha_base_url = base.into().trim_end_matches('/').to_owned();
        self
    }

    /// Passive quota snapshots per credential (M4-0016), for the management API.
    pub fn quota(&self) -> &QuotaSignals {
        &self.quota
    }

    pub(crate) fn quota_handle(&self) -> Arc<QuotaSignals> {
        self.quota.clone()
    }

    /// One turn of a downstream Responses WebSocket session (`CodexAutoExecutor`):
    /// credentials with `websockets` enabled keep a pooled upstream socket; others run
    /// the HTTP stream with the session as prompt-cache identity and cannot continue
    /// upstream state.
    pub async fn execute_in_session(
        &self,
        credential: &Credential,
        req: ExecRequest,
        cfg: &Config,
        session: &ExecSession,
    ) -> Result<ExecResponse, ExecError> {
        check_response_format(&req)?;
        crate::codex_siwc::require_token(credential)?;
        let view = View::for_request(credential, cfg).with_session(explicit_session(&req));
        let settings = Settings::scoped(cfg, &view);
        if view.websockets() {
            return self.stream_ws(&view, &settings, req, session, cfg).await;
        }
        if session.continuation {
            return Err(ExecError::replay_required());
        }
        self.stream_with_session(&view, &settings, req, Some(&session.id)).await
    }

    /// Whether `credential` keeps upstream state on a WebSocket (Go:
    /// `websocketUpstreamSupportsIncrementalInput`).
    pub fn upstream_websocket(credential: &Credential) -> bool {
        View::new(credential).websockets()
    }

    /// `upstream.codex.response-steering` as the Responses WebSocket handler reads it
    /// (Go `SDKConfig.CodexResponseSteering`), shared by every credential kind.
    pub fn response_steering_configured(cfg: &Config) -> bool {
        request::response_steering(cfg)
    }

    /// Binds a downstream connection's later client frames to its session (Go
    /// `WithWebsocketInput` and `WithWebsocketAuthCheck`). A steering turn on that session
    /// reads them after its first `response.created`; [`Self::close_session`] releases them.
    pub fn attach_steering(&self, session: &str, input: SteeringInput) {
        self.ws.attach_steering(session, input);
    }

    /// Go `InterruptExecutionSession`: forwards a client's `response.interrupt` to the
    /// session's upstream socket while a turn reads on it (see `codex_ws::Pool::interrupt`).
    pub async fn interrupt_session(
        &self,
        session: &str,
        payload: &[u8],
        enabled: impl FnOnce(&str) -> bool,
    ) -> Result<(), InterruptError> {
        self.ws.interrupt(session, payload, enabled).await
    }

    /// The error that lost the session's upstream socket, once it is lost (what Go's
    /// disconnect notifier hands the downstream handler).
    pub fn session_loss(&self, id: &str) -> Option<ExecError> {
        self.ws.loss(id)
    }

    /// Resolves when the session's upstream socket is lost.
    pub fn session_closed(&self, id: &str) -> impl std::future::Future<Output = ExecError> + Send + 'static {
        self.ws.closed(id)
    }

    /// Releases the session's upstream socket (downstream connection ended).
    pub fn close_session(&self, id: &str) {
        self.ws.close(id);
    }

    /// Refresh due: the outer tokens under Go's 96h Codex lead, or the Sign in with
    /// ChatGPT partition under its 5-minute lead (Go `shouldRefresh`). Cheap and side-effect free.
    pub fn needs_prepare(&self, credential: &Credential, cfg: &Config) -> bool {
        self.needs_prepare_at(credential, cfg, Utc::now())
    }

    pub fn needs_prepare_at(&self, credential: &Credential, _cfg: &Config, now: chrono::DateTime<Utc>) -> bool {
        codex_oauth::refresh_due(credential, now) || crate::codex_siwc::refresh_due(credential, now) == Some(true)
    }

    /// The refresh grants for `credential`, on the client its proxy selects
    /// (`NewCodexAuthWithProxyURL(cfg, auth.ProxyURL)`: the credential's proxy, then the
    /// global one, on Go's standard transport).
    pub fn refresher(&self, credential: &Credential, cfg: &Config) -> crate::codex_refresh::Refresher {
        let proxy = crate::proxy::Proxy::effective(credential, cfg);
        crate::codex_refresh::Refresher::new(self.oauth.with_client(self.transport.standard(&proxy)))
    }

    /// [`crate::codex_refresh::Refresher::prepare`].
    pub async fn prepare(&self, credential: &Credential, cfg: &Config) -> Result<MetadataPatch, ExecError> {
        self.refresher(credential, cfg).prepare(credential).await
    }

    /// [`crate::codex_refresh::Refresher::refresh_now`].
    pub async fn refresh_now(&self, credential: &Credential, cfg: &Config) -> Result<MetadataPatch, ExecError> {
        self.refresher(credential, cfg).refresh_now(credential).await
    }

    /// [`crate::codex_refresh::Refresher::refresh_all`].
    pub async fn refresh_all(&self, credential: &Credential, cfg: &Config) -> Result<MetadataPatch, ExecError> {
        self.refresher(credential, cfg).refresh_all(credential).await
    }

    pub async fn execute(
        &self,
        credential: &Credential,
        req: ExecRequest,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        if req.operation == Operation::CountTokens {
            return count_tokens(&req, cfg);
        }
        check_response_format(&req)?;
        crate::codex_siwc::require_token(credential)?;
        let view = View::for_request(credential, cfg).with_session(explicit_session(&req));
        let settings = Settings::scoped(cfg, &view);
        match (req.alt.as_deref(), req.stream) {
            (Some("responses/compact"), true) => Err(ExecError::local(
                400,
                FailureScope::Request,
                "streaming not supported for /responses/compact",
            )),
            (Some("responses/compact"), false) => self.compact(&view, &settings, req).await,
            (_, true) => self.stream_with_session(&view, &settings, req, None).await,
            (_, false) => self.buffered(&view, &settings, req).await,
        }
    }

    /// Go's `http.Client.Do` through the Codex transport: the client is picked per hop
    /// (Chrome for chatgpt.com, Go's standard transport elsewhere), redirects are
    /// followed like Go's, and gzip is undone only when the transport asked for it.
    pub(crate) async fn post(
        &self,
        view: &View<'_>,
        url: &str,
        headers: &HeaderMap,
        body: Bytes,
    ) -> Result<crate::proxy::Upstream, ExecError> {
        let transport = &self.transport;
        let (proxy, keep_alive, handshake) = (&view.proxy, view.chatgpt_keep_alive, view.handshake);
        let route = |hop: &url::Url| {
            Ok(crate::proxy::Route {
                client: transport.for_url(hop.as_str(), proxy, keep_alive, handshake),
                order: None,
            })
        };
        crate::proxy::send_routed(&route, url, go_headers(headers), body, None, None).await
    }

    /// One POST to the Codex backend, captured as Go's call sites record it: the
    /// request, a transport error, the response metadata and an error body. A non-2xx
    /// answer is the returned error; success bodies are read (and recorded) by the
    /// caller as its Go call site does.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn open(
        &self,
        view: &View<'_>,
        settings: &Settings,
        url: String,
        headers: HeaderMap,
        body: String,
        replay: &crate::codex_replay::Scope,
        wire: &Wire,
        error_body: ErrorBody,
    ) -> Result<crate::proxy::Upstream, ExecError> {
        // The shaped body names the base model, the key of per-model quota snapshots.
        let model = gjson::get(&body, "model").str().to_owned();
        wire.request(&url, &headers, body.as_bytes());
        let mut upstream = match self.post(view, &url, &headers, Bytes::from(body)).await {
            Ok(upstream) => upstream,
            Err(error) => {
                // ponytail: the text is this executor's transport message, not net/http's
                // `Post "<url>": ...`.
                wire.exec_error(&error);
                return Err(error);
            }
        };
        wire.metadata(upstream.status, &upstream.headers);
        self.quota.observe(&view.credential.id, &model, &upstream.headers);
        let status = upstream.status;
        upstream.headers.remove(http::header::CONTENT_ENCODING);
        upstream.headers.remove(http::header::CONTENT_LENGTH);
        if !(200..300).contains(&status) {
            let lossy = error_body == ErrorBody::Lossy;
            let body = match crate::proxy::read_all(upstream.body, crate::proxy::MAX_ERROR_BODY, lossy).await {
                Ok(body) => body,
                Err(error) => {
                    wire.exec_error(&error);
                    return Err(error);
                }
            };
            wire.chunk(&body);
            crate::codex_replay::clear_on_invalid_signature(&self.replay, replay, status, &body);
            return Err(response::status_error(
                status,
                &body,
                upstream.headers,
                settings.model_level_cooling,
            ));
        }
        Ok(upstream)
    }

    /// [`Self::open`], then the body: SSE framed into events, anything else read whole.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send(
        &self,
        view: &View<'_>,
        settings: &Settings,
        url: String,
        headers: HeaderMap,
        body: String,
        replay: &crate::codex_replay::Scope,
        wire: &Wire,
        error_body: ErrorBody,
    ) -> Result<ExecResponse, ExecError> {
        let upstream = self
            .open(view, settings, url, headers, body, replay, wire, error_body)
            .await?;
        let (status, headers) = (upstream.status, upstream.headers);
        let sse = headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"));
        let body = if sse {
            ResponseBody::Stream(crate::upstream::framed(upstream.body))
        } else {
            match crate::proxy::read_all(upstream.body, usize::MAX, false).await {
                Ok(body) => ResponseBody::Buffered(body),
                Err(error) => {
                    wire.exec_error(&error);
                    return Err(error);
                }
            }
        };
        Ok(ExecResponse { status, headers, body })
    }

    /// HTTP streaming. `ws_session` is the downstream WebSocket connection when an HTTP
    /// credential serves a WebSocket client (Go's execution session id).
    pub(crate) async fn stream_with_session(
        &self,
        view: &View<'_>,
        settings: &Settings,
        req: ExecRequest,
        ws_session: Option<&str>,
    ) -> Result<ExecResponse, ExecError> {
        let model = request::base_model(&req.model).to_owned();
        let request::Shaped {
            body,
            optimized: restore,
            finalize,
            ..
        } = request::shape(&req, view, settings, Call::Stream)?;
        let (body, scope) = crate::codex_replay::apply(&self.replay, &req, body).await;
        let (body, cache) = request::prompt_cache(&req, body, ws_session, false);
        let body = finalize.apply(&req, settings, request::sanitize_input_ids(body));
        report_request(&req, Format::Codex, &body);
        let headers = request::http_headers(view, settings, &req.headers, &body, &model, cache.as_deref(), true);
        let url = format!("{}/responses", view.base_url);
        let started = Instant::now();
        let wire = Wire::new(req.capture(), view.credential);
        // The response waits for the Home replay writes it caused (Go writes inline).
        let writes = scope.writes.clone();
        let sent = self
            .send(
                view,
                settings,
                url,
                headers,
                body.clone(),
                &scope,
                &wire,
                ErrorBody::Strict,
            )
            .await;
        let res = match sent {
            Ok(res) => res,
            Err(error) => {
                writes.settle().await;
                return Err(error);
            }
        };
        let upstream = events(res.body);
        let processor = Processor::new(request::is_native(&req), settings.model_level_cooling)
            .grok_keepalive(&req.headers)
            .restoring(restore)
            .replaying(self.replay.clone(), scope)
            .reporting(req.usage.clone())
            .capturing(wire);
        let stream = if settings.bootstrap_buffering {
            match response::bootstrap(upstream, processor, settings.bootstrap_timeout, started).await {
                Bootstrap::Reject(error) => {
                    writes.settle().await;
                    return Err(error);
                }
                Bootstrap::Stream(stream) => stream,
            }
        } else {
            response::processed(upstream, processor, 0)
        };
        Ok(ExecResponse {
            status: res.status,
            headers: res.headers,
            body: ResponseBody::Stream(writes.gate(client_stream(&req, &body, stream))),
        })
    }

    async fn buffered(
        &self,
        view: &View<'_>,
        settings: &Settings,
        req: ExecRequest,
    ) -> Result<ExecResponse, ExecError> {
        let model = request::base_model(&req.model).to_owned();
        let request::Shaped {
            body,
            optimized: restore,
            finalize,
            ..
        } = request::shape(&req, view, settings, Call::NonStream)?;
        let (body, scope) = crate::codex_replay::apply(&self.replay, &req, body).await;
        let (body, cache) = request::prompt_cache(&req, body, None, false);
        let body = finalize.apply(&req, settings, request::sanitize_input_ids(body));
        report_request(&req, Format::Codex, &body);
        let headers = request::http_headers(view, settings, &req.headers, &body, &model, cache.as_deref(), true);
        let url = format!("{}/responses", view.base_url);
        let wire = Wire::new(req.capture(), view.credential);
        // The response waits for the Home replay writes it caused (Go writes inline).
        let writes = scope.writes.clone();
        let result = async {
            let res = self
                .open(
                    view,
                    settings,
                    url,
                    headers,
                    body.clone(),
                    &scope,
                    &wire,
                    ErrorBody::Lossy,
                )
                .await?;
            // Go reads the whole body (`io.ReadAll`, whatever its type), records every
            // byte that arrived, then parses it, a read error's partial tail included.
            let mut upstream = res.body;
            let (mut data, mut failed) = (Vec::new(), None);
            while let Some(chunk) = upstream.next().await {
                match chunk {
                    Ok(chunk) => data.extend_from_slice(&chunk),
                    Err(error) => {
                        failed = Some(error);
                        break;
                    }
                }
            }
            wire.chunk(&data);
            let mut processor = Processor::new(false, settings.model_level_cooling)
                .restoring(restore)
                .replaying(self.replay.clone(), scope)
                .reporting(req.usage.clone());
            // `bytes.Split(data, "\n")`: every line in order, an unterminated tail included.
            if let Some(completed) = processor.buffered(&data)? {
                return Ok(ExecResponse {
                    status: res.status,
                    headers: res.headers,
                    body: ResponseBody::Buffered(non_stream_output(&req, &body, &completed, Format::Codex)?),
                });
            }
            if let Some(error) = failed {
                wire.exec_error(&error);
            }
            Err(response::request_scoped(408, response::INCOMPLETE_MESSAGE))
        }
        .await;
        writes.settle().await;
        result
    }

    async fn compact(&self, view: &View<'_>, settings: &Settings, req: ExecRequest) -> Result<ExecResponse, ExecError> {
        let model = request::base_model(&req.model).to_owned();
        let request::Shaped {
            body,
            optimized: restore,
            finalize,
            ..
        } = request::shape(&req, view, settings, Call::Compact)?;
        let (body, cache) = request::prompt_cache(&req, body, None, false);
        let body = finalize.apply(&req, settings, request::sanitize_input_ids(body));
        report_request(&req, Format::OpenAIResponse, &body);
        let headers = request::http_headers(view, settings, &req.headers, &body, &model, cache.as_deref(), false);
        let url = format!("{}/responses/compact", view.base_url);
        let wire = Wire::new(req.capture(), view.credential);
        let res = self
            .send(
                view,
                settings,
                url,
                headers,
                body.clone(),
                &Default::default(),
                &wire,
                ErrorBody::Lossy,
            )
            .await
            .map_err(compact_error)?;
        let data = match res.body {
            ResponseBody::Buffered(bytes) => bytes,
            ResponseBody::Stream(mut stream) => {
                let mut out = Vec::new();
                while let Some(event) = stream.next().await {
                    match event {
                        Ok(event) => out.extend_from_slice(&event),
                        Err(error) => {
                            wire.exec_error(&error);
                            return Err(compact_error(error));
                        }
                    }
                }
                Bytes::from(out)
            }
        };
        wire.chunk(&data);
        let text = String::from_utf8_lossy(&data);
        let text = response::restore(&text, restore);
        if req.usage.enabled() {
            // Go publishes `ParseOpenAIUsage` of the restored compaction body.
            req.usage.response_body(Format::OpenAIResponse, text.as_bytes());
        }
        Ok(ExecResponse {
            status: res.status,
            headers: res.headers,
            body: ResponseBody::Buffered(non_stream_output(&req, &body, &text, Format::OpenAIResponse)?),
        })
    }

    /// The URL an Alpha Search on `credential` posts to; `None` for an API key without
    /// a base URL.
    pub fn alpha_search_url(&self, credential: &Credential, cfg: &Config) -> Option<String> {
        self.alpha_search_url_for(&View::for_request(credential, cfg))
    }

    fn alpha_search_url_for(&self, view: &View<'_>) -> Option<String> {
        if !view.api_key {
            return Some(format!("{}/alpha/search", self.alpha_base_url));
        }
        let base = view.attr("base_url").trim();
        (!base.is_empty()).then(|| format!("{}/alpha/search", base.trim_end_matches('/')))
    }

    /// The standalone Codex Alpha Search call (`Server.codexAlphaSearch` minus selection):
    /// already in Codex search format, never translated. The upstream status and body are
    /// returned as is, with only its Content-Type.
    ///
    /// `upstream_model` is the credential-resolved model; it replaces `model` for API keys.
    /// `capture` records the attempt as Go's route does (`server_routes.go`).
    pub async fn alpha_search(
        &self,
        credential: &Credential,
        body: &[u8],
        client: &HeaderMap,
        upstream_model: &str,
        cfg: &Config,
        capture: &cpa_core::exec::CaptureSink,
    ) -> Result<ExecResponse, ExecError> {
        // Go selects with `X-Session-ID` set from the body's `id`; the request context then
        // carries that explicit session for `$CPA-SESSION-ID`.
        let mut selection_headers = client.clone();
        let id = gjson::get(&String::from_utf8_lossy(body), "id").str().trim().to_owned();
        if !id.is_empty()
            && let Ok(value) = http::HeaderValue::from_str(&id)
        {
            selection_headers.insert("x-session-id", value);
        }
        let session = cpa_common::session::cpa_session_id(Some(&cpa_common::session::extract_session_id(
            &selection_headers,
            body,
            &Default::default(),
        )));
        let view = View::for_request(credential, cfg).with_session(session);
        let mut body = sanitize_alpha_search(body);
        let Some(url) = self.alpha_search_url_for(&view) else {
            return Err(ExecError::local(
                503,
                FailureScope::Credential,
                "Codex Alpha Search API key base URL unavailable",
            ));
        };
        if view.api_key && !upstream_model.trim().is_empty() {
            body = rewrite_alpha_search_model(body, upstream_model.trim());
        }
        // `PrepareRequest` (inside `NewHttpRequest`): Sign in with ChatGPT needs its token.
        crate::codex_siwc::require_token(credential)?;
        let mut headers = HeaderMap::new();
        let mut set = |name: &'static str, value: &str| {
            if let Ok(value) = http::HeaderValue::from_str(value) {
                headers.insert(name, value);
            }
        };
        set("content-type", "application/json");
        set("accept", "application/json");
        set("originator", "codex_cli_rs");
        for name in ["version", "user-agent", "session_id", "x-client-request-id"] {
            let value = request::header(client, name).trim();
            if !value.is_empty() {
                set(name, value);
            }
        }
        if let Some(account) = credential.str("account_id").map(str::trim).filter(|s| !s.is_empty()) {
            set("chatgpt-account-id", account);
        }
        // PrepareRequest: bearer from the API key or access token, then operator headers.
        if !view.token.trim().is_empty() {
            set("authorization", &format!("Bearer {}", view.token));
        }
        for (name, value) in request::custom_headers(&view, client) {
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::try_from(name.as_str()),
                http::HeaderValue::from_str(&value),
            ) {
                headers.insert(name, value);
            }
        }
        let wire = Wire::new(capture, credential);
        wire.request(&url, &headers, &body);
        let upstream = match self.post(&view, &url, &headers, Bytes::from(body)).await {
            Ok(upstream) => upstream,
            Err(error) => {
                wire.exec_error(&error);
                return Err(error);
            }
        };
        wire.metadata(upstream.status, &upstream.headers);
        let status = upstream.status;
        let content_type = upstream.headers.get(http::header::CONTENT_TYPE).cloned();
        // `io.ReadAll(io.LimitReader(resp.Body, 32 MiB))`: bytes past the limit are never
        // read off the socket. On a read error Go records what arrived, then the error.
        let mut body = upstream.body;
        let mut data = Vec::new();
        let failed = loop {
            if data.len() >= ALPHA_SEARCH_MAX_RESPONSE {
                break None;
            }
            match body.next().await {
                Some(Ok(chunk)) => {
                    let take = chunk.len().min(ALPHA_SEARCH_MAX_RESPONSE - data.len());
                    data.extend_from_slice(&chunk[..take]);
                }
                Some(Err(error)) => break Some(error),
                None => break None,
            }
        };
        wire.chunk(&data);
        if let Some(error) = failed {
            wire.exec_error(&error);
            return Err(ExecError::local(
                502,
                FailureScope::Transport,
                "Failed to read Codex search response",
            ));
        }
        let data = Bytes::from(data);
        let mut headers = HeaderMap::new();
        if let Some(value) = content_type {
            headers.insert(http::header::CONTENT_TYPE, value);
        }
        Ok(ExecResponse {
            status,
            headers,
            body: ResponseBody::Buffered(data),
        })
    }
}

/// `CountTokens` (codex_executor_tokens.go): the request shaped as for Codex, counted
/// locally with the model's tiktoken encoding; nothing is sent upstream.
fn count_tokens(req: &ExecRequest, cfg: &Config) -> Result<ExecResponse, ExecError> {
    use crate::codex_json::{delete, set_bool_if_different, set_str, set_str_if_different};
    let model = request::base_model(&req.model);
    // Go reads is-compat only from the attempt's binding here (APIKeyModelIsCompat).
    let is_compat = req.resolved_model.as_ref().is_some_and(|r| r.is_compat());
    let client = crate::codex_client::Client::new(&req.headers, cfg, "codex", is_compat);
    let caps = req
        .resolved_model
        .as_ref()
        .map(|r| cpa_common::thinking::ModelCaps::from(&r.info));
    let ctx = cpa_translate::RequestCtx {
        model: &model,
        stream: false,
        tool_search: false,
    };
    let translated = crate::codex_client::translate_request(req.source_format, Format::Codex, &ctx, &req.body, &client)
        .map_err(|e| ExecError::local(400, FailureScope::Request, e.to_string()))?;
    // The rule baseline: the translated original, else the translated body before
    // thinking. Go counts with a failed translation's partial body; this refuses.
    let original = if req.original_body.is_empty() {
        translated.body.clone()
    } else {
        crate::codex_client::translate_request(req.source_format, Format::Codex, &ctx, &req.original_body, &client)
            .map_err(|e| ExecError::local(400, FailureScope::Request, e.to_string()))?
            .body
    };
    let body = cpa_common::thinking::apply_request_thinking(&cpa_common::thinking::RequestThinking {
        body: &translated.body,
        payload: &req.body,
        original: &req.original_body,
        model: &req.model,
        from: req.source_format.as_str(),
        to: Format::Codex.as_str(),
        provider: "codex",
        resolved: caps.as_ref().map(Some),
        has_request_transformer: cpa_translate::pair(req.source_format, Format::Codex).is_some(),
        updates_changed: translated.configuration_updates_changed,
    })
    .map_err(|e| ExecError::local(e.status(), FailureScope::Request, e.message))?;
    let mut body = String::from_utf8_lossy(&body).into_owned();
    body = set_str_if_different(body, "model", &model);
    for key in [
        "previous_response_id",
        "generate",
        "prompt_cache_retention",
        "safety_identifier",
        "stream_options",
    ] {
        body = delete(&body, key);
    }
    body = set_bool_if_different(body, "stream", false);
    if !request::is_native(req) {
        let instructions = gjson::get(&body, "instructions");
        if !instructions.exists() || instructions.kind() == gjson::Kind::Null {
            body = set_str(&body, "instructions", "");
        }
    }
    let body = crate::payload_barrier::Barrier {
        executor: "codex",
        model: &model,
        protocol: Format::Codex.as_str(),
        root: "",
        original: &original,
    }
    .apply(cfg, req, body.into_bytes());
    let encoding = crate::codex_tokens::encoding_for_model(&model);
    let count = crate::codex_tokens::count_input_tokens(encoding, &body).map_err(|e| {
        ExecError::local(
            500,
            FailureScope::Request,
            format!("codex executor: tokenizer init failed: {e}"),
        )
    })?;
    let usage =
        format!(r#"{{"response":{{"usage":{{"input_tokens":{count},"output_tokens":0,"total_tokens":{count}}}}}}}"#);
    let payload = cpa_translate::translate_token_count(req.response_format, Format::Codex, count, usage.as_bytes());
    Ok(ExecResponse {
        status: 200,
        headers: HeaderMap::new(),
        body: ResponseBody::Buffered(Bytes::from(payload)),
    })
}

/// Go reads at most 32 MiB of an Alpha Search response, whatever its status.
const ALPHA_SEARCH_MAX_RESPONSE: usize = 32 << 20;

/// Go's compact policy (`isResponsesCompactRequestFaultError`,
/// `isResponsesCompactAvailabilityNeutralError`): an upstream without compact support must
/// not cool ordinary Responses traffic. Request faults stop the request; other failures
/// except auth/payment/quota fail over without cooldown.
// ponytail: Go also exempts Cloudflare challenges and invalid_grant bodies from both rules;
// those classifiers are not ported (M4-0024).
fn compact_error(mut error: ExecError) -> ExecError {
    let credential_quota = error.status == 429 && error.scope == FailureScope::Credential;
    if credential_quota || matches!(error.status, 401 | 402 | 403 | 429) || error.scope == FailureScope::Transport {
        return error;
    }
    let body = String::from_utf8_lossy(&error.body);
    error.scope = if matches!(error.status, 400 | 404 | 405 | 409 | 413 | 422 | 501)
        || response::request_fault(error.status, &body)
    {
        FailureScope::Request
    } else {
        FailureScope::Transport
    };
    error
}

/// `sanitizeCodexAlphaSearchBody`: drop prompt-cache fields, re-marshalling only when one
/// was present (Go's map round trip sorts keys and compacts).
pub fn sanitize_alpha_search(body: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(body) else {
        return body.to_vec();
    };
    crate::codex_json::go_remarshal_object(text, |members| {
        let a = members.remove("prompt_cache_key").is_some();
        let b = members.remove("prompt_cache_retention").is_some();
        a || b
    })
    .map(String::into_bytes)
    .unwrap_or_else(|| body.to_vec())
}

/// `rewriteCodexAlphaSearchModel`.
fn rewrite_alpha_search_model(body: Vec<u8>, model: &str) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(&body) else {
        return body;
    };
    let quoted = crate::codex_json::go_quote(model, true);
    crate::codex_json::go_remarshal_object(text, |members| match members.get_mut("model") {
        Some(current) if *current != quoted => {
            *current = quoted.clone();
            true
        }
        _ => false,
    })
    .map(String::into_bytes)
    .unwrap_or(body)
}

/// The shaped headers as Go's `http.Header`: canonical keys, values in order. Go's
/// transport adds `Accept-Encoding: gzip` itself unless a header already set it.
/// How a call site reads a non-2xx body: Go's Execute and compact ignore a read error
/// (`b, _ := io.ReadAll`) and report what arrived; its stream and Images paths record the
/// read error and return it instead.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorBody {
    Lossy,
    Strict,
}

fn go_headers(headers: &HeaderMap) -> crate::proxy::GoHeaders {
    let mut out = crate::proxy::GoHeaders::new();
    for (name, value) in headers {
        out.add_raw(
            &crate::proxy::canonical_header(name.as_str()),
            String::from_utf8_lossy(value.as_bytes()),
        );
    }
    out
}

/// Go `SetTranslatedReasoningEffort`: the finalized upstream payload (prompt-cache key
/// set, input IDs sanitized), for the usage record's reasoning effort (c7b4d573).
pub(crate) fn report_request(req: &ExecRequest, upstream: Format, body: &str) {
    if req.usage.enabled() {
        req.usage.request(upstream, body.as_bytes());
    }
}

/// Go's request-context session for `$CPA-SESSION-ID` headers.
pub(crate) fn explicit_session(req: &ExecRequest) -> Option<String> {
    cpa_common::session::cpa_session_id(req.session.as_deref())
}

fn check_response_format(req: &ExecRequest) -> Result<(), ExecError> {
    let identity = matches!(req.response_format, Format::Codex | Format::OpenAIResponse);
    if identity || cpa_translate::pair(req.response_format, Format::Codex).is_some() {
        return Ok(());
    }
    Err(ExecError::local(
        501,
        FailureScope::Request,
        format!(
            "codex -> {} response translation is not registered",
            req.response_format.as_str()
        ),
    ))
}

/// Upstream body as framed SSE events, whatever its declared content type (Go scans lines).
pub(crate) fn events(body: ResponseBody) -> ExecStream {
    match body {
        ResponseBody::Stream(stream) => stream,
        ResponseBody::Buffered(bytes) => {
            let mut framer = cpa_translate::sse::Framer::default();
            let mut events = framer.push(&bytes).unwrap_or_default();
            events.extend(framer.finish());
            futures_util::stream::iter(events.into_iter().map(Ok)).boxed()
        }
    }
}

fn translate_error(error: cpa_translate::Error) -> ExecError {
    ExecError::local(502, FailureScope::Request, error.to_string())
}

/// The client body for a buffered response (`TranslateNonStream`): the registered pair,
/// else Go's identity. A translator failure (Go's apply_patch tool-input error) or empty
/// output is Go's 502 `ApplyPatchUpstreamErrorMessage`.
fn non_stream_output(
    req: &ExecRequest,
    translated: &str,
    completed: &str,
    upstream: Format,
) -> Result<Bytes, ExecError> {
    let out = match cpa_translate::pair(req.response_format, upstream) {
        Some(pair) => {
            let ctx = cpa_translate::ResponseCtx {
                model: &req.model,
                original_request: &req.original_body,
                translated_request: translated.as_bytes(),
                tool_search: false,
            };
            (pair.non_stream)(&ctx, completed.as_bytes()).unwrap_or_default()
        }
        None => completed.as_bytes().to_vec(),
    };
    if out.is_empty() {
        return Err(ExecError::local(
            502,
            FailureScope::Request,
            cpa_translate::APPLY_PATCH_UPSTREAM_ERROR,
        ));
    }
    let out = String::from_utf8_lossy(&out).into_owned();
    let out = if req.response_format == Format::OpenAIResponse {
        response::ensure_usage_details(out)
    } else {
        out
    };
    Ok(Bytes::from(out))
}

/// The client stream (`TranslateStreamWithClaudeInputTokens`): translated, with Go's
/// usage details for OpenAI Responses clients and the input-token estimate for Claude
/// clients.
fn client_stream(req: &ExecRequest, translated: &str, upstream: ExecStream) -> ExecStream {
    translate_stream(req, translated, upstream)
}

/// Streaming translation for non-Codex clients; identity for Codex clients. Go keeps
/// streaming after an apply_patch tool-input failure (Codex supports the tool natively)
/// but skips the usage and input-token touch-ups for those chunks.
fn translate_stream(req: &ExecRequest, translated: &str, upstream: ExecStream) -> ExecStream {
    let responses_client = req.response_format == Format::OpenAIResponse;
    let Some(pair) = cpa_translate::pair(req.response_format, Format::Codex) else {
        return if responses_client {
            upstream
                .map(|chunk| chunk.map(response::ensure_usage_details_chunk))
                .boxed()
        } else {
            upstream
        };
    };
    let ctx = cpa_translate::ResponseCtx {
        model: &req.model,
        original_request: &req.original_body,
        translated_request: translated.as_bytes(),
        tool_search: false,
    };
    let translator = crate::translate::stream(pair, &ctx, req);
    let original = if req.original_body.is_empty() {
        req.body.clone()
    } else {
        req.original_body.clone()
    };
    let claude =
        crate::gemini_stream::ClaudeInputTokens::new(req.source_format, Format::Codex, req.response_format, original);
    struct State {
        upstream: ExecStream,
        translator: Box<dyn cpa_translate::StreamTranslator>,
        /// Claude clients' `message_start` input-token estimate.
        claude: crate::gemini_stream::ClaudeInputTokens,
        responses_client: bool,
        ready: std::collections::VecDeque<Bytes>,
        /// The terminal error, written after the frames it flushed.
        failed: Option<ExecError>,
        done: bool,
    }
    futures_util::stream::unfold(
        State {
            upstream,
            translator,
            claude,
            responses_client,
            ready: Default::default(),
            failed: None,
            done: false,
        },
        |mut st| async move {
            loop {
                if let Some(event) = st.ready.pop_front() {
                    return Some((Ok(event), st));
                }
                if let Some(error) = st.failed.take() {
                    return Some((Err(error), st));
                }
                if st.done {
                    return None;
                }
                let result = match st.upstream.next().await {
                    Some(Ok(event)) => st.translator.event(&event).map_err(translate_error),
                    Some(Err(error)) => Err(error),
                    None => {
                        st.done = true;
                        st.translator.finish().map_err(translate_error)
                    }
                };
                match result {
                    Ok(mut events) => {
                        if !st.translator.tool_input_failed() {
                            st.claude.apply(&mut events);
                            if st.responses_client {
                                events = events.into_iter().map(response::ensure_usage_details_chunk).collect();
                            }
                        }
                        st.ready.extend(events);
                    }
                    Err(error) => {
                        // Go's responsesSSEFramer flushes the pending client frame before a
                        // terminal error is written.
                        st.ready.extend(st.translator.flush_frames());
                        st.failed = Some(error);
                        st.done = true;
                    }
                }
            }
        },
    )
    .boxed()
}

#[cfg(test)]
#[path = "codex_tests.rs"]
mod tests;
